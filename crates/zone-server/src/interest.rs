//! Interest management. The zone is a grid of 32 m cells; each player sees
//! the 3×3 cells around it, capped at the most relevant entities, with
//! update rates tiered by distance. Snapshots are delta-encoded per client
//! against the last snapshot that client acknowledged.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use combat_core::codec::{self, EntState, SnapEntity, flags, quantize_facing, quantize_pos};
use combat_core::{EntityId, EntityKind, Ms, channel};
use rayon::prelude::*;
use renet::ClientId;

use crate::zone::{Entity, Outgoing, Zone};

pub const CELL: f32 = 32.0;
/// Most entities one client is told about.
pub const VIEW_CAP: usize = 80;
/// Full 20 Hz inside this radius.
pub const NEAR: f32 = 40.0;
/// 10 Hz inside this radius, 4 Hz beyond it.
pub const MID: f32 = 80.0;
/// Snapshots kept per client as possible delta baselines (800 ms at 20 Hz).
const HISTORY: usize = 16;

pub struct Grid {
    nx: usize,
    nz: usize,
    half: [f32; 2],
    pub cells: Vec<Vec<EntityId>>,
}

impl Grid {
    pub fn new(half: [f32; 2]) -> Self {
        let nx = ((half[0] * 2.0) / CELL).ceil().max(1.0) as usize;
        let nz = ((half[1] * 2.0) / CELL).ceil().max(1.0) as usize;
        Self {
            nx,
            nz,
            half,
            cells: vec![Vec::new(); nx * nz],
        }
    }

    pub fn cell_of(&self, p: [f32; 3]) -> usize {
        let cx = (((p[0] + self.half[0]) / CELL) as isize).clamp(0, self.nx as isize - 1) as usize;
        let cz = (((p[2] + self.half[1]) / CELL) as isize).clamp(0, self.nz as isize - 1) as usize;
        cz * self.nx + cx
    }

    pub fn insert(&mut self, id: EntityId, cell: usize) {
        self.cells[cell].push(id);
    }

    pub fn remove(&mut self, id: EntityId, cell: usize) {
        if let Some(i) = self.cells[cell].iter().position(|e| *e == id) {
            self.cells[cell].swap_remove(i);
        }
    }

    /// The 3×3 block of cells around `cell` (fewer at the zone edge).
    pub fn neighbors(&self, cell: usize) -> impl Iterator<Item = usize> + '_ {
        let (cx, cz) = ((cell % self.nx) as isize, (cell / self.nx) as isize);
        (-1..=1).flat_map(move |dz| {
            (-1..=1).filter_map(move |dx| {
                let (x, z) = (cx + dx, cz + dz);
                (x >= 0 && z >= 0 && x < self.nx as isize && z < self.nz as isize)
                    .then(|| z as usize * self.nx + x as usize)
            })
        })
    }
}

/// Per-client snapshot bookkeeping.
pub struct ClientNet {
    pub client: ClientId,
    pub entity: EntityId,
    /// Newest snapshot id the client says it received.
    pub acked: u32,
    next_snap: u32,
    history: VecDeque<(u32, Vec<(EntityId, EntState)>)>,
    /// Entities in the last snapshot sent, for metrics.
    pub last_view: usize,
}

impl ClientNet {
    pub fn new(client: ClientId, entity: EntityId) -> Self {
        Self {
            client,
            entity,
            acked: 0,
            next_snap: 1,
            history: VecDeque::with_capacity(HISTORY),
            last_view: 0,
        }
    }
}

/// One entity as every client sees it this tick.
struct TickEnt<'a> {
    id: EntityId,
    pos: [f32; 3],
    state: EntState,
    name: &'a str,
}

pub fn ent_state(e: &Entity, now: Ms) -> EntState {
    let mut f = 0u8;
    if e.dead {
        f |= flags::DEAD;
    }
    if e.in_combat(now) {
        f |= flags::IN_COMBAT;
    }
    if e.evading() {
        f |= flags::EVADING;
    }
    if e.moving {
        f |= flags::MOVING;
    }
    if e.passive {
        f |= flags::PASSIVE;
    }
    let mut s = EntState {
        kind: e.kind,
        def: e.def,
        level: e.level.min(255) as u8,
        pos: quantize_pos(e.pos),
        facing: quantize_facing(e.facing),
        hp: e.hp.max(0.0).round() as u32,
        max_hp: e.stats.max_hp.round() as u32,
        mana: if e.kind == EntityKind::Player {
            e.caster.mana.round() as u32
        } else {
            0
        },
        max_mana: if e.kind == EntityKind::Player {
            e.caster.max_mana.round() as u32
        } else {
            0
        },
        flags: f,
        cast: e
            .caster
            .cast
            .as_ref()
            .map(|c| (c.ability, c.started_at as u32)),
        ..Default::default()
    };
    s.set_auras(e.auras.iter().map(|a| (a.aura, a.expires_at as u32)));
    s
}

impl Zone {
    /// Build and queue one snapshot per connected client.
    pub fn snapshot(&mut self, now: Ms) {
        let tick = self.tick;
        // One flat array of every entity's state, bucketed by cell.
        let mut ents: Vec<TickEnt> = Vec::with_capacity(self.entities.len());
        let mut by_cell: Vec<Vec<u32>> = vec![Vec::new(); self.grid.cells.len()];
        let mut index: HashMap<EntityId, u32> = HashMap::with_capacity(self.entities.len());
        // Attackers of each player: mobs with that player on their threat list.
        let mut attackers: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
        for e in self.entities.values() {
            let i = ents.len() as u32;
            ents.push(TickEnt {
                id: e.id,
                pos: e.pos,
                state: ent_state(e, now),
                name: &e.name,
            });
            by_cell[e.cell].push(i);
            index.insert(e.id, i);
            if let Some(m) = &e.mob {
                for (t, _) in &m.threat {
                    attackers.entry(*t).or_default().push(e.id);
                }
            }
        }

        let grid = &self.grid;
        let entities = &self.entities;
        let mut nets: Vec<&mut ClientNet> = self.nets.values_mut().collect();
        let out: Vec<Outgoing> = nets
            .par_iter_mut()
            .filter_map(|net| {
                let me = entities.get(&net.entity)?;
                let target = me.player.as_ref().map_or(0, |p| p.target);
                let mut forced: Vec<EntityId> = vec![net.entity];
                if target != 0 {
                    forced.push(target);
                }
                if let Some(a) = attackers.get(&net.entity) {
                    forced.extend(a.iter().copied());
                }

                // Candidates in the 3×3 cells, nearest first, capped.
                let mut cand: Vec<(f32, u32)> = Vec::with_capacity(VIEW_CAP * 2);
                for c in grid.neighbors(me.cell) {
                    for &i in &by_cell[c] {
                        let te = &ents[i as usize];
                        let (dx, dz) = (te.pos[0] - me.pos[0], te.pos[2] - me.pos[2]);
                        cand.push((dx * dx + dz * dz, i));
                    }
                }
                if cand.len() > VIEW_CAP {
                    cand.select_nth_unstable_by(VIEW_CAP - 1, |a, b| a.0.total_cmp(&b.0));
                    cand.truncate(VIEW_CAP);
                }
                for f in &forced {
                    if let Some(&i) = index.get(f)
                        && !cand.iter().any(|c| c.1 == i)
                    {
                        let te = &ents[i as usize];
                        let (dx, dz) = (te.pos[0] - me.pos[0], te.pos[2] - me.pos[2]);
                        cand.push((dx * dx + dz * dz, i));
                    }
                }
                let phase = (net.client as u64).wrapping_mul(0x9E37) ^ tick;
                let mut view: Vec<SnapEntity> = cand
                    .iter()
                    .map(|&(d2, i)| {
                        let te = &ents[i as usize];
                        let d = d2.sqrt();
                        let due = forced.contains(&te.id)
                            || d < NEAR
                            || (d < MID && phase % 2 == 0)
                            || phase % 5 == 0;
                        SnapEntity {
                            id: te.id,
                            state: te.state,
                            due,
                            name: te.name,
                        }
                    })
                    .collect();
                view.sort_unstable_by_key(|e| e.id);

                let baseline = net
                    .history
                    .iter()
                    .find(|(id, _)| *id == net.acked)
                    .map(|(id, s)| (*id, s.as_slice()));
                let snap_id = net.next_snap;
                net.next_snap += 1;
                let (bytes, known) = codec::encode_snapshot(snap_id, now as u32, baseline, &view);
                if net.history.len() >= HISTORY {
                    net.history.pop_front();
                }
                net.history.push_back((snap_id, known));
                net.last_view = view.len();
                Some(Outgoing {
                    client: net.client,
                    channel: channel::STATE,
                    bytes: Bytes::from(bytes),
                })
            })
            .collect();
        self.outbox.extend(out);
    }
}

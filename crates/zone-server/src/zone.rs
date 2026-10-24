//! Authoritative zone state: players, mobs and combat. Combat outcomes are
//! decided only here. Mob AI lives in `ai.rs`, interest management and
//! snapshots in `interest.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use combat_core::codec::MoveInput;
use combat_core::content::{Stats, TargetKind};
use combat_core::movement::{MoveValidator, MoveVerdict, ZoneGeometry, dist_xz};
use combat_core::protocol::{self, ClientMsg, ServerMsg, StopReason};
use combat_core::{
    AbilityId, AuraId, CastCtx, CasterState, ContentPack, Effect, EntityId, EntityKind,
    FailReason, Ms, Started, channel,
};
use renet::ClientId;

use crate::game::Inventory;
use crate::interest::{ClientNet, Grid};
use crate::svc::SvcReq;
use services::Party;

/// Seconds of no damage dealt or taken before an entity leaves combat.
pub const COMBAT_TIMEOUT_MS: Ms = 5000;

pub struct Outgoing {
    pub client: ClientId,
    pub channel: u8,
    pub bytes: Bytes,
}

pub struct Queued {
    pub seq: u32,
    pub ability: AbilityId,
    pub target: EntityId,
    pub move_seq: u32,
    pub received_at: Ms,
}

pub struct PlayerCtl {
    pub client_id: ClientId,
    pub validator: MoveValidator,
    pub last_move_seq: u32,
    /// Moves up to this seq were sent before the current cast was pressed.
    pub cast_move_seq: u32,
    pub rtt_ms: Ms,
    /// What the client has targeted (0 = nothing), kept in its view.
    pub target: EntityId,
    pub class: u16,
    /// Database ids; 0 for development logins, which are never saved.
    pub character: i64,
    pub account: i64,
    pub xp: u32,
    pub inv: Inventory,
    /// (item, ready at) for items with a use cooldown.
    pub item_cd: Vec<(u16, Ms)>,
    pub party: Option<Party>,
    pub handoff: Handoff,
    pub last_chat: Ms,
    /// Progress changed since the last save.
    pub dirty: bool,
    /// Standing in a portal: it fires once on entry, not every step.
    pub in_portal: bool,
}

/// Leaving this zone server for another (portal or layer merge).
#[derive(Debug, Clone, PartialEq)]
pub enum Handoff {
    None,
    /// Waiting for the world service to place us. Inputs are ignored.
    Pending { zone: String, pos: [f32; 3] },
    /// The client has its new address; it disconnects itself, or is
    /// kicked at `kick_at`.
    Sent { zone: String, pos: [f32; 3], kick_at: Ms },
}

/// Everything needed to put a player in the world.
pub struct PlayerInit {
    pub name: String,
    pub class: u16,
    pub level: u32,
    pub pos: Option<[f32; 3]>,
    pub character: i64,
    pub account: i64,
    pub xp: u32,
    pub hp: Option<f32>,
    pub mana: Option<f32>,
    pub inv: Option<Inventory>,
    pub party: Option<Party>,
}

impl PlayerInit {
    pub fn dev(name: String, class: u16, level: u32, pos: Option<[f32; 3]>) -> Self {
        Self {
            name,
            class,
            level,
            pos,
            character: 0,
            account: 0,
            xp: 0,
            hp: None,
            mana: None,
            inv: None,
            party: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobState {
    Idle,
    Combat,
    /// Walking home after losing its target; immune, heals on arrival.
    Evade,
}

pub struct MobCtl {
    pub home: [f32; 3],
    pub state: MobState,
    /// (entity, threat). Highest threat is the target.
    pub threat: Vec<(EntityId, f32)>,
    pub next_attack: Ms,
    pub respawn_ms: Ms,
    pub respawn_at: Ms,
    pub wander_to: Option<[f32; 3]>,
    pub next_wander: Ms,
    pub last_hit: Ms,
    pub cast_seq: u32,
}

impl MobCtl {
    pub fn top_threat(&self) -> Option<EntityId> {
        self.threat
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|t| t.0)
    }
}

pub struct AuraInstance {
    pub aura: AuraId,
    pub source: EntityId,
    pub spell_power: f32,
    pub expires_at: Ms,
    pub next_tick: Ms,
}

pub struct Entity {
    pub id: EntityId,
    pub kind: EntityKind,
    /// Class index for players, mob index for mobs.
    pub def: u16,
    pub name: String,
    pub level: u32,
    pub pos: [f32; 3],
    pub facing: f32,
    pub moving: bool,
    pub hp: f32,
    pub stats: Stats,
    pub caster: CasterState,
    pub queued: Option<Queued>,
    pub auras: Vec<AuraInstance>,
    pub dead: bool,
    pub combat_until: Ms,
    pub last_regen: Ms,
    pub cell: usize,
    pub player: Option<Box<PlayerCtl>>,
    pub mob: Option<Box<MobCtl>>,
    /// Training dummies: never die, never fight back.
    pub passive: bool,
}

impl Entity {
    pub fn is_player(&self) -> bool {
        self.player.is_some()
    }
    pub fn evading(&self) -> bool {
        self.mob.as_ref().is_some_and(|m| m.state == MobState::Evade)
    }
    pub fn in_combat(&self, now: Ms) -> bool {
        self.combat_until > now
    }
    pub fn client(&self) -> Option<ClientId> {
        self.player.as_ref().map(|p| p.client_id)
    }
}

#[derive(Default)]
pub struct QueueStats {
    pub count: u32,
    pub total_us: u64,
    pub max_us: u64,
}

pub struct Zone {
    pub pack: Arc<ContentPack>,
    pub zone_idx: u16,
    pub layer: u16,
    pub geo: ZoneGeometry,
    pub entities: HashMap<EntityId, Entity>,
    pub clients: HashMap<ClientId, EntityId>,
    pub nets: HashMap<ClientId, ClientNet>,
    pub grid: Grid,
    next_entity: EntityId,
    pub outbox: Vec<Outgoing>,
    rng: u64,
    pub stats: QueueStats,
    pub verbose: bool,
    scratch: Vec<EntityId>,
    pub tick: u64,
    pub corrections: u64,
    /// Clients to disconnect (failed auth, duplicate login, handoff done).
    pub kicks: Vec<ClientId>,
    /// Requests for the services thread (saves, placement, chat, party).
    pub svc: Vec<SvcReq>,
    /// Entity of each logged-in character.
    pub by_character: HashMap<i64, EntityId>,
    /// This layer is merging away: hand everyone off and close.
    pub draining: bool,
}

impl Zone {
    pub fn new(pack: Arc<ContentPack>, zone_idx: u16, layer: u16, now: Ms) -> Self {
        let zdef = &pack.zones[zone_idx as usize];
        let geo = ZoneGeometry::new(zdef);
        let grid = Grid::new(zdef.half_extent);
        let mut z = Self {
            pack: pack.clone(),
            zone_idx,
            layer,
            geo,
            entities: HashMap::new(),
            clients: HashMap::new(),
            nets: HashMap::new(),
            grid,
            next_entity: 1,
            outbox: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15 ^ ((zone_idx as u64) << 32 | layer as u64),
            stats: QueueStats::default(),
            verbose: std::env::var_os("ZONE_DEBUG").is_some(),
            scratch: Vec::new(),
            tick: 0,
            corrections: 0,
            kicks: Vec::new(),
            svc: Vec::new(),
            by_character: HashMap::new(),
            draining: false,
        };
        z.spawn_mobs(now);
        z
    }

    pub fn zone_def(&self) -> &combat_core::content::ZoneDef {
        &self.pack.zones[self.zone_idx as usize]
    }

    fn alloc_id(&mut self) -> EntityId {
        let id = self.next_entity;
        self.next_entity += 1;
        id
    }

    fn spawn_mobs(&mut self, now: Ms) {
        let spawns = self.zone_def().spawns.clone();
        for s in spawns {
            let mob = self.pack.mob_index(&s.mob).expect("validated");
            for i in 0..s.count {
                let mut pos = s.pos;
                if s.count > 1 {
                    let a = i as f32 / s.count as f32 * std::f32::consts::TAU;
                    let r = s.radius * (0.4 + 0.6 * self.rand01());
                    pos = [s.pos[0] + a.cos() * r, s.pos[1], s.pos[2] + a.sin() * r];
                    if self.geo.inside_blocker(pos) || !self.geo.in_bounds(pos) {
                        pos = s.pos;
                    }
                }
                self.add_mob(mob, pos, s.respawn_ms as Ms, now);
            }
        }
    }

    fn add_mob(&mut self, mob: u16, pos: [f32; 3], respawn_ms: Ms, now: Ms) -> EntityId {
        let id = self.alloc_id();
        let pack = self.pack.clone();
        let def = &pack.mobs[mob as usize];
        let stats = Stats {
            max_hp: def.max_hp,
            max_mana: def.mana,
            spell_power: def.spell_power,
            crit_chance: 0.05,
        };
        let cell = self.grid.cell_of(pos);
        self.grid.insert(id, cell);
        let e = Entity {
            id,
            kind: EntityKind::Mob,
            def: mob,
            name: String::new(),
            level: def.level,
            pos,
            facing: self.rand01() * std::f32::consts::TAU,
            moving: false,
            hp: def.max_hp,
            stats,
            caster: CasterState::new(self.pack.abilities.len(), def.mana),
            queued: None,
            auras: Vec::new(),
            dead: false,
            combat_until: 0,
            last_regen: now,
            cell,
            player: None,
            mob: Some(Box::new(MobCtl {
                home: pos,
                state: MobState::Idle,
                threat: Vec::new(),
                next_attack: 0,
                respawn_ms,
                respawn_at: 0,
                wander_to: None,
                next_wander: now + (self.rand01() * 8000.0) as Ms,
                last_hit: 0,
                cast_seq: 0,
            })),
            passive: def.dummy,
        };
        self.entities.insert(id, e);
        id
    }

    /// A development player joins at `pos` (or the zone spawn).
    pub fn add_player(
        &mut self,
        client_id: ClientId,
        name: String,
        class: u16,
        level: u32,
        pos: Option<[f32; 3]>,
        now: Ms,
    ) -> EntityId {
        self.add_player_with(client_id, PlayerInit::dev(name, class, level, pos), now)
    }

    /// A player joins. Returns its entity id.
    pub fn add_player_with(&mut self, client_id: ClientId, init: PlayerInit, now: Ms) -> EntityId {
        let id = self.alloc_id();
        let mut spawn = init.pos.unwrap_or(self.zone_def().player_spawn);
        if !self.geo.in_bounds(spawn) || self.geo.inside_blocker(spawn) {
            spawn = self.zone_def().player_spawn;
        }
        let dead_on_save = init.hp.is_some_and(|h| h <= 0.0);
        if dead_on_save {
            spawn = self.zone_def().graveyard;
        }
        // Spread arrivals a little so a crowd does not stack on one point.
        if init.pos.is_none() {
            let jitter = (id % 9) as f32 - 4.0;
            spawn = self.geo.slide(spawn, [jitter * 0.8, 0.0]);
        }
        let mut validator = MoveValidator::default();
        validator.reset(now, spawn);
        let class = init.class;
        let level = init.level.clamp(1, self.pack.level_cap());
        let stats = self.pack.classes[class as usize].stats_at(level);
        let cell = self.grid.cell_of(spawn);
        self.grid.insert(id, cell);
        let name = if init.name.is_empty() {
            format!("Player{id}")
        } else {
            init.name
        };
        let bag = self.pack.tenant.bag_slots as usize;
        self.entities.insert(
            id,
            Entity {
                id,
                kind: EntityKind::Player,
                def: class,
                name: name.clone(),
                level,
                pos: spawn,
                facing: 0.0,
                moving: false,
                hp: stats.max_hp,
                stats,
                caster: CasterState::new(self.pack.abilities.len(), stats.max_mana),
                queued: None,
                auras: Vec::new(),
                dead: false,
                combat_until: 0,
                last_regen: now,
                cell,
                player: Some(Box::new(PlayerCtl {
                    client_id,
                    validator,
                    last_move_seq: 0,
                    cast_move_seq: 0,
                    rtt_ms: 0,
                    target: 0,
                    class,
                    character: init.character,
                    account: init.account,
                    xp: init.xp,
                    inv: init.inv.unwrap_or_else(|| Inventory::new(bag)),
                    item_cd: Vec::new(),
                    party: init.party,
                    handoff: Handoff::None,
                    last_chat: 0,
                    dirty: false,
                    in_portal: false,
                })),
                mob: None,
                passive: false,
            },
        );
        self.recalc_stats(id);
        {
            let e = self.entities.get_mut(&id).unwrap();
            let max_hp = e.stats.max_hp;
            e.hp = if dead_on_save {
                max_hp * 0.5
            } else {
                init.hp.map_or(max_hp, |h| h.min(max_hp))
            };
            if let Some(m) = init.mana {
                e.caster.mana = m.min(e.caster.max_mana);
            }
        }
        self.clients.insert(client_id, id);
        self.nets.insert(client_id, ClientNet::new(client_id, id));
        if init.character != 0 {
            self.by_character.insert(init.character, id);
        }
        if self.verbose {
            println!("client {client_id} joined as entity {id} ({name})");
        }
        let hash = self.pack.hash;
        self.send(
            client_id,
            ServerMsg::Welcome {
                you: id,
                content_hash: hash,
                server_time: now,
                zone: self.zone_idx,
                layer: self.layer,
                class,
                name,
            },
        );
        self.send(client_id, ServerMsg::Correction { pos: spawn });
        self.send_xp(id, 0);
        self.send_inventory(id);
        if self.entities[&id].player.as_ref().unwrap().party.is_some() {
            self.send_party(id);
        }
        id
    }

    pub fn remove_player(&mut self, client_id: ClientId) -> Option<Entity> {
        let id = self.clients.remove(&client_id)?;
        self.nets.remove(&client_id);
        let e = self.entities.remove(&id)?;
        self.grid.remove(id, e.cell);
        if let Some(p) = e.player.as_ref() {
            self.by_character.remove(&p.character);
        }
        Some(e)
    }

    pub fn player_count(&self) -> usize {
        self.clients.len()
    }

    pub fn send(&mut self, client: ClientId, msg: ServerMsg) {
        if self.verbose {
            println!("  -> {client}: {msg:?}");
        }
        self.outbox.push(Outgoing {
            client,
            channel: channel::COMBAT,
            bytes: Bytes::from(protocol::encode(&msg)),
        });
    }

    /// Send to the player behind `entity`, if it is a player.
    fn send_entity(&mut self, entity: EntityId, msg: ServerMsg) {
        if let Some(c) = self.entities.get(&entity).and_then(|e| e.client()) {
            self.send(c, msg);
        }
    }

    pub fn relocate(&mut self, id: EntityId) {
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let cell = self.grid.cell_of(e.pos);
        if cell != e.cell {
            let old = e.cell;
            e.cell = cell;
            self.grid.remove(id, old);
            self.grid.insert(id, cell);
        }
    }

    pub fn hostile(&self, a: &Entity, b: &Entity) -> bool {
        a.kind != b.kind
    }

    /// Distance and line of sight from `caster` to `target` if it is a
    /// legal target for `def`.
    fn target_info(
        &self,
        caster: &Entity,
        target: EntityId,
        kind: TargetKind,
    ) -> (Option<f32>, bool) {
        if kind == TargetKind::Caster {
            return (Some(0.0), true);
        }
        match self.entities.get(&target) {
            Some(t) if !t.dead && self.hostile(caster, t) && !t.evading() => (
                Some(dist_xz(caster.pos, t.pos)),
                self.geo.los(caster.pos, t.pos),
            ),
            _ => (None, true),
        }
    }

    pub fn handle_move(&mut self, client_id: ClientId, m: MoveInput, now: Ms) {
        let Some(&id) = self.clients.get(&client_id) else {
            return;
        };
        if let Some(net) = self.nets.get_mut(&client_id) {
            net.acked = net.acked.max(m.ack);
        }
        let geo = &self.geo;
        let e = self.entities.get_mut(&id).unwrap();
        let speed = self.pack.classes[e.def as usize].move_speed;
        let p = e.player.as_mut().unwrap();
        if p.handoff != Handoff::None {
            return;
        }
        if m.seq <= p.last_move_seq {
            return; // stale or duplicate
        }
        p.last_move_seq = m.seq;
        p.target = m.target;
        if e.dead {
            return;
        }
        let verdict = p.validator.check(geo, now, m.pos, speed);
        if verdict != MoveVerdict::Ok {
            self.corrections += 1;
            let back = e.pos;
            p.validator.reset(now, back);
            if p.validator.strikes % 20 == 1 {
                println!(
                    "entity {id} ({}) failed movement check ({verdict:?}), strikes {}",
                    e.name, p.validator.strikes
                );
            }
            self.send(client_id, ServerMsg::Correction { pos: back });
            return;
        }
        e.pos = m.pos;
        e.facing = m.facing;
        e.moving = m.moving;
        let cast_move_seq = p.cast_move_seq;
        // A move sent after the cast was pressed breaks it, unless the
        // ability allows casting on the move.
        let breaks = m.moving
            && m.seq > cast_move_seq
            && e
                .caster
                .cast
                .as_ref()
                .is_some_and(|c| !self.pack.abilities[c.ability as usize].cast_while_moving);
        self.relocate(id);
        if breaks {
            self.stop_cast(id, StopReason::Interrupted);
        }
        self.check_portals(id, now);
    }

    /// Stepping into a portal starts a handoff to its zone.
    fn check_portals(&mut self, id: EntityId, now: Ms) {
        let e = &self.entities[&id];
        if e.dead {
            return;
        }
        let portal = self
            .zone_def()
            .portals
            .iter()
            .find(|p| dist_xz(p.pos, e.pos) <= p.radius)
            .cloned();
        let e = self.entities.get_mut(&id).unwrap();
        let p = e.player.as_mut().unwrap();
        let entered = portal.is_some() && !p.in_portal;
        p.in_portal = portal.is_some();
        let (Some(portal), true) = (portal, entered) else {
            return;
        };
        let e = &self.entities[&id];
        let c = e.client().unwrap();
        if e.player.as_ref().unwrap().character == 0 {
            self.system(c, "Portals need the login service (this is a dev login).");
            return;
        }
        if e.in_combat(now) {
            self.system(c, "You can't travel while in combat. Step out and back in.");
            return;
        }
        self.begin_handoff(id, portal.to_zone.clone(), portal.to_pos, None);
    }

    /// Save the character at its destination, then ask the world service
    /// where to connect. `avoid_layer` is set when this layer is merging.
    pub fn begin_handoff(&mut self, id: EntityId, zone: String, pos: [f32; 3], avoid_layer: Option<u16>) {
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        let p = e.player.as_mut().unwrap();
        if p.handoff != Handoff::None || p.character == 0 {
            return;
        }
        p.handoff = Handoff::Pending { zone: zone.clone(), pos };
        let (client, character, account) = (p.client_id, p.character, p.account);
        e.moving = false;
        e.queued = None;
        e.caster.cancel();
        // The save goes first on the same queue, so the destination zone
        // always loads the character where the portal put it.
        self.save_progress(id);
        self.svc.push(SvcReq::Place {
            client,
            character,
            account,
            zone,
            avoid_layer,
        });
    }

    /// The world service answered a handoff.
    pub fn on_placed(&mut self, client: ClientId, result: Result<services::PlaceResponse, String>, now: Ms) {
        let Some(&id) = self.clients.get(&client) else {
            return;
        };
        let e = self.entities.get_mut(&id).unwrap();
        let p = e.player.as_mut().unwrap();
        let Handoff::Pending { zone, pos } = p.handoff.clone() else {
            return;
        };
        match result.and_then(|r| {
            use base64::Engine;
            let token = base64::engine::general_purpose::STANDARD
                .decode(&r.token)
                .map_err(|e| e.to_string())?;
            Ok((r, token))
        }) {
            Ok((r, token)) => {
                p.handoff = Handoff::Sent {
                    zone,
                    pos,
                    kick_at: now + 5000,
                };
                let zone_idx = self.pack.zone_index(&r.zone).unwrap_or(0);
                self.send(
                    client,
                    ServerMsg::Handoff {
                        addr: r.addr,
                        token,
                        zone: zone_idx,
                        layer: r.layer,
                    },
                );
            }
            Err(err) => {
                // Stay here; save the real position again.
                p.handoff = Handoff::None;
                p.dirty = true;
                let back = e.pos;
                p.validator.reset(now, back);
                self.system(client, format!("Travel failed: {err}"));
                self.save_progress(id);
            }
        }
    }

    pub fn handle(&mut self, client_id: ClientId, msg: ClientMsg, now: Ms, now_us: u64, rtt_ms: Ms) {
        let Some(&id) = self.clients.get(&client_id) else {
            return;
        };
        let e = self.entities.get_mut(&id).unwrap();
        let p = e.player.as_mut().unwrap();
        p.rtt_ms = rtt_ms;
        if p.handoff != Handoff::None {
            return;
        }
        match msg {
            ClientMsg::UseItem { .. } | ClientMsg::Unequip { .. } | ClientMsg::Destroy { .. } => {
                self.handle_item(id, &msg, now);
            }
            ClientMsg::Chat { .. }
            | ClientMsg::PartyInvite { .. }
            | ClientMsg::PartyAccept
            | ClientMsg::PartyLeave => self.handle_social(id, msg, now),
            ClientMsg::Cast {
                seq,
                ability,
                target,
                queued: _,
                move_seq,
            } => {
                let Some(def) = self.pack.ability(ability) else {
                    self.send(
                        client_id,
                        ServerMsg::CastFailed {
                            seq,
                            reason: FailReason::UnknownAbility,
                        },
                    );
                    return;
                };
                let class = e.player.as_ref().unwrap().class;
                if !self.pack.classes[class as usize]
                    .abilities
                    .iter()
                    .any(|a| *a == def.id)
                {
                    self.send(
                        client_id,
                        ServerMsg::CastFailed {
                            seq,
                            reason: FailReason::UnknownAbility,
                        },
                    );
                    return;
                }
                let q = Queued {
                    seq,
                    ability,
                    target,
                    move_seq,
                    received_at: now,
                };
                // The newest press replaces anything already queued, as in WoW.
                let ready = e.caster.ready_at(ability, def);
                if ready > now && !e.dead {
                    if e.caster.in_queue_window(ability, def, now) {
                        if let Some(old) = e.queued.replace(q) {
                            self.send(
                                client_id,
                                ServerMsg::CastFailed {
                                    seq: old.seq,
                                    reason: FailReason::NotReady,
                                },
                            );
                        }
                    } else {
                        let reason = e
                            .caster
                            .check(ability, def, now, CastCtx {
                                distance: None,
                                moving: false,
                                target_moving: false,
                                los: true,
                                dead: false,
                            })
                            .unwrap_err();
                        self.send(client_id, ServerMsg::CastFailed { seq, reason });
                    }
                    return;
                }
                self.try_start(id, q, now, None, now_us);
            }
            ClientMsg::CancelCast => {
                e.queued = None;
                if e.caster.cast.is_some() {
                    self.stop_cast(id, StopReason::Cancelled);
                }
            }
            ClientMsg::Release => {
                if e.dead {
                    let gy = self.zone_def().graveyard;
                    self.revive(id, gy, 0.5, now);
                }
            }
        }
    }

    pub fn revive(&mut self, id: EntityId, pos: [f32; 3], hp_frac: f32, now: Ms) {
        let Some(e) = self.entities.get_mut(&id) else {
            return;
        };
        e.dead = false;
        e.hp = e.stats.max_hp * hp_frac;
        e.caster.mana = e.caster.max_mana * hp_frac;
        e.pos = pos;
        e.combat_until = 0;
        if let Some(p) = e.player.as_mut() {
            p.validator.reset(now, pos);
        }
        let client = e.client();
        self.relocate(id);
        if let Some(c) = client {
            self.send(c, ServerMsg::Correction { pos });
        }
    }

    pub fn stop_cast(&mut self, id: EntityId, reason: StopReason) {
        let e = self.entities.get_mut(&id).unwrap();
        if let Some(cast) = e.caster.cancel() {
            e.queued = None;
            self.send_entity(
                id,
                ServerMsg::CastStop {
                    caster: id,
                    seq: cast.seq,
                    reason,
                },
            );
        }
    }

    /// Validate and start a cast at logical time `t`. `queued_ready` is the
    /// moment the caster became ready, for casts fired from the queue.
    pub fn try_start(
        &mut self,
        id: EntityId,
        q: Queued,
        t: Ms,
        queued_ready: Option<Ms>,
        now_us: u64,
    ) -> bool {
        let pack = self.pack.clone();
        let def = &pack.abilities[q.ability as usize];
        let (distance, los) = {
            let e = &self.entities[&id];
            self.target_info(e, q.target, def.target)
        };
        let e = self.entities.get_mut(&id).unwrap();
        let ctx = CastCtx {
            distance,
            // Movement legality is checked on the client and enforced here by
            // interrupting on newer moves; the request itself is not refused
            // for it, because the stop-moving packet may still be in flight.
            moving: false,
            target_moving: e.moving,
            los,
            dead: e.dead,
        };
        if let Err(reason) = e.caster.check(q.ability, def, t, ctx) {
            if e.is_player() {
                self.send_entity(id, ServerMsg::CastFailed { seq: q.seq, reason });
            }
            return false;
        }
        let target = if def.target == TargetKind::Caster {
            id
        } else {
            q.target
        };
        let allowance = e.player.as_ref().map_or(0, |p| p.rtt_ms / 2);
        let started = e.caster.start(q.ability, def, q.seq, target, t, allowance);
        if let Some(p) = e.player.as_mut() {
            p.cast_move_seq = p.cast_move_seq.max(q.move_seq);
        }
        let my_pos = e.pos;
        let tpos = self.entities.get(&target).map_or(my_pos, |t| t.pos);
        let e = self.entities.get_mut(&id).unwrap();
        e.facing = face(my_pos, tpos, e.facing);
        let e = &self.entities[&id];
        let is_player = e.is_player();
        let queue_gap_us = queued_ready.map(|ready| now_us.saturating_sub(ready * 1000) as u32);
        if let Some(gap) = queue_gap_us {
            self.stats.count += 1;
            self.stats.total_us += gap as u64;
            self.stats.max_us = self.stats.max_us.max(gap as u64);
            if self.verbose {
                println!(
                    "[{t:>8} ms] entity {id} {:<15} seq {:<4} from queue, started {:.2} ms after caster was ready",
                    def.name,
                    q.seq,
                    gap as f64 / 1000.0
                );
            }
        }
        if is_player {
            self.send_entity(
                id,
                ServerMsg::CastStart {
                    caster: id,
                    seq: q.seq,
                    ability: q.ability,
                    target,
                    bar_ms: def.bar_ms() as u32,
                    queue_gap_us,
                },
            );
        }
        if started == Started::Instant {
            self.resolve(id, q.ability, target, t);
        }
        true
    }

    /// Apply an ability's effects at logical time `t`.
    pub fn resolve(&mut self, caster: EntityId, ability: AbilityId, target: EntityId, t: Ms) {
        let pack = self.pack.clone();
        let def = &pack.abilities[ability as usize];
        let Some(c) = self.entities.get(&caster) else {
            return;
        };
        let sp = c.stats.spell_power;
        let crit = c.stats.crit_chance;
        let aura_target = if def.target == TargetKind::Caster {
            caster
        } else {
            target
        };
        self.apply_effects(caster, &def.effects, ability, aura_target, sp, crit, t);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_effects(
        &mut self,
        caster: EntityId,
        effects: &[Effect],
        ability: AbilityId,
        target: EntityId,
        sp: f32,
        crit_chance: f32,
        t: Ms,
    ) {
        for e in effects {
            match e {
                Effect::Damage {
                    base,
                    coeff_spellpower,
                    ..
                } => {
                    let (amount, crit) = self.roll(base + coeff_spellpower * sp, crit_chance);
                    self.damage(caster, target, amount, ability, None, crit, t);
                }
                Effect::Heal {
                    base,
                    coeff_spellpower,
                } => {
                    let (amount, crit) = self.roll(base + coeff_spellpower * sp, crit_chance);
                    self.heal(caster, caster, amount, ability, crit);
                }
                Effect::RestoreMana { amount } => {
                    if let Some(c) = self.entities.get_mut(&caster) {
                        c.caster.mana = (c.caster.mana + amount).min(c.caster.max_mana);
                    }
                }
                Effect::ApplyAura { aura } => {
                    let aura_id = self.pack.aura_index(aura).expect("validated at load");
                    let def = &self.pack.auras[aura_id as usize];
                    let inst = AuraInstance {
                        aura: aura_id,
                        source: caster,
                        spell_power: sp,
                        expires_at: t + def.duration_ms as Ms,
                        next_tick: t + def.tick_ms.max(1) as Ms,
                    };
                    let Some(te) = self.entities.get_mut(&target) else {
                        continue;
                    };
                    if te.dead || te.evading() {
                        continue;
                    }
                    te.auras
                        .retain(|a| !(a.aura == aura_id && a.source == caster));
                    te.auras.push(inst);
                    self.engage(caster, target, 0.0, t);
                }
            }
        }
    }

    pub fn roll(&mut self, base: f32, crit_chance: f32) -> (f32, bool) {
        let variance = 0.95 + 0.1 * self.rand01();
        let crit = self.rand01() < crit_chance;
        (
            (base * variance * if crit { 1.5 } else { 1.0 }).round(),
            crit,
        )
    }

    pub fn rand01(&mut self) -> f32 {
        // xorshift64*: deterministic and dependency-free.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Mark both sides in combat and add threat on mobs.
    fn engage(&mut self, source: EntityId, target: EntityId, threat: f32, t: Ms) {
        if let Some(s) = self.entities.get_mut(&source) {
            s.combat_until = t + COMBAT_TIMEOUT_MS;
        }
        if let Some(tg) = self.entities.get_mut(&target) {
            tg.combat_until = t + COMBAT_TIMEOUT_MS;
            if let Some(m) = tg.mob.as_mut()
                && !tg.passive
            {
                match m.threat.iter_mut().find(|e| e.0 == source) {
                    Some(e) => e.1 += threat,
                    None => m.threat.push((source, threat.max(1.0))),
                }
                if m.state == MobState::Idle {
                    m.state = MobState::Combat;
                    m.wander_to = None;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn damage(
        &mut self,
        source: EntityId,
        target: EntityId,
        amount: f32,
        ability: AbilityId,
        aura: Option<AuraId>,
        crit: bool,
        t: Ms,
    ) {
        let Some(tg) = self.entities.get(&target) else {
            return;
        };
        if tg.dead || tg.evading() {
            return;
        }
        self.engage(source, target, amount, t);
        let tg = self.entities.get_mut(&target).unwrap();
        tg.hp -= amount;
        let passive = tg.passive;
        if let Some(m) = tg.mob.as_mut() {
            m.last_hit = t;
        }
        if passive && tg.hp <= 0.0 {
            tg.hp = tg.stats.max_hp; // training dummies never die
        }
        let killed = tg.hp <= 0.0;
        let hit = ServerMsg::Hit {
            source,
            target,
            ability,
            aura,
            amount,
            crit,
            heal: false,
        };
        self.send_entity(source, hit.clone());
        if source != target {
            self.send_entity(target, hit);
        }
        if killed {
            self.kill(target, source, t);
        }
    }

    pub fn heal(&mut self, source: EntityId, target: EntityId, amount: f32, ability: AbilityId, crit: bool) {
        let Some(tg) = self.entities.get_mut(&target) else {
            return;
        };
        if tg.dead {
            return;
        }
        let amount = amount.min(tg.stats.max_hp - tg.hp).max(0.0);
        tg.hp += amount;
        let hit = ServerMsg::Hit {
            source,
            target,
            ability,
            aura: None,
            amount,
            crit,
            heal: true,
        };
        self.send_entity(target, hit);
    }

    pub fn kill(&mut self, victim: EntityId, killer: EntityId, t: Ms) {
        let Some(v) = self.entities.get_mut(&victim) else {
            return;
        };
        v.dead = true;
        v.hp = 0.0;
        v.caster.cancel();
        v.queued = None;
        v.auras.clear();
        v.moving = false;
        v.combat_until = 0;
        let mut xp_to: Vec<EntityId> = Vec::new();
        if let Some(m) = v.mob.as_mut() {
            m.state = MobState::Idle;
            m.respawn_at = t + m.respawn_ms;
            xp_to = m.threat.drain(..).map(|(id, _)| id).collect();
        }
        let msg = ServerMsg::Died {
            entity: victim,
            killer,
        };
        self.send_entity(victim, msg.clone());
        if killer != victim {
            self.send_entity(killer, msg);
        }
        if self.entities[&victim].kind == EntityKind::Mob {
            self.on_mob_killed(victim, &xp_to, t);
        }
    }

    /// Process every combat timer that has fallen due, each at its own
    /// logical timestamp, so nothing is rounded to a tick boundary.
    pub fn advance(&mut self, now: Ms, now_us: u64) {
        let mut ids = std::mem::take(&mut self.scratch);
        ids.clear();
        ids.extend(
            self.entities
                .values()
                .filter(|e| e.caster.cast.is_some() || e.queued.is_some() || !e.auras.is_empty())
                .map(|e| e.id),
        );
        for &id in &ids {
            self.advance_entity(id, now, now_us);
        }
        self.scratch = ids;
    }

    fn advance_entity(&mut self, id: EntityId, now: Ms, now_us: u64) {
        let pack = self.pack.clone();
        loop {
            let Some(e) = self.entities.get_mut(&id) else {
                return;
            };
            if let Some(cast) = e.caster.cast.clone() {
                if let Some(tick_at) = cast.next_tick_at().filter(|&t| t <= now) {
                    e.caster.cast.as_mut().unwrap().ticks_done += 1;
                    // Channels stop if the target died or left.
                    if self.channel_target_lost(id, cast.target, cast.ability) {
                        self.stop_cast(id, StopReason::Interrupted);
                        continue;
                    }
                    self.resolve(id, cast.ability, cast.target, tick_at);
                    continue;
                }
                if cast.ends_at <= now {
                    let def = &pack.abilities[cast.ability as usize];
                    e.caster.complete_cast(def);
                    self.send_entity(
                        id,
                        ServerMsg::CastStop {
                            caster: id,
                            seq: cast.seq,
                            reason: StopReason::Completed,
                        },
                    );
                    if !cast.channel {
                        self.resolve(id, cast.ability, cast.target, cast.ends_at);
                    }
                    continue;
                }
            }
            let e = self.entities.get_mut(&id).unwrap();
            if let Some(q) = &e.queued {
                let def = &pack.abilities[q.ability as usize];
                let ready = e.caster.ready_at(q.ability, def);
                if ready <= now {
                    let q = e.queued.take().unwrap();
                    let t = ready.max(q.received_at);
                    self.try_start(id, q, t, Some(t), now_us);
                    continue;
                }
            }
            break;
        }
        self.advance_auras(id, now);
    }

    fn channel_target_lost(&self, caster: EntityId, target: EntityId, ability: AbilityId) -> bool {
        if self.pack.abilities[ability as usize].target == TargetKind::Caster {
            return false;
        }
        let Some(c) = self.entities.get(&caster) else {
            return true;
        };
        !matches!(self.entities.get(&target), Some(t) if !t.dead && self.hostile(c, t))
    }

    fn advance_auras(&mut self, id: EntityId, now: Ms) {
        let mut i = 0;
        loop {
            let Some(e) = self.entities.get_mut(&id) else {
                return;
            };
            if i >= e.auras.len() {
                return;
            }
            let a = &e.auras[i];
            let def = &self.pack.auras[a.aura as usize];
            if def.tick_ms > 0 && a.next_tick <= a.expires_at && a.next_tick <= now {
                let (tick_at, source, aura, sp) = (a.next_tick, a.source, a.aura, a.spell_power);
                e.auras[i].next_tick += def.tick_ms as Ms;
                let base = def.tick_base + def.tick_coeff_spellpower * sp;
                let (amount, crit) = self.roll(base, 0.0);
                let ability = self.ability_for_aura(aura);
                self.damage(source, id, amount, ability, Some(aura), crit, tick_at);
                continue;
            }
            if a.expires_at <= now {
                e.auras.remove(i);
                continue;
            }
            i += 1;
        }
    }

    fn ability_for_aura(&self, aura: AuraId) -> AbilityId {
        let id = &self.pack.auras[aura as usize].id;
        self.pack
            .abilities
            .iter()
            .position(|a| {
                a.effects
                    .iter()
                    .any(|e| matches!(e, Effect::ApplyAura { aura } if aura == id))
            })
            .unwrap_or(0) as AbilityId
    }

    pub fn next_deadline(&self) -> Option<Ms> {
        let mut best: Option<Ms> = None;
        let mut take = |t: Ms| best = Some(best.map_or(t, |b: Ms| b.min(t)));
        for e in self.entities.values() {
            if let Some(c) = &e.caster.cast {
                take(c.next_tick_at().unwrap_or(c.ends_at).min(c.ends_at));
            }
            if let Some(q) = &e.queued {
                take(
                    e.caster
                        .ready_at(q.ability, &self.pack.abilities[q.ability as usize]),
                );
            }
            for a in &e.auras {
                take(a.next_tick.min(a.expires_at));
            }
        }
        best
    }
}

/// Facing from `from` towards `to`, or `current` if they coincide.
pub fn face(from: [f32; 3], to: [f32; 3], current: f32) -> f32 {
    let (dx, dz) = (to[0] - from[0], to[2] - from[2]);
    if dx * dx + dz * dz < 0.01 {
        current
    } else {
        dx.atan2(dz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interest::VIEW_CAP;

    fn zone() -> Zone {
        let pack = ContentPack::load_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tenants/default"))
            .unwrap();
        let idx = pack.zone_index("elderwood").unwrap();
        Zone::new(Arc::new(pack), idx, 1, 0)
    }

    fn mob_of(z: &Zone, id: &str) -> EntityId {
        let m = z.pack.mob_index(id).unwrap();
        *z.entities
            .iter()
            .find(|(_, e)| e.kind == EntityKind::Mob && e.def == m)
            .unwrap()
            .0
    }

    #[test]
    fn mob_dies_and_respawns_at_home() {
        let mut z = zone();
        let p = z.add_player(1, "A".into(), 0, 1, None, 0);
        let wolf = mob_of(&z, "forest_wolf");
        let home = z.entities[&wolf].mob.as_ref().unwrap().home;
        z.damage(p, wolf, 1e6, 0, None, false, 100);
        assert!(z.entities[&wolf].dead);
        let respawn = z.entities[&wolf].mob.as_ref().unwrap().respawn_at;
        z.tick_ai(respawn + 1, 0);
        let w = &z.entities[&wolf];
        assert!(!w.dead && w.hp == w.stats.max_hp);
        assert_eq!(w.pos, home);
    }

    #[test]
    fn aggro_then_leash_and_evade() {
        let mut z = zone();
        let wolf = mob_of(&z, "forest_wolf");
        let wpos = z.entities[&wolf].pos;
        let p = z.add_player(1, "A".into(), 0, 1, Some([wpos[0] + 5.0, 0.0, wpos[2]]), 0);
        // Force the exact spot (spawn jitter) next to the wolf.
        z.entities.get_mut(&p).unwrap().pos = [wpos[0] + 5.0, 0.0, wpos[2]];
        z.relocate(p);
        z.tick_ai(50, 0);
        let m = z.entities[&wolf].mob.as_ref().unwrap();
        assert_eq!(m.state, MobState::Combat);
        assert_eq!(m.top_threat(), Some(p));
        // Chase for a while: the wolf closes in and starts biting.
        for t in 1..60 {
            z.tick_ai(50 + t * 50, 0);
            z.advance(50 + t * 50, 0);
        }
        assert!(z.entities[&p].hp < z.entities[&p].stats.max_hp);
        // The player teleports far away: the wolf leashes and walks home.
        z.entities.get_mut(&p).unwrap().pos = [0.0, 0.0, 150.0];
        z.relocate(p);
        z.tick_ai(4000, 0);
        assert_eq!(z.entities[&wolf].mob.as_ref().unwrap().state, MobState::Evade);
        for t in 0..400 {
            z.tick_ai(4050 + t * 50, 0);
        }
        let w = &z.entities[&wolf];
        assert_eq!(w.mob.as_ref().unwrap().state, MobState::Idle);
        assert_eq!(w.hp, w.stats.max_hp);
    }

    #[test]
    fn crowd_view_is_capped_and_target_kept() {
        let mut z = zone();
        let far_wolf = mob_of(&z, "forest_wolf");
        for c in 1..=150u64 {
            z.add_player(c, format!("P{c}"), 0, 1, None, 0);
        }
        let me = z.clients[&1];
        z.entities.get_mut(&me).unwrap().player.as_mut().unwrap().target = far_wolf;
        z.outbox.clear();
        z.snapshot(50);
        assert!(z.nets.values().all(|n| n.last_view <= VIEW_CAP + 4));
        assert_eq!(z.outbox.len(), 150);
        // Client 1 sees its far-away target even though 149 players are closer.
        let bytes = &z.outbox.iter().find(|o| o.client == 1).unwrap().bytes;
        let d = combat_core::codec::decode_snapshot(bytes, &[]).unwrap();
        assert!(d.entities.iter().any(|(id, _)| *id == far_wolf));
        assert!(d.entities.iter().any(|(id, _)| *id == me));
    }
}

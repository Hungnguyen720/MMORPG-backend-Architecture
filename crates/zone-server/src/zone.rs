//! Authoritative zone state. Combat outcomes are decided only here.
//!
//! Players cast from the content pack's ability list. Everything else is an
//! `Npc`: the training dummy, hostile mobs placed by camps, and the pawns
//! that follow each player. NPC brains run on a fixed 20 Hz AI tick; player
//! casts, channels and auras still fire the moment they fall due.

use std::collections::{BTreeMap, HashMap, VecDeque};

use combat_core::movement::{ARENA_HALF_EXTENT, MoveValidator, dist_xz};
use combat_core::projectile::{
    self, BODY_RADIUS, Flight, INTERP_DELAY_MS, MAX_DODGE_GRACE_MS, MAX_REWIND_MS,
    SERVER_HIT_LEEWAY, xz,
};
use combat_core::protocol::{ClientMsg, EntityKind, EntitySnap, PawnCmd, ServerMsg, StopReason};
use combat_core::{
    AbilityId, AuraId, CastCtx, CasterState, ContentPack, Delivery, Effect, EntityId, FailReason,
    Ms, OUT_OF_COMBAT_MS, PawnRole, Started, channel,
};
use renet::ClientId;

pub enum Dest {
    All,
    One(ClientId),
}

pub const AI_TICK_MS: Ms = 50;
/// Camp mates this close join a fight one of them is pulled into.
const SOCIAL_AGGRO_RADIUS: f32 = 10.0;
/// Pawns defend against anything fighting the party within this radius.
const PAWN_ASSIST_RADIUS: f32 = 25.0;
/// A pawn further than this from its owner is teleported back.
const PAWN_TELEPORT_DIST: f32 = 35.0;
const REGROUP_MS: Ms = 4000;
/// Bodies push apart below this distance.
const PERSONAL_SPACE: f32 = 0.9;
const NPC_CRIT_CHANCE: f32 = 0.05;
/// Players and pawns start at entity ids above every mob.
const FIRST_DYNAMIC_ENTITY: EntityId = 1000;
/// Skillshots are swept in steps this long; the hit lands at the step's end.
const SHOT_STEP_MS: Ms = 20;
/// Hostile positions kept for rewinding aimed hits (one per AI tick).
const TRAIL_LEN: usize = 12;

struct Queued {
    seq: u32,
    ability: AbilityId,
    target: EntityId,
    move_seq: u32,
    received_at: Ms,
    /// Ground point for aimed abilities, as the client sent it.
    aim: Option<[f32; 2]>,
}

/// A skillshot in flight on the server.
struct Shot {
    id: u32,
    client: ClientId,
    ability: AbilityId,
    flight: Flight,
    /// Hostiles are tested where they were this long ago: where the shooter
    /// saw them.
    rewind_ms: Ms,
    swept_to: Ms,
}

/// A player's ground ability waiting to land.
struct Blast {
    client: ClientId,
    ability: AbilityId,
    pos: [f32; 2],
    lands_at: Ms,
    rewind_ms: Ms,
}

/// A mob's telegraphed attack waiting to land.
struct Slam {
    source: EntityId,
    pos: [f32; 2],
    radius: f32,
    damage: f32,
    lands_at: Ms,
    pawns_done: bool,
    /// Players already resolved (each one gets their own latency grace).
    players_done: Vec<EntityId>,
}

struct Player {
    entity: EntityId,
    pos: [f32; 3],
    facing: f32,
    moving: bool,
    last_move_seq: u32,
    /// Moves up to this seq were sent before the current cast was pressed.
    cast_move_seq: u32,
    validator: MoveValidator,
    caster: CasterState,
    queued: Option<Queued>,
    rtt_ms: Ms,
    last_regen: Ms,
    hp: f32,
    last_damaged: Ms,
    /// Set while dead: when the player respawns.
    respawn_at: Option<Ms>,
    target: Option<EntityId>,
    pawns: Vec<EntityId>,
    /// Resolved aim of the cast in progress, if it is an aimed ability.
    cast_aim: Option<[f32; 2]>,
    /// How much earlier than its bar the current cast ends on the server.
    cast_allowance: Ms,
}

struct AuraInstance {
    aura: AuraId,
    source: EntityId,
    spell_power: f32,
    expires_at: Ms,
    next_tick: Ms,
}

enum Brain {
    /// Stands still and never dies.
    Dummy,
    Mob {
        home: [f32; 3],
        camp: usize,
        /// Walking home after a leash break; immune and healing on arrival.
        evading: bool,
    },
    Pawn {
        owner: EntityId,
        slot: usize,
        regroup_until: Ms,
        /// Set by the Attack command; held until the target dies.
        order: Option<EntityId>,
    },
}

struct Npc {
    entity: EntityId,
    kind: EntityKind,
    /// Index into `pack.mobs` or `pack.pawns`.
    def: u16,
    pos: [f32; 3],
    facing: f32,
    hp: f32,
    max_hp: f32,
    last_hit: Ms,
    auras: Vec<AuraInstance>,
    /// Who has hurt this NPC and how much. Mobs attack the top entry.
    threat: Vec<(EntityId, f32)>,
    target: Option<EntityId>,
    next_attack: Ms,
    /// Set while dead: when it died.
    died_at: Option<Ms>,
    brain: Brain,
    /// Recent (time, position), newest last, for rewinding aimed hits.
    trail: VecDeque<(Ms, [f32; 3])>,
    /// Mobs with a slam: when it may next be used, and the wind-up end.
    next_slam: Ms,
    winding_until: Ms,
}

impl Npc {
    fn alive(&self) -> bool {
        self.died_at.is_none()
    }

    fn add_threat(&mut self, source: EntityId, amount: f32) {
        match self.threat.iter_mut().find(|(e, _)| *e == source) {
            Some(entry) => entry.1 += amount,
            None => self.threat.push((source, amount)),
        }
    }

    /// Where this NPC stood at time `t`, from its trail.
    fn pos_at(&self, t: Ms) -> [f32; 3] {
        let Some(&(last_t, last)) = self.trail.back() else {
            return self.pos;
        };
        if t >= last_t {
            return last;
        }
        let mut prev = self.trail[0];
        if t <= prev.0 {
            return prev.1;
        }
        for &(bt, bp) in self.trail.iter().skip(1) {
            if t <= bt {
                let k = (t - prev.0) as f32 / (bt - prev.0).max(1) as f32;
                let (a, b) = (prev.1, bp);
                return [
                    a[0] + (b[0] - a[0]) * k,
                    a[1] + (b[1] - a[1]) * k,
                    a[2] + (b[2] - a[2]) * k,
                ];
            }
            prev = (bt, bp);
        }
        last
    }

    fn radius(&self, pack: &ContentPack) -> f32 {
        let scale = match self.kind {
            EntityKind::Mob => pack.mobs[self.def as usize].scale,
            _ => 1.0,
        };
        BODY_RADIUS * scale
    }

    /// The radius player abilities are judged against on the server.
    fn hit_radius(&self, pack: &ContentPack) -> f32 {
        self.radius(pack) + SERVER_HIT_LEEWAY
    }

    fn top_threat(&self) -> Option<EntityId> {
        self.threat
            .iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(e, _)| *e)
    }
}

/// Everything an NPC brain needs from the rest of the zone, built once per
/// AI tick so brains can read other entities while one of them is mutated.
#[derive(Clone)]
struct Body {
    kind: EntityKind,
    pos: [f32; 3],
    facing: f32,
    hp: f32,
    max_hp: f32,
    alive: bool,
    /// Pawns: their owner. Players: themselves.
    party: Option<EntityId>,
    /// Mobs: the parties on their threat table.
    fighting: Vec<EntityId>,
}

#[derive(Default)]
struct QueueStats {
    count: u32,
    total_us: u64,
    max_us: u64,
}

enum Action {
    None,
    Attack(EntityId),
    Heal(EntityId),
    /// Start a telegraphed slam centred here.
    Slam([f32; 2]),
}

pub struct Zone {
    pack: ContentPack,
    players: HashMap<ClientId, Player>,
    npcs: BTreeMap<EntityId, Npc>,
    next_entity: EntityId,
    next_ai: Ms,
    outbox: Vec<(Dest, u8, ServerMsg)>,
    shots: Vec<Shot>,
    blasts: Vec<Blast>,
    slams: Vec<Slam>,
    next_shot_id: u32,
    rng: u64,
    stats: QueueStats,
    last_stats_print: Ms,
}

impl Zone {
    pub fn new(pack: ContentPack) -> Self {
        let mut npcs = BTreeMap::new();
        let dummy = 1;
        npcs.insert(
            dummy,
            Npc {
                entity: dummy,
                kind: EntityKind::Dummy,
                def: 0,
                pos: pack.dummy.position,
                facing: 0.0,
                hp: pack.dummy.max_hp,
                max_hp: pack.dummy.max_hp,
                last_hit: 0,
                auras: Vec::new(),
                threat: Vec::new(),
                target: None,
                next_attack: 0,
                died_at: None,
                trail: VecDeque::new(),
                next_slam: 0,
                winding_until: 0,
                brain: Brain::Dummy,
            },
        );
        let mut next: EntityId = 2;
        for (camp, c) in pack.camps.iter().enumerate() {
            let def = pack.mob_index(&c.mob).expect("validated at load");
            let m = &pack.mobs[def as usize];
            for i in 0..c.count {
                // Spread the camp on a small circle around its centre.
                let a = i as f32 / c.count.max(1) as f32 * std::f32::consts::TAU;
                let r = if c.count > 1 { 2.5 } else { 0.0 };
                let home = [
                    c.position[0] + a.cos() * r,
                    0.0,
                    c.position[2] + a.sin() * r,
                ];
                npcs.insert(
                    next,
                    Npc {
                        entity: next,
                        kind: EntityKind::Mob,
                        def,
                        pos: home,
                        facing: a + std::f32::consts::PI,
                        hp: m.max_hp,
                        max_hp: m.max_hp,
                        last_hit: 0,
                        auras: Vec::new(),
                        threat: Vec::new(),
                        target: None,
                        next_attack: 0,
                        died_at: None,
                        trail: VecDeque::new(),
                        next_slam: 0,
                        winding_until: 0,
                        brain: Brain::Mob {
                            home,
                            camp,
                            evading: false,
                        },
                    },
                );
                next += 1;
            }
        }
        assert!(
            next < FIRST_DYNAMIC_ENTITY,
            "too many mobs for the id range"
        );
        println!(
            "world: {} mobs in {} camps, {} pawn types",
            npcs.len() - 1,
            pack.camps.len(),
            pack.pawns.len()
        );
        Self {
            pack,
            players: HashMap::new(),
            npcs,
            next_entity: FIRST_DYNAMIC_ENTITY,
            next_ai: 0,
            outbox: Vec::new(),
            shots: Vec::new(),
            blasts: Vec::new(),
            slams: Vec::new(),
            next_shot_id: 1,
            rng: 0x9E37_79B9_7F4A_7C15,
            stats: QueueStats::default(),
            last_stats_print: 0,
        }
    }

    fn alloc(&mut self) -> EntityId {
        let e = self.next_entity;
        self.next_entity += 1;
        e
    }

    fn spawn_point(&self, entity: EntityId) -> [f32; 3] {
        let s = self.pack.world.spawn;
        [s[0] + ((entity % 8) as f32 - 4.0) * 1.5, 0.0, s[2]]
    }

    pub fn add_player(&mut self, client_id: ClientId, now: Ms) {
        let entity = self.alloc();
        let spawn = self.spawn_point(entity);
        let mut validator = MoveValidator::default();
        validator.reset(now, spawn);
        let c = &self.pack.class;
        let mut player = Player {
            entity,
            pos: spawn,
            facing: 0.0,
            moving: false,
            last_move_seq: 0,
            cast_move_seq: 0,
            validator,
            caster: CasterState::new(self.pack.abilities.len(), c.max_mana),
            queued: None,
            rtt_ms: 0,
            last_regen: now,
            hp: c.max_hp,
            last_damaged: 0,
            respawn_at: None,
            target: None,
            pawns: Vec::new(),
            cast_aim: None,
            cast_allowance: 0,
        };
        let party: Vec<u16> = self
            .pack
            .pawn_party
            .iter()
            .map(|id| self.pack.pawn_index(id).expect("validated at load"))
            .collect();
        for (slot, def) in party.into_iter().enumerate() {
            let pe = self.alloc();
            let d = &self.pack.pawns[def as usize];
            let pos = formation_point(spawn, 0.0, slot);
            self.npcs.insert(
                pe,
                Npc {
                    entity: pe,
                    kind: EntityKind::Pawn,
                    def,
                    pos,
                    facing: 0.0,
                    hp: d.max_hp,
                    max_hp: d.max_hp,
                    last_hit: 0,
                    auras: Vec::new(),
                    threat: Vec::new(),
                    target: None,
                    next_attack: 0,
                    died_at: None,
                    trail: VecDeque::new(),
                    next_slam: 0,
                    winding_until: 0,
                    brain: Brain::Pawn {
                        owner: entity,
                        slot,
                        regroup_until: 0,
                        order: None,
                    },
                },
            );
            player.pawns.push(pe);
        }
        println!(
            "client {client_id} joined as entity {entity} with pawns {:?}",
            player.pawns
        );
        self.players.insert(client_id, player);
        self.send(
            Dest::One(client_id),
            ServerMsg::Welcome {
                you: entity,
                content_hash: self.pack.hash,
                server_time: now,
            },
        );
        self.send(Dest::One(client_id), ServerMsg::Correction { pos: spawn });
    }

    pub fn remove_player(&mut self, client_id: ClientId) {
        if let Some(p) = self.players.remove(&client_id) {
            for pe in p.pawns {
                self.npcs.remove(&pe);
            }
        }
    }

    pub fn drain_outbox(&mut self) -> Vec<(Dest, u8, ServerMsg)> {
        std::mem::take(&mut self.outbox)
    }

    fn send(&mut self, dest: Dest, msg: ServerMsg) {
        if std::env::var_os("ZONE_DEBUG").is_some() && !matches!(msg, ServerMsg::Snapshot { .. }) {
            println!("  -> {msg:?}");
        }
        let ch = match msg {
            ServerMsg::Snapshot { .. } => channel::STATE,
            _ => channel::COMBAT,
        };
        self.outbox.push((dest, ch, msg));
    }

    fn player_by_entity(&self, e: EntityId) -> Option<ClientId> {
        self.players
            .iter()
            .find(|(_, p)| p.entity == e)
            .map(|(c, _)| *c)
    }

    /// Position of a living NPC that players may attack.
    fn hostile_pos(&self, target: EntityId) -> Option<[f32; 3]> {
        self.npcs
            .get(&target)
            .filter(|n| n.kind.hostile() && n.alive())
            .map(|n| n.pos)
    }

    pub fn handle(
        &mut self,
        client_id: ClientId,
        msg: ClientMsg,
        now: Ms,
        now_us: u64,
        rtt_ms: Ms,
    ) {
        let speed = self.pack.class.move_speed;
        let Some(p) = self.players.get_mut(&client_id) else {
            return;
        };
        p.rtt_ms = rtt_ms;
        match msg {
            ClientMsg::Move {
                seq,
                pos,
                facing,
                moving,
            } => {
                if seq <= p.last_move_seq {
                    return; // stale or duplicate
                }
                p.last_move_seq = seq;
                if p.respawn_at.is_some() {
                    return; // the dead don't walk
                }
                if !p.validator.check(now, pos, speed) {
                    let back = p.pos;
                    p.validator.reset(now, back);
                    println!("entity {} failed movement check, correcting", p.entity);
                    self.send(Dest::One(client_id), ServerMsg::Correction { pos: back });
                    return;
                }
                p.pos = pos;
                p.facing = facing;
                p.moving = moving;
                // A move sent after the cast was pressed breaks it, unless the
                // ability allows casting on the move.
                if moving
                    && seq > p.cast_move_seq
                    && let Some(cast) = &p.caster.cast
                {
                    let def = &self.pack.abilities[cast.ability as usize];
                    if !def.cast_while_moving {
                        self.stop_cast(client_id, StopReason::Interrupted);
                    }
                }
            }
            ClientMsg::Cast {
                seq,
                ability,
                target,
                queued,
                move_seq,
                aim,
            } => {
                let Some(def) = self.pack.ability(ability) else {
                    self.send(
                        Dest::One(client_id),
                        ServerMsg::CastFailed {
                            seq,
                            reason: FailReason::UnknownAbility,
                        },
                    );
                    return;
                };
                if p.respawn_at.is_some() {
                    self.send(
                        Dest::One(client_id),
                        ServerMsg::CastFailed {
                            seq,
                            reason: FailReason::Dead,
                        },
                    );
                    return;
                }
                // The newest press replaces anything already queued, as in WoW.
                let ready = p.caster.ready_at(ability, def);
                if ready > now {
                    if p.caster.in_queue_window(ability, def, now) {
                        if let Some(old) = p.queued.take() {
                            self.send(
                                Dest::One(client_id),
                                ServerMsg::CastFailed {
                                    seq: old.seq,
                                    reason: FailReason::NotReady,
                                },
                            );
                        }
                        let p = self.players.get_mut(&client_id).unwrap();
                        p.queued = Some(Queued {
                            seq,
                            ability,
                            target,
                            move_seq,
                            received_at: now,
                            aim,
                        });
                    } else {
                        let reason = p
                            .caster
                            .check(ability, def, now, ctx_for(p, None))
                            .unwrap_err();
                        self.send(Dest::One(client_id), ServerMsg::CastFailed { seq, reason });
                    }
                    return;
                }
                let _ = queued;
                self.try_start(
                    client_id,
                    Queued {
                        seq,
                        ability,
                        target,
                        move_seq,
                        received_at: now,
                        aim,
                    },
                    now,
                    None,
                    now_us,
                );
            }
            ClientMsg::CancelCast => {
                p.queued = None;
                if p.caster.cast.is_some() {
                    self.stop_cast(client_id, StopReason::Cancelled);
                }
            }
            ClientMsg::SetTarget { target } => {
                p.target = target;
            }
            ClientMsg::PawnCommand { cmd } => {
                let (target, pawns) = (p.target, p.pawns.clone());
                let target = target.filter(|&t| self.hostile_pos(t).is_some());
                for pe in pawns {
                    if let Some(Npc {
                        brain:
                            Brain::Pawn {
                                regroup_until,
                                order,
                                ..
                            },
                        ..
                    }) = self.npcs.get_mut(&pe)
                    {
                        match cmd {
                            PawnCmd::Attack => {
                                *order = target;
                                *regroup_until = 0;
                            }
                            PawnCmd::Regroup => {
                                *order = None;
                                *regroup_until = now + REGROUP_MS;
                            }
                        }
                    }
                }
            }
        }
    }

    fn stop_cast(&mut self, client_id: ClientId, reason: StopReason) {
        let p = self.players.get_mut(&client_id).unwrap();
        if let Some(cast) = p.caster.cancel() {
            p.queued = None;
            let caster = p.entity;
            self.send(
                Dest::All,
                ServerMsg::CastStop {
                    caster,
                    seq: cast.seq,
                    reason,
                },
            );
        }
    }

    /// Validate and start a cast at logical time `t`. `queued_ready` is the
    /// moment the caster became ready, for casts fired from the queue.
    fn try_start(
        &mut self,
        client_id: ClientId,
        q: Queued,
        t: Ms,
        queued_ready: Option<Ms>,
        now_us: u64,
    ) {
        let aimed = self.pack.abilities[q.ability as usize].delivery.aimed();
        let mut aim = None;
        if aimed {
            match self.resolve_aim(client_id, &q) {
                Ok(a) => aim = Some(a),
                Err(reason) => {
                    self.send(
                        Dest::One(client_id),
                        ServerMsg::CastFailed { seq: q.seq, reason },
                    );
                    return;
                }
            }
        } else {
            let friendly = self.npcs.get(&q.target).is_some_and(|n| !n.kind.hostile())
                || self.player_by_entity(q.target).is_some();
            if friendly {
                self.send(
                    Dest::One(client_id),
                    ServerMsg::CastFailed {
                        seq: q.seq,
                        reason: FailReason::InvalidTarget,
                    },
                );
                return;
            }
        }
        // Aimed abilities were clamped to their range above.
        let dist = if aimed {
            Some(0.0)
        } else {
            let p = &self.players[&client_id];
            self.hostile_pos(q.target).map(|tp| dist_xz(p.pos, tp))
        };
        let p = self.players.get_mut(&client_id).unwrap();
        let def = &self.pack.abilities[q.ability as usize];
        if let Err(reason) = p.caster.check(q.ability, def, t, ctx_for(p, dist)) {
            self.send(
                Dest::One(client_id),
                ServerMsg::CastFailed { seq: q.seq, reason },
            );
            return;
        }
        let allowance = p.rtt_ms / 2;
        let started = p
            .caster
            .start(q.ability, def, q.seq, q.target, t, allowance);
        p.cast_move_seq = p.cast_move_seq.max(q.move_seq);
        p.cast_aim = aim;
        p.cast_allowance = p
            .caster
            .cast
            .as_ref()
            .map_or(0, |c| c.bar_ms - (c.ends_at - c.started_at));
        let caster = p.entity;
        let queue_gap_us = queued_ready.map(|ready| now_us.saturating_sub(ready * 1000) as u32);
        if let Some(gap) = queue_gap_us {
            self.stats.count += 1;
            self.stats.total_us += gap as u64;
            self.stats.max_us = self.stats.max_us.max(gap as u64);
            println!(
                "[{t:>8} ms] entity {caster} {:<15} seq {:<4} from queue, started {:.2} ms after caster was ready",
                def.name,
                q.seq,
                gap as f64 / 1000.0
            );
        } else {
            println!(
                "[{t:>8} ms] entity {caster} {:<15} seq {:<4}",
                def.name, q.seq
            );
        }
        let bar_ms = def.bar_ms() as u32;
        self.send(
            Dest::All,
            ServerMsg::CastStart {
                caster,
                seq: q.seq,
                ability: q.ability,
                target: q.target,
                bar_ms,
                queue_gap_us,
            },
        );
        if started == Started::Instant {
            match aim {
                Some(aim) => self.launch(client_id, q.ability, q.seq, aim, t, 0),
                None => self.resolve(client_id, q.ability, q.target, t),
            }
        }
    }

    /// Where an aimed ability goes: the client's aim point, else the target
    /// (soft targeting, so a tap still works), clamped to range.
    fn resolve_aim(&self, client_id: ClientId, q: &Queued) -> Result<[f32; 2], FailReason> {
        let p = &self.players[&client_id];
        let def = &self.pack.abilities[q.ability as usize];
        let from = xz(p.pos);
        let wanted = q
            .aim
            .filter(|a| a.iter().all(|v| v.is_finite()))
            .or_else(|| self.hostile_pos(q.target).map(xz));
        Ok(match def.delivery {
            Delivery::Target => unreachable!("only aimed abilities have an aim"),
            // A skillshot only needs a direction; with nothing to aim at it
            // flies along the caster's facing.
            Delivery::Skillshot => wanted.unwrap_or(from),
            Delivery::Ground => {
                projectile::clamp_aim(from, wanted.ok_or(FailReason::NoTarget)?, def.range)
            }
            Delivery::Dash => {
                let dir = projectile::direction(from, wanted.unwrap_or(from), p.facing);
                [from[0] + dir[0] * def.dash_m, from[1] + dir[1] * def.dash_m]
            }
        })
    }

    /// An aimed ability leaves its caster at time `t`. `allowance` is how much
    /// earlier than the player's bar the server finished the wind-up.
    fn launch(
        &mut self,
        client_id: ClientId,
        ability: AbilityId,
        seq: u32,
        aim: [f32; 2],
        t: Ms,
        allowance: Ms,
    ) {
        let def = &self.pack.abilities[ability as usize];
        let p = self.players.get_mut(&client_id).unwrap();
        let origin = xz(p.pos);
        // The shooter saw hostiles INTERP_DELAY_MS plus network delay ago:
        // one way for the snapshot, plus one way for the request unless the
        // wind-up already made up for it.
        let rewind_ms = (INTERP_DELAY_MS + p.rtt_ms.saturating_sub(allowance)).min(MAX_REWIND_MS);
        let caster = p.entity;
        match def.delivery {
            Delivery::Target => {}
            Delivery::Dash => p.validator.grant(t, def.dash_m),
            Delivery::Skillshot => {
                let flight = Flight::new(origin, aim, p.facing, def.speed, def.range, def.width, t);
                let id = self.next_shot_id;
                self.next_shot_id += 1;
                self.shots.push(Shot {
                    id,
                    client: client_id,
                    ability,
                    flight,
                    rewind_ms,
                    swept_to: t,
                });
                self.send(
                    Dest::All,
                    ServerMsg::ProjectileSpawn {
                        id,
                        caster,
                        seq,
                        ability,
                        origin,
                        dir: flight.dir,
                        server_time: t,
                    },
                );
            }
            Delivery::Ground => {
                let delay_ms = def.delay_ms;
                self.blasts.push(Blast {
                    client: client_id,
                    ability,
                    pos: aim,
                    lands_at: t + delay_ms as Ms,
                    rewind_ms,
                });
                self.send(
                    Dest::All,
                    ServerMsg::GroundBlast {
                        caster,
                        seq,
                        ability,
                        pos: aim,
                        delay_ms,
                    },
                );
            }
        }
    }

    /// Sweep every skillshot up to `now` against hostiles where the shooter
    /// saw them. A uniform grid replaces the linear scan once zones hold
    /// hundreds of mobs; a greybox zone has a few dozen.
    fn advance_shots(&mut self, now: Ms) {
        let mut i = 0;
        while i < self.shots.len() {
            let end = self.shots[i].flight.ends_at();
            let mut verdict: Option<(Option<EntityId>, Ms, f32)> = None;
            loop {
                let s = &self.shots[i];
                let (a, b) = (s.swept_to, (s.swept_to + SHOT_STEP_MS).min(end));
                if b > now {
                    break;
                }
                let look = ((a + b) / 2).saturating_sub(s.rewind_ms);
                let mut best: Option<(f32, EntityId)> = None;
                for n in self.npcs.values().filter(|n| n.kind.hostile() && n.alive()) {
                    if let Some(d) =
                        s.flight
                            .sweep(a, b, xz(n.pos_at(look)), n.hit_radius(&self.pack))
                        && best.is_none_or(|(bd, _)| d < bd)
                    {
                        best = Some((d, n.entity));
                    }
                }
                let range = s.flight.range;
                self.shots[i].swept_to = b;
                if let Some((d, e)) = best {
                    verdict = Some((Some(e), b, d));
                } else if b >= end {
                    verdict = Some((None, b, range));
                }
                if verdict.is_some() {
                    break;
                }
            }
            let Some((hit, t, d)) = verdict else {
                i += 1;
                continue;
            };
            let shot = self.shots.swap_remove(i);
            let f = shot.flight;
            let at = [f.origin[0] + f.dir[0] * d, f.origin[1] + f.dir[1] * d];
            self.send(
                Dest::All,
                ServerMsg::ProjectileEnd {
                    id: shot.id,
                    hit,
                    at,
                },
            );
            if let Some(e) = hit
                && self.players.contains_key(&shot.client)
            {
                self.resolve(shot.client, shot.ability, e, t);
            }
        }
    }

    /// Land ground abilities that are due, nearest victims first.
    fn advance_blasts(&mut self, now: Ms) {
        let (due, rest): (Vec<Blast>, Vec<Blast>) = std::mem::take(&mut self.blasts)
            .into_iter()
            .partition(|b| b.lands_at <= now);
        self.blasts = rest;
        for b in due {
            if !self.players.contains_key(&b.client) {
                continue;
            }
            let def = &self.pack.abilities[b.ability as usize];
            let look = b.lands_at.saturating_sub(b.rewind_ms);
            let mut victims: Vec<(f32, EntityId)> = self
                .npcs
                .values()
                .filter(|n| n.kind.hostile() && n.alive())
                .filter_map(|n| {
                    let p = xz(n.pos_at(look));
                    projectile::in_blast(b.pos, def.radius, p, n.hit_radius(&self.pack)).then(
                        || {
                            (
                                dist_xz([p[0], 0.0, p[1]], [b.pos[0], 0.0, b.pos[1]]),
                                n.entity,
                            )
                        },
                    )
                })
                .collect();
            victims.sort_by(|a, b| a.0.total_cmp(&b.0));
            victims.truncate(def.max_targets as usize);
            for (_, e) in victims {
                self.resolve(b.client, b.ability, e, b.lands_at);
            }
        }
    }

    /// Land mob slams. Pawns are tested when it lands; each player a little
    /// later (their one-way latency), against their newest position, so a
    /// dodge that cleared the circle on their screen also clears it here.
    fn advance_slams(&mut self, now: Ms) {
        let mut i = 0;
        while i < self.slams.len() {
            if now < self.slams[i].lands_at {
                i += 1;
                continue;
            }
            let source_alive = self
                .npcs
                .get(&self.slams[i].source)
                .is_some_and(|n| n.alive());
            if !source_alive {
                self.slams.swap_remove(i);
                continue;
            }
            let Slam {
                source,
                pos,
                radius,
                damage,
                lands_at,
                ..
            } = self.slams[i];
            let mut victims = Vec::new();
            if !self.slams[i].pawns_done {
                self.slams[i].pawns_done = true;
                for n in self.npcs.values() {
                    if n.kind == EntityKind::Pawn
                        && n.alive()
                        && projectile::in_blast(pos, radius, xz(n.pos), BODY_RADIUS)
                    {
                        victims.push((n.entity, lands_at));
                    }
                }
            }
            for p in self.players.values() {
                let at = lands_at + (p.rtt_ms / 2).min(MAX_DODGE_GRACE_MS);
                if now < at || self.slams[i].players_done.contains(&p.entity) {
                    continue;
                }
                self.slams[i].players_done.push(p.entity);
                if p.respawn_at.is_none()
                    && projectile::in_blast(pos, radius, xz(p.pos), BODY_RADIUS)
                {
                    victims.push((p.entity, at));
                }
            }
            for (target, t) in victims {
                self.send(
                    Dest::All,
                    ServerMsg::Swing {
                        source,
                        target,
                        amount: damage,
                        crit: false,
                        heal: false,
                    },
                );
                self.damage(target, damage, source, 1.0, t);
            }
            if now >= lands_at + MAX_DODGE_GRACE_MS {
                self.slams.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Apply an ability's effects to a hostile NPC.
    fn resolve(&mut self, client_id: ClientId, ability: AbilityId, target: EntityId, t: Ms) {
        if self.hostile_pos(target).is_none() {
            return; // died (or reset) while the cast was in flight
        }
        let source = self.players[&client_id].entity;
        let sp = self.pack.class.spell_power;
        let effects = self.pack.abilities[ability as usize].effects.clone();
        for e in effects {
            match e {
                Effect::Damage {
                    base,
                    coeff_spellpower,
                    ..
                } => {
                    let (amount, crit) = self.roll(base + coeff_spellpower * sp, None);
                    self.send(
                        Dest::All,
                        ServerMsg::Hit {
                            source,
                            target,
                            ability,
                            aura: None,
                            amount,
                            crit,
                        },
                    );
                    self.damage(target, amount, source, 1.0, t);
                }
                Effect::ApplyAura { aura } => {
                    let aura_id = self.pack.aura_index(&aura).expect("validated at load");
                    let def = &self.pack.auras[aura_id as usize];
                    let inst = AuraInstance {
                        aura: aura_id,
                        source,
                        spell_power: sp,
                        expires_at: t + def.duration_ms as Ms,
                        next_tick: t + def.tick_ms.max(1) as Ms,
                    };
                    let duration_ms = def.duration_ms;
                    let Some(n) = self.npcs.get_mut(&target).filter(|n| n.alive()) else {
                        continue;
                    };
                    n.auras
                        .retain(|a| !(a.aura == aura_id && a.source == source));
                    n.auras.push(inst);
                    // Applying a debuff pulls the mob just like damage does.
                    n.add_threat(source, 1.0);
                    self.send(
                        Dest::All,
                        ServerMsg::AuraApplied {
                            target,
                            aura: aura_id,
                            duration_ms,
                        },
                    );
                }
            }
        }
    }

    fn roll(&mut self, base: f32, crit_chance: Option<f32>) -> (f32, bool) {
        let variance = 0.95 + 0.1 * self.rand01();
        let chance = crit_chance.unwrap_or(self.pack.class.crit_chance);
        let crit = self.rand01() < chance;
        (
            (base * variance * if crit { 1.5 } else { 1.0 }).round(),
            crit,
        )
    }

    fn rand01(&mut self) -> f32 {
        // xorshift64*: deterministic and dependency-free.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Deal damage to any entity. Handles threat, social aggro and death.
    fn damage(&mut self, target: EntityId, amount: f32, source: EntityId, threat_mult: f32, t: Ms) {
        if let Some(client) = self.player_by_entity(target) {
            let p = self.players.get_mut(&client).unwrap();
            if p.respawn_at.is_some() {
                return;
            }
            p.hp -= amount;
            p.last_damaged = t;
            if p.hp <= 0.0 {
                p.hp = 0.0;
                p.respawn_at = Some(t + self.pack.class.respawn_ms as Ms);
                p.queued = None;
                self.stop_cast(client, StopReason::Interrupted);
                println!("entity {target} (player) died");
                self.send(
                    Dest::All,
                    ServerMsg::Died {
                        entity: target,
                        killer: Some(source),
                    },
                );
            }
            return;
        }
        let Some(n) = self.npcs.get_mut(&target).filter(|n| n.alive()) else {
            return;
        };
        if matches!(n.brain, Brain::Mob { evading: true, .. }) {
            return; // evading mobs are immune
        }
        n.hp -= amount;
        n.last_hit = t;
        if n.kind.hostile() {
            n.add_threat(source, amount * threat_mult);
        }
        let camp = match n.brain {
            Brain::Mob { camp, .. } => Some(camp),
            _ => None,
        };
        let pos = n.pos;
        if n.hp <= 0.0 {
            if matches!(n.brain, Brain::Dummy) {
                n.hp = n.max_hp; // training dummies never die
                return;
            }
            n.hp = 0.0;
            n.died_at = Some(t);
            n.threat.clear();
            n.target = None;
            n.auras.clear();
            let kind = n.kind;
            println!("entity {target} ({kind:?}) died");
            self.send(
                Dest::All,
                ServerMsg::Died {
                    entity: target,
                    killer: Some(source),
                },
            );
            return;
        }
        // Social aggro: idle camp mates nearby join in.
        if let Some(camp) = camp {
            for m in self.npcs.values_mut() {
                if let Brain::Mob {
                    camp: c, evading, ..
                } = m.brain
                    && c == camp
                    && !evading
                    && m.alive()
                    && m.threat.is_empty()
                    && dist_xz(m.pos, pos) <= SOCIAL_AGGRO_RADIUS
                {
                    m.add_threat(source, 1.0);
                }
            }
        }
    }

    /// Process every combat timer that has fallen due, each at its own
    /// logical timestamp, so nothing is rounded to a tick boundary.
    pub fn advance(&mut self, now: Ms, now_us: u64) {
        let ids: Vec<ClientId> = self.players.keys().copied().collect();
        for id in ids {
            self.advance_player(id, now, now_us);
        }
        self.advance_auras(now);
        self.advance_shots(now);
        self.advance_blasts(now);
        self.advance_slams(now);
        while now >= self.next_ai {
            let t = self.next_ai;
            self.tick_ai(t);
            self.next_ai = t + AI_TICK_MS;
        }
        if now >= self.last_stats_print + 10_000 {
            self.last_stats_print = now;
            if self.stats.count > 0 {
                println!(
                    "-- spell queue, last 10 s: {} queued casts, mean gap {:.2} ms, max gap {:.2} ms",
                    self.stats.count,
                    self.stats.total_us as f64 / self.stats.count as f64 / 1000.0,
                    self.stats.max_us as f64 / 1000.0
                );
                self.stats = QueueStats::default();
            }
        }
    }

    fn advance_player(&mut self, id: ClientId, now: Ms, now_us: u64) {
        {
            let regen = self.pack.class.mana_regen_per_sec;
            let p = self.players.get_mut(&id).unwrap();
            p.caster.regen(now - p.last_regen, regen);
            p.last_regen = now;
        }
        loop {
            let p = self.players.get_mut(&id).unwrap();
            let entity = p.entity;
            if let Some(cast) = p.caster.cast.clone() {
                if let Some(tick_at) = cast.next_tick_at().filter(|&t| t <= now) {
                    p.caster.cast.as_mut().unwrap().ticks_done += 1;
                    self.resolve(id, cast.ability, cast.target, tick_at);
                    continue;
                }
                if cast.ends_at <= now {
                    let def = &self.pack.abilities[cast.ability as usize];
                    p.caster.complete_cast(def);
                    self.send(
                        Dest::All,
                        ServerMsg::CastStop {
                            caster: entity,
                            seq: cast.seq,
                            reason: StopReason::Completed,
                        },
                    );
                    if !cast.channel {
                        let p = &self.players[&id];
                        match p.cast_aim {
                            Some(aim) => {
                                let allowance = p.cast_allowance;
                                self.launch(
                                    id,
                                    cast.ability,
                                    cast.seq,
                                    aim,
                                    cast.ends_at,
                                    allowance,
                                );
                            }
                            None => self.resolve(id, cast.ability, cast.target, cast.ends_at),
                        }
                    }
                    continue;
                }
            }
            let p = self.players.get_mut(&id).unwrap();
            if let Some(q) = &p.queued {
                let def = &self.pack.abilities[q.ability as usize];
                let ready = p.caster.ready_at(q.ability, def);
                if ready <= now {
                    let q = p.queued.take().unwrap();
                    let t = ready.max(q.received_at);
                    self.try_start(id, q, t, Some(t), now_us);
                    continue;
                }
            }
            break;
        }
    }

    /// Damage-over-time ticks and expiry on every NPC, plus dummy resets.
    fn advance_auras(&mut self, now: Ms) {
        let ids: Vec<EntityId> = self
            .npcs
            .iter()
            .filter(|(_, n)| !n.auras.is_empty() || matches!(n.brain, Brain::Dummy))
            .map(|(id, _)| *id)
            .collect();
        for target in ids {
            let mut i = 0;
            while let Some(n) = self.npcs.get_mut(&target) {
                if i >= n.auras.len() {
                    break;
                }
                let a = &n.auras[i];
                let def = &self.pack.auras[a.aura as usize];
                if def.tick_ms > 0 && a.next_tick <= a.expires_at && a.next_tick <= now {
                    let (tick_at, source, aura, sp) =
                        (a.next_tick, a.source, a.aura, a.spell_power);
                    n.auras[i].next_tick += def.tick_ms as Ms;
                    let (amount, crit) =
                        self.roll(def.tick_base + def.tick_coeff_spellpower * sp, None);
                    let ability = self.ability_for_aura(aura);
                    self.send(
                        Dest::All,
                        ServerMsg::Hit {
                            source,
                            target,
                            ability,
                            aura: Some(aura),
                            amount,
                            crit,
                        },
                    );
                    self.damage(target, amount, source, 1.0, tick_at);
                    continue;
                }
                if a.expires_at <= now {
                    let aura = a.aura;
                    n.auras.remove(i);
                    self.send(Dest::All, ServerMsg::AuraRemoved { target, aura });
                    continue;
                }
                i += 1;
            }
            let reset = self.pack.dummy.reset_after_ms as Ms;
            if let Some(n) = self.npcs.get_mut(&target)
                && matches!(n.brain, Brain::Dummy)
                && n.auras.is_empty()
                && now >= n.last_hit + reset
            {
                n.hp = n.max_hp;
                n.threat.clear();
            }
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

    fn bodies(&self) -> HashMap<EntityId, Body> {
        let mut out = HashMap::with_capacity(self.players.len() + self.npcs.len());
        for p in self.players.values() {
            out.insert(
                p.entity,
                Body {
                    kind: EntityKind::Player,
                    pos: p.pos,
                    facing: p.facing,
                    hp: p.hp,
                    max_hp: self.pack.class.max_hp,
                    alive: p.respawn_at.is_none(),
                    party: Some(p.entity),
                    fighting: Vec::new(),
                },
            );
        }
        for n in self.npcs.values() {
            let party = match n.brain {
                Brain::Pawn { owner, .. } => Some(owner),
                _ => None,
            };
            out.insert(
                n.entity,
                Body {
                    kind: n.kind,
                    pos: n.pos,
                    facing: n.facing,
                    hp: n.hp,
                    max_hp: n.max_hp,
                    alive: n.alive(),
                    party,
                    fighting: Vec::new(),
                },
            );
        }
        // Resolve threat tables to parties in a second pass, once every
        // body's party is known.
        for n in self.npcs.values() {
            if n.threat.is_empty() {
                continue;
            }
            let mut parties: Vec<EntityId> = n
                .threat
                .iter()
                .filter_map(|(e, _)| out.get(e).and_then(|b| b.party))
                .collect();
            parties.dedup();
            out.get_mut(&n.entity).unwrap().fighting = parties;
        }
        out
    }

    /// One step of every NPC brain, plus player respawn and regeneration.
    fn tick_ai(&mut self, now: Ms) {
        let dt = AI_TICK_MS as f32 / 1000.0;
        self.tick_players(now, dt);
        let bodies = self.bodies();
        let player_targets: HashMap<EntityId, Option<EntityId>> = self
            .players
            .values()
            .map(|p| (p.entity, p.target))
            .collect();
        let ids: Vec<EntityId> = self.npcs.keys().copied().collect();
        let mut actions = Vec::new();
        for id in &ids {
            let n = self.npcs.get_mut(id).unwrap();
            let action = think(n, &self.pack, now, dt, &bodies, &player_targets);
            actions.push((*id, action));
        }
        for (id, action) in actions {
            match action {
                Action::None => {}
                Action::Attack(target) => self.swing(id, target, now, false),
                Action::Heal(target) => self.swing(id, target, now, true),
                Action::Slam(pos) => self.start_slam(id, pos, now),
            }
        }
        self.separate();
        for n in self.npcs.values_mut().filter(|n| n.kind.hostile()) {
            n.trail.push_back((now, n.pos));
            if n.trail.len() > TRAIL_LEN {
                n.trail.pop_front();
            }
        }
    }

    fn start_slam(&mut self, source: EntityId, pos: [f32; 2], now: Ms) {
        let Some(slam) = self
            .npcs
            .get(&source)
            .and_then(|n| self.pack.mobs[n.def as usize].slam.clone())
        else {
            return;
        };
        self.slams.push(Slam {
            source,
            pos,
            radius: slam.radius,
            damage: slam.damage,
            lands_at: now + slam.windup_ms as Ms,
            pawns_done: false,
            players_done: Vec::new(),
        });
        self.send(
            Dest::All,
            ServerMsg::Telegraph {
                source,
                pos,
                radius: slam.radius,
                windup_ms: slam.windup_ms,
            },
        );
    }

    fn tick_players(&mut self, now: Ms, dt: f32) {
        let c = &self.pack.class;
        let (max_hp, regen) = (c.max_hp, c.hp_regen_per_sec);
        let mut respawned = Vec::new();
        for (client, p) in self.players.iter_mut() {
            if let Some(at) = p.respawn_at {
                if now >= at {
                    p.respawn_at = None;
                    p.hp = max_hp;
                    respawned.push((*client, p.entity));
                }
                continue;
            }
            if now >= p.last_damaged + OUT_OF_COMBAT_MS {
                p.hp = (p.hp + regen * dt).min(max_hp);
            }
        }
        for (client, entity) in respawned {
            let spawn = self.spawn_point(entity);
            let p = self.players.get_mut(&client).unwrap();
            p.pos = spawn;
            p.validator.reset(now, spawn);
            println!("entity {entity} respawned");
            self.send(Dest::One(client), ServerMsg::Correction { pos: spawn });
        }
    }

    fn swing(&mut self, source: EntityId, target: EntityId, now: Ms, heal: bool) {
        let Some(n) = self.npcs.get(&source).filter(|n| n.alive()) else {
            return;
        };
        let (base, threat) = match n.kind {
            EntityKind::Mob => (self.pack.mobs[n.def as usize].attack_damage, 1.0),
            EntityKind::Pawn => {
                let d = &self.pack.pawns[n.def as usize];
                (
                    if heal { d.heal_amount } else { d.attack_damage },
                    d.threat_mult,
                )
            }
            _ => return,
        };
        let (amount, crit) = self.roll(base, Some(NPC_CRIT_CHANCE));
        self.send(
            Dest::All,
            ServerMsg::Swing {
                source,
                target,
                amount,
                crit,
                heal,
            },
        );
        if heal {
            self.heal(target, amount);
        } else {
            self.damage(target, amount, source, threat, now);
        }
    }

    fn heal(&mut self, target: EntityId, amount: f32) {
        if let Some(client) = self.player_by_entity(target) {
            let max = self.pack.class.max_hp;
            let p = self.players.get_mut(&client).unwrap();
            if p.respawn_at.is_none() {
                p.hp = (p.hp + amount).min(max);
            }
        } else if let Some(n) = self.npcs.get_mut(&target).filter(|n| n.alive()) {
            n.hp = (n.hp + amount).min(n.max_hp);
        }
    }

    /// Push overlapping NPCs apart so a pack doesn't stand in one spot.
    fn separate(&mut self) {
        let pos: Vec<(EntityId, [f32; 3])> = self
            .npcs
            .values()
            .filter(|n| n.alive() && !matches!(n.brain, Brain::Dummy))
            .map(|n| (n.entity, n.pos))
            .collect();
        for i in 0..pos.len() {
            let mut push = [0.0f32; 2];
            for j in 0..pos.len() {
                if i == j {
                    continue;
                }
                let (a, b) = (pos[i].1, pos[j].1);
                let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
                let d = (dx * dx + dz * dz).sqrt();
                if d < PERSONAL_SPACE {
                    let (ux, uz) = if d > 1e-4 {
                        (dx / d, dz / d)
                    } else {
                        // Exactly stacked: split by id so both don't pick the same side.
                        let a = (pos[i].0 as f32) * 2.399;
                        (a.cos(), a.sin())
                    };
                    let k = (PERSONAL_SPACE - d) * 0.5;
                    push[0] += ux * k;
                    push[1] += uz * k;
                }
            }
            let n = self.npcs.get_mut(&pos[i].0).unwrap();
            n.pos[0] = (n.pos[0] + push[0]).clamp(-ARENA_HALF_EXTENT, ARENA_HALF_EXTENT);
            n.pos[2] = (n.pos[2] + push[1]).clamp(-ARENA_HALF_EXTENT, ARENA_HALF_EXTENT);
        }
    }

    pub fn next_deadline(&self) -> Option<Ms> {
        let mut best: Option<Ms> = Some(self.next_ai);
        let mut take = |t: Ms| best = Some(best.map_or(t, |b: Ms| b.min(t)));
        for p in self.players.values() {
            if let Some(c) = &p.caster.cast {
                take(c.next_tick_at().unwrap_or(c.ends_at).min(c.ends_at));
            }
            if let Some(q) = &p.queued {
                take(
                    p.caster
                        .ready_at(q.ability, &self.pack.abilities[q.ability as usize]),
                );
            }
        }
        for n in self.npcs.values() {
            for a in &n.auras {
                take(a.next_tick.min(a.expires_at));
            }
        }
        for s in &self.shots {
            take((s.swept_to + SHOT_STEP_MS).min(s.flight.ends_at()));
        }
        for b in &self.blasts {
            take(b.lands_at);
        }
        for s in &self.slams {
            for p in self.players.values() {
                take(s.lands_at + (p.rtt_ms / 2).min(MAX_DODGE_GRACE_MS));
            }
            take(s.lands_at);
        }
        best
    }

    pub fn snapshot(&mut self, now: Ms) {
        let mut entities = Vec::with_capacity(self.players.len() + self.npcs.len());
        for n in self.npcs.values() {
            let owner = match n.brain {
                Brain::Pawn { owner, .. } => Some(owner),
                _ => None,
            };
            entities.push(EntitySnap {
                id: n.entity,
                kind: n.kind,
                def: n.def,
                owner,
                target: n.target,
                pos: n.pos,
                facing: n.facing,
                hp: n.hp,
                max_hp: n.max_hp,
                mana: 0.0,
                max_mana: 0.0,
                casting: None,
                auras: n
                    .auras
                    .iter()
                    .map(|a| (a.aura, a.expires_at.saturating_sub(now) as u32))
                    .collect(),
            });
        }
        for p in self.players.values() {
            entities.push(EntitySnap {
                id: p.entity,
                kind: EntityKind::Player,
                def: 0,
                owner: None,
                target: p.target,
                pos: p.pos,
                facing: p.facing,
                hp: p.hp,
                max_hp: self.pack.class.max_hp,
                mana: p.caster.mana,
                max_mana: p.caster.max_mana,
                casting: p.caster.cast.as_ref().map(|c| {
                    (
                        c.ability,
                        now.saturating_sub(c.started_at) as u32,
                        c.bar_ms as u32,
                    )
                }),
                auras: Vec::new(),
            });
        }
        self.send(
            Dest::All,
            ServerMsg::Snapshot {
                server_time: now,
                entities,
            },
        );
    }
}

/// Decide and move one NPC. Returns the attack or heal it wants to land this
/// tick; the zone applies it after every brain has run.
fn think(
    n: &mut Npc,
    pack: &ContentPack,
    now: Ms,
    dt: f32,
    bodies: &HashMap<EntityId, Body>,
    player_targets: &HashMap<EntityId, Option<EntityId>>,
) -> Action {
    match n.brain {
        Brain::Dummy => {
            patrol(n, pack, now);
            Action::None
        }
        Brain::Mob { home, evading, .. } => think_mob(n, pack, home, evading, now, dt, bodies),
        Brain::Pawn {
            owner,
            slot,
            regroup_until,
            order,
            ..
        } => {
            let (action, order) = think_pawn(
                n,
                pack,
                owner,
                slot,
                now < regroup_until,
                order,
                now,
                dt,
                bodies,
                player_targets,
            );
            if let Brain::Pawn { order: o, .. } = &mut n.brain {
                *o = order;
            }
            action
        }
    }
}

/// The training dummy walks a circle around its spot, if the pack says so.
fn patrol(n: &mut Npc, pack: &ContentPack, now: Ms) {
    let d = &pack.dummy;
    if d.patrol_radius <= 0.0 || d.patrol_speed <= 0.0 {
        return;
    }
    let lap_ms = (std::f32::consts::TAU * d.patrol_radius / d.patrol_speed * 1000.0) as Ms;
    let a = (now % lap_ms.max(1)) as f32 / lap_ms as f32 * std::f32::consts::TAU;
    n.pos = [
        d.position[0] + a.sin() * d.patrol_radius,
        0.0,
        d.position[2] + a.cos() * d.patrol_radius,
    ];
    // Walking direction is the derivative: (cos a, -sin a).
    n.facing = a.cos().atan2(-a.sin());
}

fn set_evading(n: &mut Npc, value: bool) {
    if let Brain::Mob { evading, .. } = &mut n.brain {
        *evading = value;
    }
}

fn think_mob(
    n: &mut Npc,
    pack: &ContentPack,
    home: [f32; 3],
    evading: bool,
    now: Ms,
    dt: f32,
    bodies: &HashMap<EntityId, Body>,
) -> Action {
    let def = &pack.mobs[n.def as usize];
    if let Some(died) = n.died_at {
        if now >= died + def.respawn_ms as Ms {
            n.died_at = None;
            n.hp = n.max_hp;
            n.pos = home;
            n.next_attack = 0;
            n.trail.clear();
        }
        return Action::None;
    }
    if evading {
        if step_toward(
            &mut n.pos,
            &mut n.facing,
            home,
            def.move_speed * 1.5 * dt,
            0.2,
        ) {
            set_evading(n, false);
            n.hp = n.max_hp;
        }
        return Action::None;
    }
    n.threat
        .retain(|(e, _)| bodies.get(e).is_some_and(|b| b.alive));
    if n.threat.is_empty() {
        let nearest = bodies
            .iter()
            .filter(|(_, b)| b.alive && matches!(b.kind, EntityKind::Player | EntityKind::Pawn))
            .map(|(e, b)| (*e, dist_xz(n.pos, b.pos)))
            .filter(|(_, d)| *d <= def.aggro_radius)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((e, _)) = nearest {
            n.add_threat(e, 1.0);
        }
    }
    let Some(target) = n.top_threat() else {
        n.target = None;
        step_toward(
            &mut n.pos,
            &mut n.facing,
            home,
            def.move_speed * 0.6 * dt,
            0.3,
        );
        if dist_xz(n.pos, home) < 0.5 {
            n.hp = (n.hp + n.max_hp * 0.1 * dt).min(n.max_hp);
        }
        return Action::None;
    };
    if dist_xz(n.pos, home) > def.leash_radius {
        set_evading(n, true);
        n.threat.clear();
        n.target = None;
        n.auras.clear();
        return Action::None;
    }
    n.target = Some(target);
    let tp = bodies[&target].pos;
    if now < n.winding_until {
        return Action::None; // rooted while the slam winds up
    }
    if let Some(slam) = &def.slam
        && now >= n.next_slam
        && dist_xz(n.pos, tp) <= def.attack_range + slam.radius * 0.5
    {
        n.next_slam = now + slam.cooldown_ms as Ms;
        n.winding_until = now + slam.windup_ms as Ms;
        n.next_attack = n.next_attack.max(n.winding_until + def.attack_ms as Ms / 2);
        let (dx, dz) = (tp[0] - n.pos[0], tp[2] - n.pos[2]);
        if dx.abs() + dz.abs() > 1e-3 {
            n.facing = dx.atan2(dz);
        }
        return Action::Slam(xz(tp));
    }
    engage(
        n,
        tp,
        def.attack_range,
        def.move_speed * dt,
        def.attack_ms,
        now,
    )
    .map_or(Action::None, Action::Attack)
}

/// Returns the action and the (possibly cleared) standing attack order.
#[allow(clippy::too_many_arguments)]
fn think_pawn(
    n: &mut Npc,
    pack: &ContentPack,
    owner: EntityId,
    slot: usize,
    regrouping: bool,
    mut order: Option<EntityId>,
    now: Ms,
    dt: f32,
    bodies: &HashMap<EntityId, Body>,
    player_targets: &HashMap<EntityId, Option<EntityId>>,
) -> (Action, Option<EntityId>) {
    let def = &pack.pawns[n.def as usize];
    let Some(owner_body) = bodies.get(&owner) else {
        return (Action::None, order);
    };
    if let Some(died) = n.died_at {
        if now >= died + def.respawn_ms as Ms {
            n.died_at = None;
            n.hp = n.max_hp;
            n.pos = formation_point(owner_body.pos, owner_body.facing, slot);
        }
        return (Action::None, order);
    }
    if now >= n.last_hit + OUT_OF_COMBAT_MS {
        n.hp = (n.hp + n.max_hp * 0.05 * dt).min(n.max_hp);
    }
    let alive_hostile = |e: &EntityId| bodies.get(e).is_some_and(|b| b.alive && b.kind.hostile());
    // Is this enemy fighting the party (the owner or any of their pawns)?
    let party_fight = |e: &EntityId| bodies.get(e).is_some_and(|b| b.fighting.contains(&owner));
    if order.is_some_and(|o| !alive_hostile(&o)) {
        order = None;
    }
    let follow_point = formation_point(owner_body.pos, owner_body.facing, slot);
    if dist_xz(n.pos, owner_body.pos) > PAWN_TELEPORT_DIST {
        n.pos = follow_point;
        n.target = None;
        return (Action::None, order);
    }
    if regrouping {
        n.target = None;
        step_toward(
            &mut n.pos,
            &mut n.facing,
            follow_point,
            def.move_speed * 1.2 * dt,
            0.3,
        );
        return (Action::None, order);
    }

    // Healers look after the party first.
    if def.role == PawnRole::Healer {
        let hurt = bodies
            .iter()
            .filter(|(_, b)| b.alive && b.party == Some(owner) && b.hp < b.max_hp * 0.7)
            .min_by(|a, b| (a.1.hp / a.1.max_hp).total_cmp(&(b.1.hp / b.1.max_hp)))
            .map(|(e, b)| (*e, b.pos));
        if let Some((e, pos)) = hurt {
            n.target = Some(e);
            let action = engage(
                n,
                pos,
                def.attack_range,
                def.move_speed * dt,
                def.attack_ms,
                now,
            )
            .map_or(Action::None, Action::Heal);
            return (action, order);
        }
    }

    // Standing order, else the owner's target once it is in the fight, else
    // the nearest enemy that is fighting the party.
    let target = order
        .or_else(|| {
            player_targets
                .get(&owner)
                .copied()
                .flatten()
                .filter(|t| alive_hostile(t) && party_fight(t))
        })
        .or_else(|| {
            bodies
                .iter()
                .filter(|(e, b)| {
                    b.alive
                        && b.kind == EntityKind::Mob
                        && dist_xz(b.pos, owner_body.pos) <= PAWN_ASSIST_RADIUS
                        && party_fight(e)
                })
                .min_by(|a, b| dist_xz(n.pos, a.1.pos).total_cmp(&dist_xz(n.pos, b.1.pos)))
                .map(|(e, _)| *e)
        });
    n.target = target;
    let action = match target {
        Some(t) => engage(
            n,
            bodies[&t].pos,
            def.attack_range,
            def.move_speed * dt,
            def.attack_ms,
            now,
        )
        .map_or(Action::None, Action::Attack),
        None => {
            // Catch up faster when far behind.
            let d = dist_xz(n.pos, follow_point);
            let speed = def.move_speed * if d > 6.0 { 1.4 } else { 1.0 };
            step_toward(&mut n.pos, &mut n.facing, follow_point, speed * dt, 0.3);
            Action::None
        }
    };
    (action, order)
}

/// Where pawn `slot` stands relative to its owner: behind and to the side.
/// Facing 0 looks down +Z, matching the client's models.
fn formation_point(owner: [f32; 3], facing: f32, slot: usize) -> [f32; 3] {
    let side = if slot.is_multiple_of(2) { -1.6 } else { 1.6 };
    let back = -1.6 - (slot / 2) as f32 * 1.4;
    let (s, c) = facing.sin_cos();
    // Local (side, back) rotated by facing: forward = (sin, cos), right = (cos, -sin).
    [
        owner[0] + side * c + back * s,
        0.0,
        owner[2] - side * s + back * c,
    ]
}

/// Move up to `step` metres toward `goal`, stopping `stop` short. Returns
/// true once within `stop`.
fn step_toward(pos: &mut [f32; 3], facing: &mut f32, goal: [f32; 3], step: f32, stop: f32) -> bool {
    let (dx, dz) = (goal[0] - pos[0], goal[2] - pos[2]);
    let d = (dx * dx + dz * dz).sqrt();
    if d <= stop {
        return true;
    }
    let mv = step.min(d - stop);
    pos[0] += dx / d * mv;
    pos[2] += dz / d * mv;
    pos[0] = pos[0].clamp(-ARENA_HALF_EXTENT, ARENA_HALF_EXTENT);
    pos[2] = pos[2].clamp(-ARENA_HALF_EXTENT, ARENA_HALF_EXTENT);
    *facing = dx.atan2(dz);
    d - mv <= stop
}

/// Close to `range` of `target_pos`, face it, and return the target id the
/// caller should hit when the swing timer allows.
fn engage(
    n: &mut Npc,
    target_pos: [f32; 3],
    range: f32,
    step: f32,
    attack_ms: u32,
    now: Ms,
) -> Option<EntityId> {
    let target = n.target?;
    let d = dist_xz(n.pos, target_pos);
    if d > range {
        step_toward(&mut n.pos, &mut n.facing, target_pos, step, range * 0.85);
        return None;
    }
    let (dx, dz) = (target_pos[0] - n.pos[0], target_pos[2] - n.pos[2]);
    if dx.abs() + dz.abs() > 1e-3 {
        n.facing = dx.atan2(dz);
    }
    if now >= n.next_attack {
        n.next_attack = now + attack_ms as Ms;
        return Some(target);
    }
    None
}

fn ctx_for(p: &Player, distance: Option<f32>) -> CastCtx {
    // Movement legality is checked on the client and enforced here by
    // interrupting on newer moves; the request itself is not refused for it,
    // because the stop-moving packet may still be in flight.
    CastCtx {
        distance,
        moving: false,
        target_moving: p.moving,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACK: &str = include_str!("../../../godot/content/abilities.yaml");

    fn zone() -> Zone {
        Zone::new(ContentPack::from_yaml(PACK).unwrap())
    }

    fn run(z: &mut Zone, from: Ms, to: Ms) {
        let mut t = from;
        while t <= to {
            z.advance(t, t * 1000);
            t += 10;
        }
    }

    fn first_mob(z: &Zone) -> EntityId {
        *z.npcs
            .iter()
            .find(|(_, n)| n.kind == EntityKind::Mob)
            .unwrap()
            .0
    }

    fn walk_to(z: &mut Zone, client: ClientId, pos: [f32; 3], now: Ms) {
        let p = z.players.get_mut(&client).unwrap();
        p.pos = pos;
        p.validator.reset(now, pos);
    }

    #[test]
    fn players_get_their_pawns_and_lose_them_on_leaving() {
        let mut z = zone();
        z.add_player(7, 0);
        let pawns = z.players[&7].pawns.clone();
        assert_eq!(pawns.len(), z.pack.pawn_party.len());
        assert!(pawns.iter().all(|p| z.npcs.contains_key(p)));
        z.remove_player(7);
        assert!(pawns.iter().all(|p| !z.npcs.contains_key(p)));
    }

    #[test]
    fn pawns_follow_their_owner() {
        let mut z = zone();
        z.add_player(7, 0);
        walk_to(&mut z, 7, [10.0, 0.0, 30.0], 0);
        run(&mut z, 0, 5000);
        for pe in &z.players[&7].pawns {
            let d = dist_xz(z.npcs[pe].pos, [10.0, 0.0, 30.0]);
            assert!(d < 4.0, "pawn {pe} is {d} m from its owner");
        }
    }

    #[test]
    fn mob_aggroes_attacks_and_pawns_join_in() {
        let mut z = zone();
        z.add_player(7, 0);
        let mob = first_mob(&z);
        let mp = z.npcs[&mob].pos;
        // Stand inside aggro range with the pawns.
        walk_to(&mut z, 7, [mp[0] + 4.0, 0.0, mp[2]], 0);
        for pe in z.players[&7].pawns.clone() {
            z.npcs.get_mut(&pe).unwrap().pos = [mp[0] + 5.0, 0.0, mp[2] + 1.0];
        }
        run(&mut z, 0, 6000);
        let me = z.players[&7].entity;
        let pawns = z.players[&7].pawns.clone();
        let msgs = z.drain_outbox();
        let mob_hit_us = msgs.iter().any(|(_, _, m)| {
            matches!(m, ServerMsg::Swing { source, target, heal: false, .. }
                if *source == mob && (*target == me || pawns.contains(target)))
        });
        let pawn_hit_mob = msgs.iter().any(|(_, _, m)| {
            matches!(m, ServerMsg::Swing { source, target, heal: false, .. }
                if pawns.contains(source) && z.npcs.get(target).is_some_and(|n| n.kind == EntityKind::Mob))
        });
        assert!(mob_hit_us, "mob never attacked the party");
        assert!(pawn_hit_mob, "pawns never fought back");
    }

    #[test]
    fn mob_leashes_home_and_heals() {
        let mut z = zone();
        z.add_player(7, 0);
        let mob = first_mob(&z);
        let me = z.players[&7].entity;
        let home = z.npcs[&mob].pos;
        z.npcs.get_mut(&mob).unwrap().hp -= 100.0;
        z.npcs.get_mut(&mob).unwrap().add_threat(me, 50.0);
        // Run far away, beyond the leash.
        walk_to(&mut z, 7, [home[0], 0.0, (home[2] + 55.0).min(59.0)], 0);
        run(&mut z, 0, 20_000);
        let n = &z.npcs[&mob];
        assert!(dist_xz(n.pos, home) < 3.5, "mob did not go home");
        assert_eq!(n.hp, n.max_hp);
        assert!(n.threat.is_empty());
    }

    #[test]
    fn players_die_and_respawn() {
        let mut z = zone();
        z.add_player(7, 0);
        let me = z.players[&7].entity;
        z.damage(me, 1e9, 1, 1.0, 100);
        assert!(z.players[&7].respawn_at.is_some());
        let respawn = z.pack.class.respawn_ms as Ms;
        run(&mut z, 100, 100 + respawn + 100);
        let p = &z.players[&7];
        assert!(p.respawn_at.is_none());
        assert!(p.hp > 0.0);
    }

    #[test]
    fn mobs_die_and_respawn() {
        let mut z = zone();
        z.add_player(7, 0);
        let me = z.players[&7].entity;
        let mob = first_mob(&z);
        z.damage(mob, 1e9, me, 1.0, 0);
        assert!(!z.npcs[&mob].alive());
        let respawn = z.pack.mobs[z.npcs[&mob].def as usize].respawn_ms as Ms;
        run(&mut z, 0, respawn + 100);
        assert!(z.npcs[&mob].alive());
    }
    fn ability(z: &Zone, id: &str) -> AbilityId {
        z.pack.abilities.iter().position(|a| a.id == id).unwrap() as AbilityId
    }

    fn cast(
        z: &mut Zone,
        client: ClientId,
        ability: AbilityId,
        aim: Option<[f32; 2]>,
        now: Ms,
        rtt: Ms,
    ) {
        let msg = ClientMsg::Cast {
            seq: 1,
            ability,
            target: 0,
            queued: false,
            move_seq: 0,
            aim,
        };
        z.handle(client, msg, now, now * 1000, rtt);
    }

    /// A player with no pawns, standing `dist` m south of `mob`.
    fn lone_player_near(z: &mut Zone, mob: EntityId, dist: f32) -> [f32; 3] {
        z.add_player(7, 0);
        for pe in std::mem::take(&mut z.players.get_mut(&7).unwrap().pawns) {
            z.npcs.remove(&pe);
        }
        let mp = z.npcs[&mob].pos;
        let at = [mp[0], 0.0, mp[2] + dist];
        walk_to(z, 7, at, 0);
        at
    }

    fn shot_verdicts(msgs: &[(Dest, u8, ServerMsg)]) -> Vec<Option<EntityId>> {
        msgs.iter()
            .filter_map(|(_, _, m)| match m {
                ServerMsg::ProjectileEnd { hit, .. } => Some(*hit),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn skillshot_hits_what_it_is_aimed_at_and_misses_otherwise() {
        let mut z = zone();
        let mob = first_mob(&z);
        let mp = z.npcs[&mob].pos;
        lone_player_near(&mut z, mob, 15.0);
        run(&mut z, 0, 200);
        let spike = ability(&z, "glacial_spike");
        cast(&mut z, 7, spike, Some([mp[0], mp[2]]), 210, 0);
        run(&mut z, 210, 2000);
        let msgs = z.drain_outbox();
        assert_eq!(shot_verdicts(&msgs), vec![Some(mob)]);
        assert!(
            msgs.iter()
                .any(|(_, _, m)| matches!(m, ServerMsg::Hit { target, .. } if *target == mob))
        );

        // Aimed 90 degrees off: flies along empty ground and gives up.
        let me = z.players[&7].pos;
        cast(&mut z, 7, spike, Some([me[0] + 10.0, me[2]]), 5000, 0);
        run(&mut z, 5000, 7000);
        assert_eq!(shot_verdicts(&z.drain_outbox()), vec![None]);
    }

    #[test]
    fn skillshot_without_aim_fires_at_the_target() {
        let mut z = zone();
        let mob = first_mob(&z);
        lone_player_near(&mut z, mob, 12.0);
        run(&mut z, 0, 200);
        let spike = ability(&z, "glacial_spike");
        let msg = ClientMsg::Cast {
            seq: 1,
            ability: spike,
            target: mob,
            queued: false,
            move_seq: 0,
            aim: None,
        };
        z.handle(7, msg, 210, 210_000, 0);
        run(&mut z, 210, 2000);
        assert_eq!(shot_verdicts(&z.drain_outbox()), vec![Some(mob)]);
    }

    #[test]
    fn rewound_positions_interpolate_the_trail() {
        let mut z = zone();
        let mob = first_mob(&z);
        let n = z.npcs.get_mut(&mob).unwrap();
        n.trail = VecDeque::from(vec![(1000, [0.0, 0.0, 0.0]), (1050, [1.0, 0.0, 0.0])]);
        n.pos = [1.0, 0.0, 0.0];
        assert_eq!(n.pos_at(900), [0.0, 0.0, 0.0]);
        assert!((n.pos_at(1025)[0] - 0.5).abs() < 1e-5);
        assert_eq!(n.pos_at(2000), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn ground_blast_hits_the_camp_up_to_its_cap() {
        let mut z = zone();
        let mob = first_mob(&z);
        let centre = z.pack.camps[0].position;
        lone_player_near(&mut z, mob, 14.0);
        run(&mut z, 0, 200);
        let rune = ability(&z, "flame_rune");
        z.pack.abilities[rune as usize].max_targets = 2;
        cast(&mut z, 7, rune, Some([centre[0], centre[2]]), 210, 0);
        run(&mut z, 210, 1500);
        let hits = z
            .drain_outbox()
            .iter()
            .filter(|(_, _, m)| matches!(m, ServerMsg::Hit { ability, .. } if *ability == rune))
            .count();
        assert_eq!(hits, 2, "three ghouls in the circle, capped at two");
    }

    #[test]
    fn ground_blast_needs_an_aim_or_a_target() {
        let mut z = zone();
        let mob = first_mob(&z);
        lone_player_near(&mut z, mob, 14.0);
        let rune = ability(&z, "flame_rune");
        cast(&mut z, 7, rune, None, 100, 0);
        assert!(z.drain_outbox().iter().any(|(_, _, m)| matches!(
            m,
            ServerMsg::CastFailed {
                reason: FailReason::NoTarget,
                ..
            }
        )));
    }

    /// Pull the brute, wait for its slam, then step out `step_out_after` ms
    /// after the slam lands on the server (None = stay in). Returns whether
    /// the slam hit us.
    fn slam_test(rtt: Ms, step_out_after: Option<Ms>) -> bool {
        let mut z = zone();
        let brute = *z
            .npcs
            .iter()
            .find(|(_, n)| n.kind == EntityKind::Mob && z.pack.mobs[n.def as usize].slam.is_some())
            .unwrap()
            .0;
        lone_player_near(&mut z, brute, 1.5);
        z.players.get_mut(&7).unwrap().rtt_ms = rtt;
        let damage = z.pack.mobs[z.npcs[&brute].def as usize]
            .slam
            .clone()
            .unwrap()
            .damage;
        let me = z.players[&7].entity;
        let mut lands_at = None;
        let mut t = 0;
        while t < 6000 {
            z.advance(t, t * 1000);
            // Keep the player planted where the brute can reach.
            for (_, _, m) in z.drain_outbox() {
                match m {
                    ServerMsg::Telegraph { windup_ms, .. } if lands_at.is_none() => {
                        lands_at = Some(t + windup_ms as Ms);
                    }
                    ServerMsg::Swing {
                        source,
                        target,
                        amount,
                        ..
                    } if source == brute && target == me && amount == damage => {
                        return true;
                    }
                    _ => {}
                }
            }
            if let (Some(l), Some(after)) = (lands_at, step_out_after)
                && t >= l + after
            {
                let p = z.players[&7].pos;
                walk_to(&mut z, 7, [p[0] + 12.0, 0.0, p[2]], t);
            }
            if lands_at.is_some_and(|l| t > l + 500) {
                return false;
            }
            t += 5;
        }
        panic!("the brute never slammed");
    }

    #[test]
    fn slam_hits_players_who_stay_in_the_circle() {
        assert!(slam_test(100, None));
    }

    #[test]
    fn slam_misses_a_dodge_inside_the_latency_grace() {
        // 100 ms RTT: the server waits 50 ms for this player's newest move.
        assert!(!slam_test(100, Some(30)));
    }

    #[test]
    fn slam_hits_a_dodge_that_came_too_late() {
        assert!(slam_test(100, Some(90)));
    }

    #[test]
    fn dash_lets_the_player_move_faster_than_walking() {
        let mut z = zone();
        z.add_player(7, 0);
        let start = z.players[&7].pos;
        let far = [start[0] + 6.5, 0.0, start[2]];
        let moved = |z: &mut Zone, seq: u32, t: Ms| {
            let msg = ClientMsg::Move {
                seq,
                pos: far,
                facing: 0.0,
                moving: true,
            };
            z.handle(7, msg, t, t * 1000, 0);
            z.players[&7].pos == far
        };
        assert!(
            !moved(&mut z, 1, 200),
            "6.5 m in 200 ms is too fast on foot"
        );
        let step = ability(&z, "blink_step");
        cast(&mut z, 7, step, Some([far[0], far[2]]), 300, 0);
        assert!(moved(&mut z, 2, 400), "a granted dash covers it");
    }
}

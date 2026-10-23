//! Authoritative zone state. Combat outcomes are decided only here.

use std::collections::HashMap;

use combat_core::movement::{MoveValidator, dist_xz};
use combat_core::protocol::{ClientMsg, EntityKind, EntitySnap, ServerMsg, StopReason};
use combat_core::{
    AbilityId, AuraId, CastCtx, CasterState, ContentPack, Effect, EntityId, FailReason, Ms,
    Started, channel,
};
use renet::ClientId;

pub enum Dest {
    All,
    One(ClientId),
}

struct Queued {
    seq: u32,
    ability: AbilityId,
    target: EntityId,
    move_seq: u32,
    received_at: Ms,
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
}

struct AuraInstance {
    aura: AuraId,
    source: EntityId,
    spell_power: f32,
    expires_at: Ms,
    next_tick: Ms,
}

struct Dummy {
    entity: EntityId,
    pos: [f32; 3],
    hp: f32,
    max_hp: f32,
    last_hit: Ms,
    auras: Vec<AuraInstance>,
}

#[derive(Default)]
struct QueueStats {
    count: u32,
    total_us: u64,
    max_us: u64,
}

pub struct Zone {
    pack: ContentPack,
    players: HashMap<ClientId, Player>,
    dummy: Dummy,
    next_entity: EntityId,
    outbox: Vec<(Dest, u8, ServerMsg)>,
    rng: u64,
    stats: QueueStats,
    last_stats_print: Ms,
}

impl Zone {
    pub fn new(pack: ContentPack) -> Self {
        let dummy = Dummy {
            entity: 1,
            pos: pack.dummy.position,
            hp: pack.dummy.max_hp,
            max_hp: pack.dummy.max_hp,
            last_hit: 0,
            auras: Vec::new(),
        };
        Self {
            pack,
            players: HashMap::new(),
            dummy,
            next_entity: 100,
            outbox: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
            stats: QueueStats::default(),
            last_stats_print: 0,
        }
    }

    pub fn add_player(&mut self, client_id: ClientId, now: Ms) {
        let entity = self.next_entity;
        self.next_entity += 1;
        let spawn = [((entity % 8) as f32 - 4.0) * 1.5, 0.0, 0.0];
        let mut validator = MoveValidator::default();
        validator.reset(now, spawn);
        let c = &self.pack.class;
        self.players.insert(
            client_id,
            Player {
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
            },
        );
        println!("client {client_id} joined as entity {entity}");
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
        self.players.remove(&client_id);
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

    fn target_distance(&self, from: [f32; 3], target: EntityId) -> Option<f32> {
        if target == self.dummy.entity {
            return Some(dist_xz(from, self.dummy.pos));
        }
        None
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
        let dist = {
            let p = &self.players[&client_id];
            self.target_distance(p.pos, q.target)
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
            self.resolve(client_id, q.ability, q.target, t, false);
        }
    }

    /// Apply an ability's effects. `tick_only` is used for channel ticks.
    fn resolve(
        &mut self,
        client_id: ClientId,
        ability: AbilityId,
        target: EntityId,
        t: Ms,
        _tick_only: bool,
    ) {
        if target != self.dummy.entity {
            return;
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
                    let (amount, crit) = self.roll(base + coeff_spellpower * sp);
                    self.damage_dummy(amount, t);
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
                    self.dummy
                        .auras
                        .retain(|a| !(a.aura == aura_id && a.source == source));
                    self.dummy.auras.push(inst);
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

    fn roll(&mut self, base: f32) -> (f32, bool) {
        let variance = 0.95 + 0.1 * self.rand01();
        let crit = self.rand01() < self.pack.class.crit_chance;
        (
            (base * variance * if crit { 1.5 } else { 1.0 }).round(),
            crit,
        )
    }

    fn rand01(&mut self) -> f32 {
        // xorshift64*: deterministic and dependency-free. Fine for a dummy.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn damage_dummy(&mut self, amount: f32, t: Ms) {
        self.dummy.hp -= amount;
        if self.dummy.hp <= 0.0 {
            self.dummy.hp = self.dummy.max_hp; // training dummies never die
        }
        self.dummy.last_hit = t;
    }

    /// Process every combat timer that has fallen due, each at its own
    /// logical timestamp, so nothing is rounded to a tick boundary.
    pub fn advance(&mut self, now: Ms, now_us: u64) {
        let ids: Vec<ClientId> = self.players.keys().copied().collect();
        for id in ids {
            self.advance_player(id, now, now_us);
        }
        self.advance_dummy(now);
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
                    self.resolve(id, cast.ability, cast.target, tick_at, true);
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
                        self.resolve(id, cast.ability, cast.target, cast.ends_at, false);
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

    fn advance_dummy(&mut self, now: Ms) {
        let target = self.dummy.entity;
        let mut i = 0;
        while i < self.dummy.auras.len() {
            let a = &self.dummy.auras[i];
            let def = &self.pack.auras[a.aura as usize];
            if def.tick_ms > 0 && a.next_tick <= a.expires_at && a.next_tick <= now {
                let (tick_at, source, aura, sp) = (a.next_tick, a.source, a.aura, a.spell_power);
                self.dummy.auras[i].next_tick += def.tick_ms as Ms;
                let (amount, crit) = self.roll(def.tick_base + def.tick_coeff_spellpower * sp);
                self.damage_dummy(amount, tick_at);
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
                continue;
            }
            if a.expires_at <= now {
                let aura = a.aura;
                self.dummy.auras.remove(i);
                self.send(Dest::All, ServerMsg::AuraRemoved { target, aura });
                continue;
            }
            i += 1;
        }
        let reset = self.pack.dummy.reset_after_ms as Ms;
        if self.dummy.hp < self.dummy.max_hp
            && self.dummy.auras.is_empty()
            && now >= self.dummy.last_hit + reset
        {
            self.dummy.hp = self.dummy.max_hp;
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
        for a in &self.dummy.auras {
            take(a.next_tick.min(a.expires_at));
        }
        best
    }

    pub fn snapshot(&mut self, now: Ms) {
        let mut entities = Vec::with_capacity(self.players.len() + 1);
        let d = &self.dummy;
        entities.push(EntitySnap {
            id: d.entity,
            kind: EntityKind::Dummy,
            pos: d.pos,
            facing: 0.0,
            hp: d.hp,
            max_hp: d.max_hp,
            mana: 0.0,
            max_mana: 0.0,
            casting: None,
            auras: d
                .auras
                .iter()
                .map(|a| (a.aura, a.expires_at.saturating_sub(now) as u32))
                .collect(),
        });
        for p in self.players.values() {
            entities.push(EntitySnap {
                id: p.entity,
                kind: EntityKind::Player,
                pos: p.pos,
                facing: p.facing,
                hp: self.pack.class.max_hp,
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

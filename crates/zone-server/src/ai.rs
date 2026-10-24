//! Mob AI and other 20 Hz upkeep: regeneration, respawns, threat, chasing,
//! melee swings and mob spellcasting through the same rules players use.

use combat_core::movement::dist_xz;
use combat_core::protocol::StopReason;
use combat_core::{AbilityId, CastCtx, EntityId, EntityKind, Ms};

use crate::zone::{COMBAT_TIMEOUT_MS, MobState, Queued, Zone, face};

/// AI step length; matches the snapshot rate.
pub const AI_DT_MS: Ms = 50;

impl Zone {
    pub fn tick_ai(&mut self, now: Ms, now_us: u64) {
        self.tick += 1;
        let ids: Vec<EntityId> = self.entities.keys().copied().collect();
        for id in ids {
            let Some(e) = self.entities.get(&id) else {
                continue;
            };
            if e.kind == EntityKind::Player {
                self.regen_player(id, now);
            } else {
                self.think(id, now, now_us);
            }
        }
    }

    fn regen_player(&mut self, id: EntityId, now: Ms) {
        let pack = self.pack.clone();
        let e = self.entities.get_mut(&id).unwrap();
        let dt = now.saturating_sub(e.last_regen);
        e.last_regen = now;
        if e.dead {
            return;
        }
        let class = &pack.classes[e.def as usize];
        e.caster.regen(dt, class.mana_regen_per_sec);
        if !e.in_combat(now) {
            e.hp = (e.hp + class.hp_regen_per_sec_ooc * dt as f32 / 1000.0).min(e.stats.max_hp);
        }
    }

    fn think(&mut self, id: EntityId, now: Ms, now_us: u64) {
        let pack = self.pack.clone();
        let e = self.entities.get_mut(&id).unwrap();
        let def = &pack.mobs[e.def as usize];
        let dt = now.saturating_sub(e.last_regen);
        e.last_regen = now;
        e.caster.regen(dt, def.mana / 60.0);
        let m = e.mob.as_mut().unwrap();

        if e.dead {
            if now >= m.respawn_at {
                let home = m.home;
                m.state = MobState::Idle;
                m.threat.clear();
                e.hp = e.stats.max_hp;
                e.caster.mana = e.caster.max_mana;
                self.revive(id, home, 1.0, now);
            }
            return;
        }

        if e.passive {
            if e.hp < e.stats.max_hp
                && e.auras.is_empty()
                && now >= m.last_hit + def.reset_after_ms as Ms
            {
                e.hp = e.stats.max_hp;
            }
            return;
        }

        match m.state {
            MobState::Idle => self.idle(id, now),
            MobState::Combat => self.fight(id, now, now_us),
            MobState::Evade => self.evade(id, now),
        }
    }

    fn idle(&mut self, id: EntityId, now: Ms) {
        let pack = self.pack.clone();
        let e = &self.entities[&id];
        let def = &pack.mobs[e.def as usize];
        // Aggro: the nearest living player in radius and in sight.
        if def.aggro_radius > 0.0 {
            let mut best: Option<(EntityId, f32)> = None;
            for cell in self.grid.neighbors(e.cell) {
                for &other in &self.grid.cells[cell] {
                    let o = &self.entities[&other];
                    if o.kind != EntityKind::Player || o.dead {
                        continue;
                    }
                    let d = dist_xz(e.pos, o.pos);
                    if d <= def.aggro_radius
                        && best.is_none_or(|b| d < b.1)
                        && self.geo.los(e.pos, o.pos)
                    {
                        best = Some((other, d));
                    }
                }
            }
            if let Some((target, _)) = best {
                let e = self.entities.get_mut(&id).unwrap();
                let m = e.mob.as_mut().unwrap();
                m.threat.push((target, 1.0));
                m.state = MobState::Combat;
                m.wander_to = None;
                e.moving = false;
                e.combat_until = now + COMBAT_TIMEOUT_MS;
                return;
            }
        }
        // Wander around home.
        if def.wander_radius <= 0.0 {
            return;
        }
        let r1 = self.rand01();
        let r2 = self.rand01();
        let r3 = self.rand01();
        let e = self.entities.get_mut(&id).unwrap();
        let m = e.mob.as_mut().unwrap();
        match m.wander_to {
            None if now >= m.next_wander => {
                let a = r1 * std::f32::consts::TAU;
                let r = def.wander_radius * r2.sqrt();
                m.wander_to = Some([m.home[0] + a.cos() * r, m.home[1], m.home[2] + a.sin() * r]);
            }
            Some(to) => {
                let speed = def.move_speed * 0.35;
                if self.step_towards(id, to, speed, 0.3) {
                    let e = self.entities.get_mut(&id).unwrap();
                    let m = e.mob.as_mut().unwrap();
                    m.wander_to = None;
                    m.next_wander = now + 4000 + (r3 * 8000.0) as Ms;
                    e.moving = false;
                }
            }
            None => {}
        }
    }

    fn fight(&mut self, id: EntityId, now: Ms, now_us: u64) {
        let pack = self.pack.clone();
        // Drop threat on targets that died, left, or ran out of range.
        let target = {
            let e = &self.entities[&id];
            let def = &pack.mobs[e.def as usize];
            let m = e.mob.as_ref().unwrap();
            let alive: Vec<(EntityId, f32)> = m
                .threat
                .iter()
                .copied()
                .filter(|(t, _)| {
                    self.entities
                        .get(t)
                        .is_some_and(|o| !o.dead && dist_xz(o.pos, m.home) < def.leash_radius * 2.0)
                })
                .collect();
            let leashed = dist_xz(e.pos, m.home) > def.leash_radius;
            let e = self.entities.get_mut(&id).unwrap();
            let m = e.mob.as_mut().unwrap();
            m.threat = alive;
            if leashed || m.threat.is_empty() {
                None
            } else {
                m.top_threat()
            }
        };
        let Some(target) = target else {
            self.start_evade(id);
            return;
        };
        let e = &self.entities[&id];
        let def = &pack.mobs[e.def as usize];
        let tpos = self.entities[&target].pos;
        let dist = dist_xz(e.pos, tpos);
        let los = self.geo.los(e.pos, tpos);
        if let Some(e) = self.entities.get_mut(&id) {
            e.combat_until = now + COMBAT_TIMEOUT_MS;
        }
        if self.entities[&id].caster.cast.is_some() {
            return; // finishing a cast; the timer resolves it
        }

        // Spells first, by priority, using the shared cast rules.
        let abilities: Vec<AbilityId> = def
            .abilities
            .iter()
            .map(|a| pack.ability_index(a).unwrap())
            .collect();
        for ab in abilities {
            let adef = &pack.abilities[ab as usize];
            let e = &self.entities[&id];
            let ctx = CastCtx {
                distance: Some(dist),
                moving: false,
                target_moving: false,
                los,
                dead: false,
            };
            if e.caster.check(ab, adef, now, ctx).is_ok() {
                let e = self.entities.get_mut(&id).unwrap();
                e.moving = false;
                let m = e.mob.as_mut().unwrap();
                m.cast_seq += 1;
                let q = Queued {
                    seq: m.cast_seq,
                    ability: ab,
                    target,
                    move_seq: 0,
                    received_at: now,
                };
                if self.try_start(id, q, now, None, now_us) {
                    return;
                }
            }
        }

        // Melee, or close the distance.
        let has_ranged = def.abilities.iter().any(|a| {
            let ad = &pack.abilities[pack.ability_index(a).unwrap() as usize];
            ad.range > def.melee_range + 1.0
        });
        let want = if has_ranged && los && dist <= 28.0 {
            dist // casters hold position while their spell recharges
        } else {
            def.melee_range * 0.8
        };
        if dist > want.max(def.melee_range * 0.8) + 0.05 || !los {
            self.step_towards(id, tpos, def.move_speed, def.melee_range * 0.8);
            return;
        }
        let e = self.entities.get_mut(&id).unwrap();
        e.moving = false;
        e.facing = face(e.pos, tpos, e.facing);
        let m = e.mob.as_mut().unwrap();
        if dist <= def.melee_range && now >= m.next_attack && def.melee_damage > 0.0 {
            m.next_attack = now + def.attack_ms as Ms;
            let (amount, crit) = self.roll(def.melee_damage, 0.05);
            // Ability id 0 with no aura reads as "melee" on the client.
            self.damage(id, target, amount, u16::MAX, None, crit, now);
        }
    }

    fn start_evade(&mut self, id: EntityId) {
        let e = self.entities.get_mut(&id).unwrap();
        if e.caster.cast.is_some() {
            self.stop_cast(id, StopReason::Interrupted);
        }
        let e = self.entities.get_mut(&id).unwrap();
        e.auras.clear();
        let m = e.mob.as_mut().unwrap();
        m.state = MobState::Evade;
        m.threat.clear();
    }

    fn evade(&mut self, id: EntityId, now: Ms) {
        let pack = self.pack.clone();
        let e = &self.entities[&id];
        let def = &pack.mobs[e.def as usize];
        let home = e.mob.as_ref().unwrap().home;
        if self.step_towards(id, home, def.move_speed * 1.4, 0.3) {
            let e = self.entities.get_mut(&id).unwrap();
            e.hp = e.stats.max_hp;
            e.moving = false;
            e.combat_until = 0;
            let m = e.mob.as_mut().unwrap();
            m.state = MobState::Idle;
            m.next_wander = now + 3000;
        }
    }

    /// Walk one AI step towards `to`, sliding along walls. Returns true once
    /// within `stop_at` metres.
    fn step_towards(&mut self, id: EntityId, to: [f32; 3], speed: f32, stop_at: f32) -> bool {
        let e = self.entities.get_mut(&id).unwrap();
        let (dx, dz) = (to[0] - e.pos[0], to[2] - e.pos[2]);
        let d = (dx * dx + dz * dz).sqrt();
        if d <= stop_at + 0.01 {
            e.moving = false;
            return true;
        }
        let step = (speed * AI_DT_MS as f32 / 1000.0).min(d - stop_at);
        let next = self.geo.slide(e.pos, [dx / d * step, dz / d * step]);
        e.facing = face(e.pos, to, e.facing);
        e.moving = next != e.pos;
        e.pos = next;
        self.relocate(id);
        false
    }
}

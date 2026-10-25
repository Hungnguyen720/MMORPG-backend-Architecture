//! A simple player brain for load-test bots: wander, pick fights with the
//! nearest mob, play the rotation through the spell queue, release when
//! dead. Speaks only the real client API, so bots exercise the same code
//! paths as a human on the Godot client.

use combat_core::codec::flags;
use combat_core::movement::dist_xz;
use combat_core::{EntityId, Ms};

use crate::{ClientEvent, ClientSession};

#[derive(Debug, Default, Clone)]
pub struct BotStats {
    pub casts_confirmed: u32,
    pub queue_gaps_us: Vec<u32>,
    pub damage: f64,
    pub kills: u32,
    pub deaths: u32,
    pub errors: u32,
    pub corrections: u32,
    pub potions: u32,
    pub xp: u64,
    pub items_looted: u32,
    pub zone_changes: u32,
}

pub struct Brain {
    pub home: [f32; 3],
    pub roam: f32,
    goal: Option<[f32; 3]>,
    next_goal: Ms,
    rng: u64,
    rotation_i: usize,
    pressed_at: Ms,
    dead_since: Option<Ms>,
    last_tick: Ms,
    /// If false, the bot only walks around (pure movement load).
    pub fight: bool,
    pub stats: BotStats,
    /// Every event the brain saw this frame, for callers that care.
    pub seen: Vec<ClientEvent>,
    next_potion: Ms,
}

impl Brain {
    pub fn new(home: [f32; 3], roam: f32, seed: u64) -> Self {
        Self {
            home,
            roam,
            goal: None,
            next_goal: 0,
            rng: seed | 1,
            rotation_i: 0,
            pressed_at: 0,
            dead_since: None,
            last_tick: 0,
            fight: true,
            stats: BotStats::default(),
            seen: Vec::new(),
            next_potion: 0,
        }
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Advance one frame. Call after `session.update()`.
    pub fn tick(&mut self, s: &mut ClientSession) {
        let now = s.now();
        let dt = if self.last_tick == 0 {
            0.0
        } else {
            (now - self.last_tick).min(100) as f32 / 1000.0
        };
        self.last_tick = now;
        if let Some(p) = s.take_correction() {
            s.pos = p;
            self.stats.corrections += 1;
        }
        self.seen.clear();
        for e in s.take_events() {
            match &e {
                ClientEvent::Confirmed { queue_gap_us, .. } => {
                    self.stats.casts_confirmed += 1;
                    if let Some(g) = queue_gap_us {
                        self.stats.queue_gaps_us.push(*g);
                    }
                }
                ClientEvent::Hit {
                    amount,
                    mine: true,
                    heal: false,
                    ..
                } => self.stats.damage += *amount as f64,
                ClientEvent::Xp { gained, .. } => self.stats.xp += *gained as u64,
                ClientEvent::Looted { items } => {
                    self.stats.items_looted += items.iter().map(|i| i.1).sum::<u32>()
                }
                ClientEvent::ZoneChange { .. } => self.stats.zone_changes += 1,
                ClientEvent::Died { entity, .. } => {
                    if Some(*entity) == s.me {
                        self.stats.deaths += 1;
                    } else {
                        self.stats.kills += 1;
                    }
                }
                ClientEvent::Error(_) => self.stats.errors += 1,
                _ => {}
            }
            self.seen.push(e);
        }
        if !s.is_connected() {
            return;
        }
        if s.dead() {
            let since = *self.dead_since.get_or_insert(now);
            if now > since + 2000 {
                s.release();
                self.dead_since = Some(now);
            }
            return;
        }
        self.dead_since = None;
        self.maybe_potion(s, now);

        if self.fight {
            if s.target
                .is_none_or(|t| !s.is_hostile(t) || s.entities[&t].has_flag(flags::PASSIVE))
            {
                s.target = s.next_target(35.0, false);
            }
            if let Some(t) = s.target
                && let Some(tp) = s.entity_pos(t)
            {
                let d = dist_xz(s.pos, tp);
                if d > 26.0 || !s.geo.los(s.pos, tp) {
                    self.walk_towards(s, tp, dt);
                } else {
                    s.set_local_move(s.pos, s.facing, false);
                    self.cast(s, t, now);
                }
                return;
            }
        }
        // Roam.
        if self.goal.is_none() && now >= self.next_goal {
            let a = self.rand() * std::f32::consts::TAU;
            let r = self.roam * self.rand().sqrt();
            self.goal =
                Some(
                    s.geo
                        .clamp([self.home[0] + a.cos() * r, 0.0, self.home[2] + a.sin() * r]),
                );
        }
        match self.goal {
            Some(g) if dist_xz(s.pos, g) > 1.0 => {
                let before = s.pos;
                self.walk_towards(s, g, dt);
                if dt > 0.0 && dist_xz(before, s.pos) < 0.001 {
                    self.goal = None; // stuck on a wall: pick another goal
                }
            }
            Some(_) => {
                self.goal = None;
                self.next_goal = now + 1000 + (self.rand() * 4000.0) as Ms;
                s.set_local_move(s.pos, s.facing, false);
            }
            None => s.set_local_move(s.pos, s.facing, false),
        }
    }

    /// Drink a healing potion below 35 % health, like a careful player.
    fn maybe_potion(&mut self, s: &mut ClientSession, now: Ms) {
        if now < self.next_potion {
            return;
        }
        let Some(st) = s.my_state() else {
            return;
        };
        if st.max_hp == 0 || (st.hp as f32) > st.max_hp as f32 * 0.35 {
            return;
        }
        let potion = s.bag.iter().position(|slot| {
            slot.is_some_and(|(item, _)| {
                let def = &s.pack.items[item as usize];
                def.on_use
                    .iter()
                    .any(|e| matches!(e, combat_core::Effect::Heal { .. }))
            })
        });
        if let Some(slot) = potion {
            s.use_item(slot);
            self.stats.potions += 1;
            self.next_potion = now + 31_000;
        } else {
            self.next_potion = now + 5_000;
        }
    }

    pub fn walk_towards(&mut self, s: &mut ClientSession, to: [f32; 3], dt: f32) {
        let (dx, dz) = (to[0] - s.pos[0], to[2] - s.pos[2]);
        let d = (dx * dx + dz * dz).sqrt().max(0.001);
        let step = (s.move_speed() * dt).min(d);
        s.walk([dx / d * step, dz / d * step]);
    }

    fn cast(&mut self, s: &mut ClientSession, _target: EntityId, now: Ms) {
        // Same rotation as the Phase 0 bot: lean on the spell queue.
        let n = s.bar.len();
        let rotation: Vec<usize> = [3usize, 0, 1, 2, 0, 1]
            .into_iter()
            .filter(|i| *i < n)
            .collect();
        if rotation.is_empty() {
            return;
        }
        let next = rotation[self.rotation_i % rotation.len()];
        let ab = s.bar[next];
        let def = &s.pack.abilities[ab as usize];
        let ready = s.caster.ready_at(ab, def);
        if s.queued_slot().is_none() && now + 250 >= ready && now >= self.pressed_at + 150 {
            s.press(next);
            self.pressed_at = now;
            self.rotation_i += 1;
        }
        if n > 4 && s.swipe(4) == 0.0 && s.caster.cast.is_none() && s.queued_slot().is_none() {
            s.press(4);
        }
    }
}

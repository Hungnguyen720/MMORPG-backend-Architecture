//! Per-caster timing rules: GCD, cooldowns, casts, channels, spell queue.
//! Pure functions of (state, ability, time), so the client can predict with
//! the same code the server uses to decide.

use serde::{Deserialize, Serialize};

use crate::content::AbilityDef;
use crate::{AbilityId, EntityId, MAX_LATENCY_ALLOWANCE_MS, Ms, QUEUE_WINDOW_MS, RANGE_LEEWAY};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailReason {
    NotReady,
    OnCooldown,
    Casting,
    NoMana,
    OutOfRange,
    NoTarget,
    Moving,
    UnknownAbility,
    LineOfSight,
    InvalidTarget,
    Dead,
}

impl FailReason {
    pub fn text(self) -> &'static str {
        match self {
            FailReason::NotReady => "Ability is not ready yet",
            FailReason::OnCooldown => "Ability is not ready yet",
            FailReason::Casting => "Already casting",
            FailReason::NoMana => "Not enough mana",
            FailReason::OutOfRange => "Out of range",
            FailReason::NoTarget => "You have no target",
            FailReason::Moving => "Can't do that while moving",
            FailReason::UnknownAbility => "Unknown ability",
            FailReason::LineOfSight => "Target not in line of sight",
            FailReason::InvalidTarget => "Invalid target",
            FailReason::Dead => "You are dead",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveCast {
    pub seq: u32,
    pub ability: AbilityId,
    pub target: EntityId,
    pub started_at: Ms,
    pub ends_at: Ms,
    /// Nominal bar length the player sees; `ends_at` may be earlier.
    pub bar_ms: Ms,
    pub channel: bool,
    pub ticks_done: u32,
    pub ticks_total: u32,
    pub tick_interval: Ms,
}

impl ActiveCast {
    pub fn next_tick_at(&self) -> Option<Ms> {
        (self.channel && self.ticks_done < self.ticks_total)
            .then(|| self.started_at + self.tick_interval * (self.ticks_done as Ms + 1))
    }
}

/// What the caster knows about the world when it tries to cast.
#[derive(Debug, Clone, Copy)]
pub struct CastCtx {
    /// Distance to the target, or None without a valid target.
    pub distance: Option<f32>,
    pub moving: bool,
    pub target_moving: bool,
    /// Line of sight to the target (always true for self-cast abilities).
    pub los: bool,
    pub dead: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// Resolve effects now.
    Instant,
    /// A cast or channel bar is running.
    Bar,
}

#[derive(Debug, Clone)]
pub struct CasterState {
    pub gcd_start: Ms,
    pub gcd_until: Ms,
    /// (started, ready_at) per ability.
    pub cooldowns: Vec<(Ms, Ms)>,
    pub cast: Option<ActiveCast>,
    /// When the last cast finished. Lets a queued ability start at that exact
    /// moment even if the loop notices a little later.
    pub last_cast_end: Ms,
    pub mana: f32,
    pub max_mana: f32,
}

impl CasterState {
    pub fn new(ability_count: usize, max_mana: f32) -> Self {
        Self {
            gcd_start: 0,
            gcd_until: 0,
            cooldowns: vec![(0, 0); ability_count],
            cast: None,
            last_cast_end: 0,
            mana: max_mana,
            max_mana,
        }
    }

    /// Earliest time this ability could start, ignoring mana and range.
    pub fn ready_at(&self, id: AbilityId, def: &AbilityDef) -> Ms {
        let mut t = self.cooldowns[id as usize].1.max(self.last_cast_end);
        if def.on_gcd() {
            t = t.max(self.gcd_until);
        }
        if let Some(c) = &self.cast {
            t = t.max(c.ends_at);
        }
        t
    }

    pub fn check(
        &self,
        id: AbilityId,
        def: &AbilityDef,
        now: Ms,
        ctx: CastCtx,
    ) -> Result<(), FailReason> {
        if ctx.dead {
            return Err(FailReason::Dead);
        }
        if self.ready_at(id, def) > now {
            return Err(if self.cast.as_ref().is_some_and(|c| c.ends_at > now) {
                FailReason::Casting
            } else if self.cooldowns[id as usize].1 > now {
                FailReason::OnCooldown
            } else {
                FailReason::NotReady
            });
        }
        if def.mana_cost > self.mana {
            return Err(FailReason::NoMana);
        }
        if ctx.moving && !def.is_instant() && !def.cast_while_moving {
            return Err(FailReason::Moving);
        }
        let Some(distance) = ctx.distance else {
            return Err(FailReason::NoTarget);
        };
        let leeway = if ctx.moving || ctx.target_moving {
            RANGE_LEEWAY
        } else {
            0.0
        };
        if distance > def.range + leeway {
            return Err(FailReason::OutOfRange);
        }
        if def.requires_los && !ctx.los {
            return Err(FailReason::LineOfSight);
        }
        Ok(())
    }

    /// True if a press now may be held and fired when the caster is ready.
    pub fn in_queue_window(&self, id: AbilityId, def: &AbilityDef, now: Ms) -> bool {
        self.ready_at(id, def) <= now + QUEUE_WINDOW_MS
    }

    /// Start an ability that passed `check`. `allowance_ms` shortens hard
    /// casts on the server so its cast ends when the player's bar does.
    pub fn start(
        &mut self,
        id: AbilityId,
        def: &AbilityDef,
        seq: u32,
        target: EntityId,
        now: Ms,
        allowance_ms: Ms,
    ) -> Started {
        if def.on_gcd() {
            self.gcd_start = now;
            self.gcd_until = now + def.gcd_ms as Ms;
        }
        if def.is_instant() {
            self.pay(id, def, now);
            return Started::Instant;
        }
        let bar_ms = def.bar_ms();
        let (ends_at, ticks_total, tick_interval) = if def.is_channel() {
            self.pay(id, def, now);
            (now + bar_ms, def.channel_ticks, def.channel_tick_interval())
        } else {
            let allowance = allowance_ms.min(MAX_LATENCY_ALLOWANCE_MS).min(bar_ms);
            (now + bar_ms - allowance, 0, 0)
        };
        self.cast = Some(ActiveCast {
            seq,
            ability: id,
            target,
            started_at: now,
            ends_at,
            bar_ms,
            channel: def.is_channel(),
            ticks_done: 0,
            ticks_total,
            tick_interval,
        });
        Started::Bar
    }

    /// Hard casts pay mana and start their cooldown on completion.
    pub fn complete_cast(&mut self, def: &AbilityDef) -> Option<ActiveCast> {
        let cast = self.cast.take()?;
        self.last_cast_end = cast.ends_at;
        if !cast.channel {
            self.pay(cast.ability, def, cast.ends_at);
        }
        Some(cast)
    }

    pub fn cancel(&mut self) -> Option<ActiveCast> {
        let cast = self.cast.take()?;
        self.last_cast_end = cast.started_at;
        Some(cast)
    }

    fn pay(&mut self, id: AbilityId, def: &AbilityDef, now: Ms) {
        self.mana = (self.mana - def.mana_cost).max(0.0);
        if def.cooldown_ms > 0 {
            self.cooldowns[id as usize] = (now, now + def.cooldown_ms as Ms);
        }
    }

    pub fn regen(&mut self, dt_ms: Ms, per_sec: f32) {
        self.mana = (self.mana + per_sec * dt_ms as f32 / 1000.0).min(self.max_mana);
    }

    /// Remaining fraction (1 → 0) of the GCD or cooldown gating this ability,
    /// whichever ends later. Drives the action button swipe.
    pub fn swipe(&self, id: AbilityId, def: &AbilityDef, now: Ms) -> f32 {
        let (cs, ce) = self.cooldowns[id as usize];
        let (s, e) = if def.on_gcd() && self.gcd_until > ce {
            (self.gcd_start, self.gcd_until)
        } else {
            (cs, ce)
        };
        if e <= now || e <= s {
            return 0.0;
        }
        (e - now) as f32 / (e - s) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentPack;

    fn pack() -> ContentPack {
        ContentPack::load_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tenants/default"
        ))
        .unwrap()
    }

    fn ctx() -> CastCtx {
        CastCtx {
            distance: Some(10.0),
            moving: false,
            target_moving: false,
            los: true,
            dead: false,
        }
    }

    fn idx(p: &ContentPack, id: &str) -> AbilityId {
        p.ability_index(id).unwrap()
    }

    #[test]
    fn pack_loads() {
        let p = pack();
        assert_eq!(p.class_bar(0).len(), 5);
    }

    #[test]
    fn los_blocks_after_range() {
        let p = pack();
        let lance = idx(&p, "frost_lance");
        let c = CasterState::new(p.abilities.len(), 1000.0);
        let blind = CastCtx {
            los: false,
            ..ctx()
        };
        assert_eq!(
            c.check(lance, &p.abilities[lance as usize], 0, blind),
            Err(FailReason::LineOfSight)
        );
    }

    #[test]
    fn gcd_blocks_then_queue_window_opens() {
        let p = pack();
        let shard = idx(&p, "ice_shard");
        let def = &p.abilities[shard as usize];
        let mut c = CasterState::new(p.abilities.len(), 1000.0);
        assert_eq!(c.start(shard, def, 1, 0, 0, 0), Started::Instant);
        assert_eq!(c.check(shard, def, 1000, ctx()), Err(FailReason::NotReady));
        assert!(!c.in_queue_window(shard, def, 1099));
        assert!(c.in_queue_window(shard, def, 1100));
        assert!(c.check(shard, def, 1500, ctx()).is_ok());
    }

    #[test]
    fn allowance_shortens_hard_cast_only() {
        let p = pack();
        let lance = idx(&p, "frost_lance");
        let def = &p.abilities[lance as usize];
        let mut c = CasterState::new(p.abilities.len(), 1000.0);
        c.start(lance, def, 1, 0, 1000, 500);
        let cast = c.cast.clone().unwrap();
        assert_eq!(
            cast.ends_at,
            1000 + def.cast_ms as Ms - MAX_LATENCY_ALLOWANCE_MS
        );
        assert_eq!(cast.bar_ms, def.cast_ms as Ms);
    }

    #[test]
    fn off_gcd_ignores_gcd_but_not_cooldown() {
        let p = pack();
        let shard = idx(&p, "ice_shard");
        let surge = idx(&p, "arcane_surge");
        let mut c = CasterState::new(p.abilities.len(), 1000.0);
        c.start(shard, &p.abilities[shard as usize], 1, 0, 0, 0);
        let sdef = &p.abilities[surge as usize];
        assert!(c.check(surge, sdef, 10, ctx()).is_ok());
        c.start(surge, sdef, 2, 0, 10, 0);
        assert_eq!(
            c.check(surge, sdef, 2000, ctx()),
            Err(FailReason::OnCooldown)
        );
    }

    #[test]
    fn channel_ticks_evenly() {
        let p = pack();
        let ch = idx(&p, "arcane_torrent");
        let def = &p.abilities[ch as usize];
        let mut c = CasterState::new(p.abilities.len(), 1000.0);
        c.start(ch, def, 1, 0, 0, 0);
        let mut cast = c.cast.clone().unwrap();
        let mut ticks = vec![];
        while let Some(t) = cast.next_tick_at() {
            ticks.push(t);
            cast.ticks_done += 1;
        }
        assert_eq!(ticks.len() as u32, def.channel_ticks);
        assert_eq!(*ticks.last().unwrap(), def.channel_ms as Ms);
    }

    #[test]
    fn queued_after_cast_starts_at_cast_end() {
        let p = pack();
        let lance = idx(&p, "frost_lance");
        let shard = idx(&p, "ice_shard");
        let mut c = CasterState::new(p.abilities.len(), 1000.0);
        c.start(lance, &p.abilities[lance as usize], 1, 0, 0, 0);
        let end = c.cast.as_ref().unwrap().ends_at;
        c.complete_cast(&p.abilities[lance as usize]);
        // GCD ended long ago; the cast end is what gates the next ability.
        assert_eq!(c.ready_at(shard, &p.abilities[shard as usize]), end);
    }

    #[test]
    fn moving_blocks_casts_not_instants() {
        let p = pack();
        let c = CasterState::new(p.abilities.len(), 1000.0);
        let moving = CastCtx {
            moving: true,
            ..ctx()
        };
        let lance = idx(&p, "frost_lance");
        let shard = idx(&p, "ice_shard");
        assert_eq!(
            c.check(lance, &p.abilities[lance as usize], 0, moving),
            Err(FailReason::Moving)
        );
        assert!(
            c.check(shard, &p.abilities[shard as usize], 0, moving)
                .is_ok()
        );
    }
}

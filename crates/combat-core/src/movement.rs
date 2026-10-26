//! Movement constants and the server's plausibility check. The client owns
//! its position; the server only rejects moves that cannot be legitimate.

use std::collections::VecDeque;

use crate::Ms;

/// Half-size of the flat greybox map, in metres.
pub const ARENA_HALF_EXTENT: f32 = 60.0;
/// Speed may exceed the class speed by this factor over the window.
pub const SPEED_TOLERANCE: f32 = 1.1;
pub const CHECK_WINDOW_MS: Ms = 1000;
/// Absorbs bursts of packets released together (lag spikes, slider changes).
pub const SLACK_METRES: f32 = 1.5;

pub fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    (dx * dx + dz * dz).sqrt()
}

pub fn in_arena(p: [f32; 3]) -> bool {
    p[0].abs() <= ARENA_HALF_EXTENT
        && p[2].abs() <= ARENA_HALF_EXTENT
        && p.iter().all(|v| v.is_finite())
}

/// Rolling window of accepted positions for one entity.
#[derive(Debug, Default)]
pub struct MoveValidator {
    history: VecDeque<(Ms, [f32; 3])>,
    /// Extra distance a dash has earned, and until when it counts.
    bonus: f32,
    bonus_until: Ms,
}

impl MoveValidator {
    pub fn reset(&mut self, now: Ms, pos: [f32; 3]) {
        self.history.clear();
        self.history.push_back((now, pos));
        self.bonus = 0.0;
    }

    /// A dash the server granted: allow `metres` more than walking speed for
    /// the next check window.
    pub fn grant(&mut self, now: Ms, metres: f32) {
        if now > self.bonus_until {
            self.bonus = 0.0;
        }
        self.bonus += metres;
        self.bonus_until = now + CHECK_WINDOW_MS + 200;
    }

    /// Returns true and records the move if it is plausible.
    pub fn check(&mut self, now: Ms, pos: [f32; 3], max_speed: f32) -> bool {
        if !in_arena(pos) {
            return false;
        }
        while self.history.len() > 1 && now.saturating_sub(self.history[0].0) > CHECK_WINDOW_MS {
            self.history.pop_front();
        }
        let ok = match self.history.front() {
            Some(&(t0, p0)) => {
                let dt = now.saturating_sub(t0).max(50) as f32 / 1000.0;
                let bonus = if now <= self.bonus_until {
                    self.bonus
                } else {
                    0.0
                };
                dist_xz(p0, pos) <= max_speed * SPEED_TOLERANCE * dt + SLACK_METRES + bonus
            }
            None => true,
        };
        if ok {
            self.history.push_back((now, pos));
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_needs_a_grant() {
        let mut v = MoveValidator::default();
        v.reset(0, [0.0; 3]);
        // 7 m/s for 100 ms plus a 6 m dash.
        assert!(!v.check(100, [6.7, 0.0, 0.0], 7.0));
        v.grant(100, 6.0);
        assert!(v.check(100, [6.7, 0.0, 0.0], 7.0));
        // The grant expires once the window has moved on.
        assert!(!v.check(3000, [6.7 + 24.0, 0.0, 0.0], 7.0));
    }
}

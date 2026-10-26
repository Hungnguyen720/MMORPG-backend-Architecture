//! Aimed abilities: skillshot flight and the hit tests for skillshots and
//! ground blasts. Flight is deterministic from (origin, direction, speed,
//! launch time), so the server sends one spawn message and every client
//! flies the projectile itself. Server and client run the same hit test;
//! the only difference is which positions they feed it.

use crate::Ms;

/// Collision radius of a body of scale 1 (a 1.8 m human), in metres.
pub const BODY_RADIUS: f32 = 0.5;

/// How far back the server looks up monster positions when testing an aimed
/// ability: the client draws monsters `INTERP_DELAY_MS` in the past plus the
/// network delay. Capped so a laggy shooter cannot hit what is long gone.
pub const MAX_REWIND_MS: Ms = 300;
/// The client's interpolation delay, which the server's rewind makes up for.
pub const INTERP_DELAY_MS: Ms = 100;

/// The server judges aimed hits this much wider than the client draws them.
/// Timing between the two views never lines up to the millisecond, so a shot
/// that just grazes on screen could otherwise miss on the server; in PvE the
/// shooter gets the benefit of the doubt.
pub const SERVER_HIT_LEEWAY: f32 = 0.15;

/// Extra time the server waits before resolving a telegraphed attack against
/// a player, so a dodge the player saw land in time also counts on the
/// server. Up to one-way latency, capped here.
pub const MAX_DODGE_GRACE_MS: Ms = 100;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flight {
    /// Ground-plane (x, z) start point.
    pub origin: [f32; 2],
    /// Unit direction on the ground plane.
    pub dir: [f32; 2],
    pub speed: f32,
    pub range: f32,
    /// Collision half-width added to the body radius.
    pub half_width: f32,
    pub launched_at: Ms,
}

impl Flight {
    /// A flight from `origin` toward `aim`. Aiming at your own feet shoots
    /// along `facing` instead.
    pub fn new(
        origin: [f32; 2],
        aim: [f32; 2],
        facing: f32,
        speed: f32,
        range: f32,
        width: f32,
        launched_at: Ms,
    ) -> Self {
        Self {
            origin,
            dir: direction(origin, aim, facing),
            speed,
            range,
            half_width: width * 0.5,
            launched_at,
        }
    }

    pub fn ends_at(&self) -> Ms {
        self.launched_at + (self.range / self.speed * 1000.0) as Ms
    }

    /// Distance travelled at time `t`, clamped to the flight.
    pub fn travelled(&self, t: Ms) -> f32 {
        let dt = t.saturating_sub(self.launched_at) as f32 / 1000.0;
        (dt * self.speed).min(self.range)
    }

    pub fn pos_at(&self, t: Ms) -> [f32; 2] {
        let d = self.travelled(t);
        [
            self.origin[0] + self.dir[0] * d,
            self.origin[1] + self.dir[1] * d,
        ]
    }

    /// Does the stretch of flight between `t0` and `t1` touch a body of
    /// `radius` at `pos`? Returns how far along the whole flight it touches,
    /// so the earliest of several candidates can be picked.
    pub fn sweep(&self, t0: Ms, t1: Ms, pos: [f32; 2], radius: f32) -> Option<f32> {
        let (a, b) = (self.travelled(t0), self.travelled(t1));
        let r = radius + self.half_width;
        // Project the body onto the flight line.
        let rel = [pos[0] - self.origin[0], pos[1] - self.origin[1]];
        let along = rel[0] * self.dir[0] + rel[1] * self.dir[1];
        let across = (rel[0] * self.dir[1] - rel[1] * self.dir[0]).abs();
        if across > r {
            return None;
        }
        // First distance along the line where the circle is touched.
        let first = along - (r * r - across * across).sqrt();
        let touch = first.max(0.0);
        if touch > b || along + r < a {
            return None;
        }
        Some(touch.max(a))
    }
}

/// Unit vector from `from` toward `to` on the ground plane, falling back to
/// `facing` (radians, 0 = +Z) when they coincide.
pub fn direction(from: [f32; 2], to: [f32; 2], facing: f32) -> [f32; 2] {
    let (dx, dz) = (to[0] - from[0], to[1] - from[1]);
    let len = (dx * dx + dz * dz).sqrt();
    if len < 1e-3 {
        [facing.sin(), facing.cos()]
    } else {
        [dx / len, dz / len]
    }
}

/// Clamp an aim point to `range` from `from`.
pub fn clamp_aim(from: [f32; 2], aim: [f32; 2], range: f32) -> [f32; 2] {
    let (dx, dz) = (aim[0] - from[0], aim[1] - from[1]);
    let len = (dx * dx + dz * dz).sqrt();
    if len <= range || len < 1e-6 {
        aim
    } else {
        [from[0] + dx / len * range, from[1] + dz / len * range]
    }
}

/// Is a body of `radius` at `pos` inside a blast of `blast_radius` at `at`?
pub fn in_blast(at: [f32; 2], blast_radius: f32, pos: [f32; 2], radius: f32) -> bool {
    let (dx, dz) = (pos[0] - at[0], pos[1] - at[1]);
    (dx * dx + dz * dz).sqrt() <= blast_radius + radius
}

/// The (x, z) of a 3D position.
pub fn xz(p: [f32; 3]) -> [f32; 2] {
    [p[0], p[2]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot() -> Flight {
        // 30 m/s straight down +Z from the origin, 30 m range, 1 m wide.
        Flight::new([0.0, 0.0], [0.0, 10.0], 0.0, 30.0, 30.0, 1.0, 1000)
    }

    #[test]
    fn flies_straight_and_stops_at_range() {
        let f = shot();
        assert_eq!(f.pos_at(1000), [0.0, 0.0]);
        let p = f.pos_at(1500);
        assert!((p[1] - 15.0).abs() < 1e-3);
        assert_eq!(f.ends_at(), 2000);
        assert!((f.pos_at(5000)[1] - 30.0).abs() < 1e-3);
    }

    #[test]
    fn sweep_hits_bodies_on_the_line_only_when_reached() {
        let f = shot();
        // Body 12 m ahead, 0.8 m off the line: inside 0.5 + 0.5.
        let body = [0.8, 12.0];
        assert!(
            f.sweep(1000, 1200, body, BODY_RADIUS).is_none(),
            "not reached yet"
        );
        let hit = f.sweep(1200, 1500, body, BODY_RADIUS).expect("hit");
        assert!(hit > 11.0 && hit < 12.0, "touches before the centre: {hit}");
        // Already passed: no hit in a later stretch.
        assert!(f.sweep(1500, 1700, body, BODY_RADIUS).is_none());
        // Too far to the side.
        assert!(f.sweep(1000, 2000, [1.2, 12.0], BODY_RADIUS).is_none());
        // Behind the caster.
        assert!(f.sweep(1000, 2000, [0.0, -3.0], BODY_RADIUS).is_none());
    }

    #[test]
    fn sweep_hits_a_body_standing_on_the_caster() {
        let f = shot();
        assert_eq!(f.sweep(1000, 1010, [0.2, 0.3], BODY_RADIUS), Some(0.0));
    }

    #[test]
    fn aim_on_own_feet_uses_facing() {
        let d = direction([1.0, 1.0], [1.0, 1.0], std::f32::consts::FRAC_PI_2);
        assert!((d[0] - 1.0).abs() < 1e-5 && d[1].abs() < 1e-5);
    }

    #[test]
    fn aim_is_clamped_to_range() {
        let a = clamp_aim([0.0, 0.0], [30.0, 40.0], 10.0);
        assert!((a[0] - 6.0).abs() < 1e-4 && (a[1] - 8.0).abs() < 1e-4);
        assert_eq!(clamp_aim([0.0, 0.0], [3.0, 4.0], 10.0), [3.0, 4.0]);
    }

    #[test]
    fn blast_counts_body_edges() {
        assert!(in_blast([0.0, 0.0], 4.0, [4.4, 0.0], BODY_RADIUS));
        assert!(!in_blast([0.0, 0.0], 4.0, [4.6, 0.0], BODY_RADIUS));
    }
}

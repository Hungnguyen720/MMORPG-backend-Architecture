//! Movement rules and zone collision. The client owns its position; the
//! server only rejects moves that cannot be legitimate. Collision is a set of
//! axis-aligned blockers exported with the zone, so client, server and mob AI
//! agree on walls without a full navmesh yet.

use std::collections::VecDeque;

use crate::Ms;
use crate::content::{Blocker, ZoneDef};

/// Speed may exceed the class speed by this factor over the window.
pub const SPEED_TOLERANCE: f32 = 1.1;
pub const CHECK_WINDOW_MS: Ms = 1000;
/// Absorbs bursts of packets released together (lag spikes, slider changes).
pub const SLACK_METRES: f32 = 1.5;
/// Characters are treated as circles of this radius against walls.
pub const BODY_RADIUS: f32 = 0.4;
/// Blockers shorter than this do not block line of sight.
pub const LOS_MIN_HEIGHT: f32 = 1.6;

pub fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    (dx * dx + dz * dz).sqrt()
}

/// The collision shape of one zone.
#[derive(Debug, Clone)]
pub struct ZoneGeometry {
    pub half_extent: [f32; 2],
    pub blockers: Vec<Blocker>,
}

impl ZoneGeometry {
    pub fn new(z: &ZoneDef) -> Self {
        Self {
            half_extent: z.half_extent,
            blockers: z.blockers.clone(),
        }
    }

    /// A square open arena; used by tests.
    pub fn open(half: f32) -> Self {
        Self {
            half_extent: [half, half],
            blockers: Vec::new(),
        }
    }

    pub fn in_bounds(&self, p: [f32; 3]) -> bool {
        p.iter().all(|v| v.is_finite())
            && p[0].abs() <= self.half_extent[0]
            && p[2].abs() <= self.half_extent[1]
    }

    pub fn clamp(&self, p: [f32; 3]) -> [f32; 3] {
        let m = BODY_RADIUS;
        [
            p[0].clamp(-self.half_extent[0] + m, self.half_extent[0] - m),
            p[1],
            p[2].clamp(-self.half_extent[1] + m, self.half_extent[1] - m),
        ]
    }

    /// True if a body standing at `p` overlaps a wall.
    pub fn inside_blocker(&self, p: [f32; 3]) -> bool {
        self.blockers.iter().any(|b| {
            let (hx, hz) = (b.size[0] / 2.0 + BODY_RADIUS, b.size[1] / 2.0 + BODY_RADIUS);
            (p[0] - b.center[0]).abs() < hx && (p[2] - b.center[1]).abs() < hz
        })
    }

    /// True if walking straight from `a` to `b` passes through a wall.
    /// `shrink` lets the server forgive grazing a corner.
    pub fn path_blocked(&self, a: [f32; 3], b: [f32; 3], shrink: f32) -> bool {
        self.blockers.iter().any(|w| {
            let r = (BODY_RADIUS - shrink).max(0.0);
            segment_hits_box(a, b, w, r)
        })
    }

    /// Line of sight between two points at chest height.
    pub fn los(&self, a: [f32; 3], b: [f32; 3]) -> bool {
        !self
            .blockers
            .iter()
            .any(|w| w.height >= LOS_MIN_HEIGHT && segment_hits_box(a, b, w, 0.0))
    }

    /// Move from `p` by `delta`, sliding along walls and zone edges instead of
    /// stopping dead. Used by the client, bots and mob AI alike.
    pub fn slide(&self, p: [f32; 3], delta: [f32; 2]) -> [f32; 3] {
        let full = self.clamp([p[0] + delta[0], p[1], p[2] + delta[1]]);
        if !self.inside_blocker(full) {
            return full;
        }
        let x_only = self.clamp([p[0] + delta[0], p[1], p[2]]);
        if !self.inside_blocker(x_only) {
            return x_only;
        }
        let z_only = self.clamp([p[0], p[1], p[2] + delta[1]]);
        if !self.inside_blocker(z_only) {
            return z_only;
        }
        p
    }
}

/// Slab test of segment a→b (on the ground plane) against a box grown by `r`.
fn segment_hits_box(a: [f32; 3], b: [f32; 3], w: &Blocker, r: f32) -> bool {
    let min = [w.center[0] - w.size[0] / 2.0 - r, w.center[1] - w.size[1] / 2.0 - r];
    let max = [w.center[0] + w.size[0] / 2.0 + r, w.center[1] + w.size[1] / 2.0 + r];
    let o = [a[0], a[2]];
    let d = [b[0] - a[0], b[2] - a[2]];
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for i in 0..2 {
        if d[i].abs() < 1e-6 {
            if o[i] <= min[i] || o[i] >= max[i] {
                return false;
            }
        } else {
            let inv = 1.0 / d[i];
            let (mut ta, mut tb) = ((min[i] - o[i]) * inv, (max[i] - o[i]) * inv);
            if ta > tb {
                std::mem::swap(&mut ta, &mut tb);
            }
            t0 = t0.max(ta);
            t1 = t1.min(tb);
            if t0 >= t1 {
                return false;
            }
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveVerdict {
    Ok,
    TooFast,
    OutOfBounds,
    ThroughWall,
}

/// Rolling window of accepted positions for one entity.
#[derive(Debug, Default)]
pub struct MoveValidator {
    history: VecDeque<(Ms, [f32; 3])>,
    last: Option<[f32; 3]>,
    /// Failed checks, for flagging repeat offenders.
    pub strikes: u32,
}

impl MoveValidator {
    pub fn reset(&mut self, now: Ms, pos: [f32; 3]) {
        self.history.clear();
        self.history.push_back((now, pos));
        self.last = Some(pos);
    }

    /// Records the move and returns Ok if it is plausible.
    pub fn check(
        &mut self,
        geo: &ZoneGeometry,
        now: Ms,
        pos: [f32; 3],
        max_speed: f32,
    ) -> MoveVerdict {
        if !geo.in_bounds(pos) {
            return self.fail(MoveVerdict::OutOfBounds);
        }
        if let Some(last) = self.last
            && geo.path_blocked(last, pos, 0.2)
        {
            return self.fail(MoveVerdict::ThroughWall);
        }
        while self.history.len() > 1 && now.saturating_sub(self.history[0].0) > CHECK_WINDOW_MS {
            self.history.pop_front();
        }
        if let Some(&(t0, p0)) = self.history.front() {
            let dt = now.saturating_sub(t0).max(50) as f32 / 1000.0;
            if dist_xz(p0, pos) > max_speed * SPEED_TOLERANCE * dt + SLACK_METRES {
                return self.fail(MoveVerdict::TooFast);
            }
        }
        self.history.push_back((now, pos));
        self.last = Some(pos);
        MoveVerdict::Ok
    }

    fn fail(&mut self, v: MoveVerdict) -> MoveVerdict {
        self.strikes += 1;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walled() -> ZoneGeometry {
        ZoneGeometry {
            half_extent: [50.0, 50.0],
            blockers: vec![Blocker {
                center: [0.0, 0.0],
                size: [2.0, 20.0],
                height: 3.0,
            }],
        }
    }

    #[test]
    fn walls_block_paths_and_sight() {
        let g = walled();
        assert!(g.path_blocked([-5.0, 0.0, 0.0], [5.0, 0.0, 0.0], 0.0));
        assert!(!g.path_blocked([-5.0, 0.0, 15.0], [5.0, 0.0, 15.0], 0.0));
        assert!(!g.los([-5.0, 0.0, 0.0], [5.0, 0.0, 0.0]));
        assert!(g.los([-5.0, 0.0, -12.0], [-5.0, 0.0, 12.0]));
    }

    #[test]
    fn slide_runs_along_a_wall() {
        let g = walled();
        let p = g.slide([-1.5, 0.0, 0.0], [1.0, 1.0]);
        assert!(p[0] <= -1.4 && p[2] > 0.9, "{p:?}");
    }

    #[test]
    fn validator_rejects_teleport_and_wall_crossing() {
        let g = walled();
        let mut v = MoveValidator::default();
        v.reset(0, [-5.0, 0.0, 0.0]);
        assert_eq!(v.check(&g, 100, [5.0, 0.0, 0.0], 7.0), MoveVerdict::ThroughWall);
        assert_eq!(v.check(&g, 100, [-5.0, 0.0, 30.0], 7.0), MoveVerdict::TooFast);
        assert_eq!(v.check(&g, 100, [-5.0, 0.0, 0.6], 7.0), MoveVerdict::Ok);
        assert_eq!(v.check(&g, 100, [-60.0, 0.0, 0.6], 7.0), MoveVerdict::OutOfBounds);
    }
}

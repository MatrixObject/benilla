//! The pure server-authored path walk ([`Spline`], `SMSG_MONSTER_MOVE`).
//!
//! This is `benilla-app`'s `net/motion/spline.rs` sampling logic, moved here verbatim (minus the
//! Bevy `Component` derive, the trace lines and the world-collision re-ground system) so the Bevy
//! client and the FFI host evaluate the same server path the same way: constant speed, located by
//! chord length, lerp on a ground path and a uniform Catmull-Rom on a flying one.

use std::time::{Duration, Instant};

use crate::CreateSpline;

/// A server-dictated path (`SMSG_MONSTER_MOVE`) in raw WoW coordinates, walked at constant speed
/// over `duration` from `start`; present only while the unit is on it.
#[derive(Debug, Clone)]
pub struct Spline {
    pub points: Vec<[f32; 3]>,
    pub start: Instant,
    pub duration: Duration,
    /// The server's spline id, echoed in `CMSG_MOVE_SPLINE_DONE` when the spline drives our own
    /// body; the server accepts only its newest id (vmangos `MovementHandler.cpp:807`).
    pub id: u32,
    /// The `FLYING` bit was clear: the 1.12 client zeroes the spline's Δz (`0x616cec`) and takes Z
    /// from the terrain, so the host re-grounds this unit. Flying keeps its Z.
    pub grounded: bool,
    /// `SPLINEFLAG_RUNMODE`: a spline without it sets `MOVEFLAG_WALK_MODE`.
    pub run_mode: bool,
    /// The transport of a deck path (`SMSG_MONSTER_MOVE_TRANSPORT`), whose points are deck-local.
    pub deck: Option<u64>,
}

impl Spline {
    /// Average speed in yd/s (path length over duration); the gait picker reads it.
    pub fn speed(&self) -> f32 {
        let length: f32 = self
            .points
            .windows(2)
            .map(|w| {
                let (dx, dy, dz) = (w[1][0] - w[0][0], w[1][1] - w[0][1], w[1][2] - w[0][2]);
                (dx * dx + dy * dy + dz * dz).sqrt()
            })
            .sum();
        length / self.duration.as_secs_f32().max(1e-3)
    }

    /// Elapsed over duration, clamped to `0..=1`.
    pub fn elapsed_frac(&self) -> f32 {
        (Instant::now()
            .saturating_duration_since(self.start)
            .as_secs_f32()
            / self.duration.as_secs_f32().max(1e-3))
        .clamp(0.0, 1.0)
    }

    /// Whether the walk is still under way at `now`.
    pub fn running(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) < self.duration
    }

    /// Raw-WoW position, facing (`None` when vertical) and pitch `asin(dz/len)` at `now`, at
    /// constant speed. Segments are located by chord length here in both modes; the reference
    /// uses the chord on a ground path only and a flying segment's Catmull-Rom length in 20 steps
    /// (`0x454320`, `0x453760`). The evaluator `0x4541b0` lerps a ground path and runs a uniform
    /// Catmull-Rom (`0x453580`) on a flying one.
    pub fn sample(&self, now: Instant) -> ([f32; 3], Option<f32>, f32) {
        let pts = self.points.as_slice();
        if pts.len() < 2 {
            return (pts.first().copied().unwrap_or([0.0; 3]), None, 0.0);
        }
        let seg = |a: [f32; 3], b: [f32; 3]| {
            let (dx, dy, dz) = (b[0] - a[0], b[1] - a[1], b[2] - a[2]);
            (dx * dx + dy * dy + dz * dz).sqrt()
        };
        let lengths: Vec<f32> = pts.windows(2).map(|w| seg(w[0], w[1])).collect();
        let total: f32 = lengths.iter().sum();
        if total <= f32::EPSILON {
            return (pts[0], None, 0.0);
        }
        let frac = (now.saturating_duration_since(self.start).as_secs_f32()
            / self.duration.as_secs_f32().max(1e-3))
        .clamp(0.0, 1.0);
        let mut want = frac * total;
        for (i, &len) in lengths.iter().enumerate() {
            if want <= len || i + 1 == lengths.len() {
                let t = if len > f32::EPSILON {
                    (want / len).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (pos, dir) = if self.grounded {
                    let (a, b) = (pts[i], pts[i + 1]);
                    let pos = [
                        a[0] + (b[0] - a[0]) * t,
                        a[1] + (b[1] - a[1]) * t,
                        a[2] + (b[2] - a[2]) * t,
                    ];
                    let dir = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                    (pos, dir)
                } else {
                    catmull_rom(pts, i, t)
                };
                let (dx, dy, dz) = (dir[0], dir[1], dir[2]);
                let facing = (dx * dx + dy * dy > 1e-6).then(|| dy.atan2(dx));
                let dlen = (dx * dx + dy * dy + dz * dz).sqrt();
                let pitch = if dlen > f32::EPSILON {
                    (dz / dlen).clamp(-1.0, 1.0).asin()
                } else {
                    0.0
                };
                return (pos, facing, pitch);
            }
            want -= len;
        }
        (*pts.last().unwrap(), None, 0.0)
    }

    /// The flying `(pitch, bank)` in radians (`0x7c5490`'s flying branch): pitch is the tangent's
    /// climb; bank is `2·sign(cross)·acos(t̂·d̂)` between the XY tangent and the XY direction to
    /// the point 1000 ms ahead (`0x7c5623`; 0 under the `2.384e-7` eps), snapped to ±π past ±π/2.
    pub fn flight_attitude(&self, now: Instant) -> (f32, f32) {
        let (pos, facing, pitch) = self.sample(now);
        let Some(f) = facing else {
            return (pitch, 0.0);
        };
        let (look, ..) = self.sample(now + Duration::from_millis(1000));
        let (dx, dy) = (look[0] - pos[0], look[1] - pos[1]);
        let dlen = (dx * dx + dy * dy).sqrt();
        if dlen <= 2.384e-7 {
            return (pitch, 0.0);
        }
        let (tx, ty) = (f.cos(), f.sin());
        let (ux, uy) = (dx / dlen, dy / dlen);
        let dot = (tx * ux + ty * uy).clamp(-1.0, 1.0);
        let cross = tx * uy - ty * ux;
        let theta = if cross < 0.0 { -dot.acos() } else { dot.acos() } * 2.0;
        let bank = if theta < -std::f32::consts::FRAC_PI_2 {
            -std::f32::consts::PI
        } else if theta >= std::f32::consts::FRAC_PI_2 {
            std::f32::consts::PI
        } else {
            theta
        };
        (pitch, bank)
    }
}

/// Uniform Catmull-Rom position and `d/du` tangent on `pts[i] → pts[i+1]` at `u ∈ [0,1]`, the
/// ends duplicated as the 1.12 client stores them (`0x601d30`, `0x601e66`), so the curve passes
/// every waypoint.
fn catmull_rom(pts: &[[f32; 3]], i: usize, u: f32) -> ([f32; 3], [f32; 3]) {
    let p0 = pts[i.saturating_sub(1)];
    let p1 = pts[i];
    let p2 = pts[i + 1];
    let p3 = pts[(i + 2).min(pts.len() - 1)];
    let (u2, u3) = (u * u, u * u * u);
    let mut pos = [0.0f32; 3];
    let mut dir = [0.0f32; 3];
    for a in 0..3 {
        // The standard uniform C-R basis: p(u) = ½·(2P₁ + (−P₀+P₂)u + (2P₀−5P₁+4P₂−P₃)u² +
        // (−P₀+3P₁−3P₂+P₃)u³); dir is its analytic d/du.
        let c1 = p2[a] - p0[a];
        let c2 = 2.0 * p0[a] - 5.0 * p1[a] + 4.0 * p2[a] - p3[a];
        let c3 = -p0[a] + 3.0 * p1[a] - 3.0 * p2[a] + p3[a];
        pos[a] = 0.5 * (2.0 * p1[a] + c1 * u + c2 * u2 + c3 * u3);
        dir[a] = 0.5 * (c1 + 2.0 * c2 * u + 3.0 * c3 * u2);
    }
    (pos, dir)
}

/// The [`Spline`] for one `SMSG_MONSTER_MOVE` along `path` (`[start, …, endpoint]`), or `None`
/// for a stop, a zero duration or a path under two points.
pub fn monster_move_spline(
    path: Vec<[f32; 3]>,
    spline_id: u32,
    stop: bool,
    duration_ms: u32,
    flying: bool,
    run_mode: bool,
    deck: Option<u64>,
) -> Option<Spline> {
    if stop || duration_ms == 0 || path.len() < 2 {
        return None;
    }
    Some(Spline {
        points: path,
        start: Instant::now(),
        duration: Duration::from_millis(u64::from(duration_ms)),
        id: spline_id,
        grounded: !flying,
        run_mode,
        deck,
    })
}

/// The [`Spline`] a unit is already riding in its create block, back-dated by
/// [`CreateSpline::time_passed_ms`]; `None` for a degenerate path, a zero duration or a finished
/// ride.
pub fn create_spline(spline: CreateSpline) -> Option<Spline> {
    if spline.duration_ms == 0
        || spline.path.len() < 2
        || spline.time_passed_ms >= spline.duration_ms
    {
        return None;
    }
    let passed = Duration::from_millis(u64::from(spline.time_passed_ms));
    Some(Spline {
        points: spline.path,
        // `checked_sub`: a spline older than the monotonic clock's origin would panic on `-`.
        start: Instant::now()
            .checked_sub(passed)
            .unwrap_or_else(Instant::now),
        duration: Duration::from_millis(u64::from(spline.duration_ms)),
        id: spline.id,
        grounded: !spline.flying,
        run_mode: spline.run_mode,
        // The create block's spline tail names no transport; its path is in world coordinates.
        deck: None,
    })
}

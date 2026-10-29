//! SmartScreenRect, the client's per-frame anti-overlap placement solver (`UIUtil\SmartScreenRect`),
//! ported across the FFI boundary from `crates/benilla-app/src/smart_rect.rs`. Each rect is
//! relocated off the rects already claimed this frame in its bucket, then claimed. 5875 has two
//! buckets that never interact: nameplates (0) and combat worldtext (1). No state crosses frames, so
//! a pushed rect snaps back when its blocker goes.
//!
//! The viewport here is the UI root in device pixels, Y down; the reference's gx device space is
//! Y-up and every clamp is symmetric, so the mirror is exact.
//! - Solve (`0x5097a0`), a bounded BFS: the seed is the input clamped to the bands (`0x509bf0`
//!   mode 0), whose region (`0x509d80`) fixes the four-direction [`ORDER`]. A push against the
//!   first strictly overlapping claimed rect (`0x509220`) lands edge-to-edge; an unmoved try is
//!   adopted at once, a moved one is queued. Dequeues are capped at
//!   `(floor(W/w)+1)(floor(H/h)+1)` (`0x509dd0`); exhaustion returns the input, drawn overlapping.
//! - Seat tail (`0x509520`, `0x509ec0`): clamp center-X and the top edge half-extent inside,
//!   rebuild top-anchored, normalize (`0x509e20`), claim (`0x509660`).
//! - The node-strategy skip at `0x509874` is dead in 5875 and `showsmartrects` has no readers, so
//!   neither is ported.

use std::collections::VecDeque;

use glam::Vec2;

/// The near-top band (`[0x8087cc]`), in gx units, where one unit is the screen diagonal.
const DDC_NEAR_TOP: f32 = 0.0375;
/// The near-bottom band (`[0x8087d0]`); the X bands (`[0x8087d4]`, `[0x8087d8]`) are 0.0.
const DDC_NEAR_BOTTOM: f32 = 0.01875;

/// The push order per region (`0x8087e0`): 0 up, 1 left, 2 right, 3 down (`0x808870` slots).
const ORDER: [[u8; 4]; 9] = [
    [0, 2, 3, 1], // 0 center:                UP RIGHT DOWN LEFT
    [1, 0, 2, 0], // 1 near bottom:           LEFT UP RIGHT UP
    [1, 3, 2, 3], // 2 near top:              LEFT DOWN RIGHT DOWN
    [0, 1, 3, 1], // 3 off right:             UP LEFT DOWN LEFT
    [0, 2, 3, 2], // 4 off left:              UP RIGHT DOWN RIGHT
    [0, 1, 0, 1], // 5 off right + near bottom
    [0, 2, 0, 2], // 6 off left  + near bottom
    [3, 1, 3, 1], // 7 off right + near top
    [3, 2, 3, 2], // 8 off left  + near top
];

/// A seat rectangle in viewport pixels, Y down: the app's `bevy::math::Rect`, which this crate
/// carries as [`glam::Vec2`] only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Seat {
    pub min: Vec2,
    pub max: Vec2,
}

impl Seat {
    pub(crate) fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Self {
            min: Vec2::new(min_x, min_y),
            max: Vec2::new(max_x, max_y),
        }
    }

    fn width(self) -> f32 {
        self.max.x - self.min.x
    }

    fn height(self) -> f32 {
        self.max.y - self.min.y
    }
}

/// One claim bucket, the rects placed this frame; the first claimed never moves.
#[derive(Default)]
pub(crate) struct SmartBucket(Vec<Seat>);

impl SmartBucket {
    /// The per-frame reset (`0x509500`).
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    /// The seat without the claim. Like the app's plate caller, the result is snapped to device
    /// pixels before [`Self::claim`], so the border art blits 1:1.
    pub(crate) fn resolve(&self, desired: Seat, viewport: Vec2) -> Seat {
        let h = desired.height();
        let (half_w, half_h) = (desired.width() * 0.5, h * 0.5);
        let input = normalize(desired, viewport);
        let solved = self.solve(input, viewport);
        // `0x509720`'s center-X and top edge; `max()` keeps an over-wide rect's bounds ordered.
        let cx =
            ((solved.min.x + solved.max.x) * 0.5).clamp(half_w, (viewport.x - half_w).max(half_w));
        let top = solved
            .min
            .y
            .clamp(half_h, (viewport.y - half_h).max(half_h));
        normalize(Seat::new(cx - half_w, top, cx + half_w, top + h), viewport)
    }

    /// `0x509660`.
    pub(crate) fn claim(&mut self, rect: Seat) {
        self.0.push(rect);
    }

    /// The bounded BFS (`0x5097a0`).
    fn solve(&self, input: Seat, viewport: Vec2) -> Seat {
        let (seed, flags) = clamp_to_bands(input, viewport);
        let region = region_index(flags);
        // `0x509dd0`. A degenerate size saturates, but never overlaps, so adopts at once.
        let bound = ((viewport.x / input.width()).trunc() as i64 + 1)
            .saturating_mul((viewport.y / input.height()).trunc() as i64 + 1);
        let mut queue = VecDeque::from([seed]);
        for _ in 0..bound {
            let Some(node) = queue.pop_front() else {
                return input; // every branch went off-screen
            };
            // The per-try skip (`0x509bf0` mode 1) tests the node, so it is hoisted.
            if band_flags(node, viewport) != 0 {
                continue;
            }
            for &strategy in &ORDER[region as usize] {
                let candidate = push(&self.0, node, strategy);
                if candidate == node {
                    return candidate; // unmoved: overlaps nothing
                }
                queue.push_back(candidate);
            }
        }
        input
    }
}

/// The on-screen clamp (`0x509e20`), physical bottom, top, left, right, so an oversize rect pins
/// top and right.
fn normalize(r: Seat, viewport: Vec2) -> Seat {
    let mut r = r;
    if r.max.y > viewport.y {
        let d = r.max.y - viewport.y;
        r.min.y -= d;
        r.max.y -= d;
    }
    if r.min.y < 0.0 {
        let d = -r.min.y;
        r.min.y += d;
        r.max.y += d;
    }
    if r.min.x < 0.0 {
        let d = -r.min.x;
        r.min.x += d;
        r.max.x += d;
    }
    if r.max.x > viewport.x {
        let d = r.max.x - viewport.x;
        r.min.x -= d;
        r.max.x -= d;
    }
    r
}

/// The region flags (`0x509bf0`): bit 0 off left, 1 off right, 2 near top, 3 near bottom.
fn band_flags(r: Seat, viewport: Vec2) -> u8 {
    let diag = viewport.length();
    let mut flags = 0;
    if r.min.x < 0.0 {
        flags |= 1;
    }
    if r.max.x > viewport.x {
        flags |= 2;
    }
    if r.min.y < DDC_NEAR_TOP * diag {
        flags |= 4;
    }
    if r.max.y > viewport.y - DDC_NEAR_BOTTOM * diag {
        flags |= 8;
    }
    flags
}

/// `0x509bf0` mode 0: the flags and the seed, each offending edge clamped to its band.
///
/// The edge is assigned the boundary and its opposite derived from the size, as the reference
/// does: translating by the gap can land one ULP short in `f32`, the seed then fails the strict
/// re-test in [`SmartBucket::solve`], and the plate pops to the seat clamp for a frame.
fn clamp_to_bands(r: Seat, viewport: Vec2) -> (Seat, u8) {
    let diag = viewport.length();
    let flags = band_flags(r, viewport);
    let (w, h) = (r.width(), r.height());
    let mut r = r;
    if flags & 1 != 0 {
        r.min.x = 0.0;
        r.max.x = w;
    }
    if flags & 2 != 0 {
        r.max.x = viewport.x;
        r.min.x = viewport.x - w;
    }
    if flags & 4 != 0 {
        r.min.y = DDC_NEAR_TOP * diag;
        r.max.y = r.min.y + h;
    }
    if flags & 8 != 0 {
        r.max.y = viewport.y - DDC_NEAR_BOTTOM * diag;
        r.min.y = r.max.y - h;
    }
    (r, flags)
}

/// The 3x3 region (`0x509d80`), for all 16 flag words: bit 0 dominates and ignores bit 1, then
/// bit 2 ignoring bit 3, then bit 1, then bit 3.
fn region_index(flags: u8) -> u8 {
    if flags & 1 != 0 {
        if flags & 4 != 0 {
            8
        } else if flags & 8 != 0 {
            6
        } else {
            4
        }
    } else if flags & 4 != 0 {
        if flags & 2 != 0 {
            7
        } else {
            2
        }
    } else if flags & 2 != 0 {
        if flags & 8 != 0 {
            5
        } else {
            3
        }
    } else if flags & 8 != 0 {
        1
    } else {
        0
    }
}

/// One try (`0x808870`, `0x509220`): push edge-to-edge off the first strict overlap
/// (`0x5091c0`, `0x509300`, `0x509360`, `0x5093c0`), or return the node unchanged.
fn push(claimed: &[Seat], r: Seat, strategy: u8) -> Seat {
    let Some(c) = claimed
        .iter()
        .find(|c| r.max.x > c.min.x && r.min.x < c.max.x && r.max.y > c.min.y && r.min.y < c.max.y)
    else {
        return r;
    };
    let mut r = r;
    match strategy {
        0 => {
            // Up.
            let d = r.max.y - c.min.y;
            r.min.y -= d;
            r.max.y -= d;
        }
        1 => {
            let d = r.max.x - c.min.x;
            r.min.x -= d;
            r.max.x -= d;
        }
        2 => {
            let d = c.max.x - r.min.x;
            r.min.x += d;
            r.max.x += d;
        }
        _ => {
            // Down.
            let d = c.max.y - r.min.y;
            r.min.y += d;
            r.max.y += d;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Vec2 = Vec2::new(1024.0, 768.0);

    fn seat(bucket: &mut SmartBucket, desired: Seat) -> Seat {
        let r = bucket.resolve(desired, VP);
        bucket.claim(r);
        r
    }

    #[test]
    fn a_free_rect_stays_put() {
        let mut b = SmartBucket::default();
        let r = Seat::new(500.0, 380.0, 540.0, 400.0);
        assert_eq!(seat(&mut b, r), r);
    }

    #[test]
    fn a_center_overlap_pushes_up_abutting() {
        let mut b = SmartBucket::default();
        let r = Seat::new(500.0, 380.0, 540.0, 400.0);
        seat(&mut b, r);
        let pushed = seat(&mut b, r);
        assert_eq!(pushed.max.y, 380.0, "bottom abuts the blocker's top");
        assert_eq!(pushed.min.y, 360.0, "size preserved");
        assert_eq!((pushed.min.x, pushed.max.x), (500.0, 540.0), "x untouched");
    }

    /// The up child still overlaps the second rect while the right child is free.
    #[test]
    fn a_third_goes_right_not_twice_up() {
        let mut b = SmartBucket::default();
        let r = Seat::new(500.0, 380.0, 540.0, 400.0);
        seat(&mut b, r);
        seat(&mut b, r);
        let third = seat(&mut b, r);
        assert_eq!(third.min.x, 540.0, "left edge abuts the blocker's right");
    }

    /// A plate seeded inside the near-top band seats exactly on the band edge, never a ULP short
    /// of it (which pops the plate to the seat clamp for a frame).
    #[test]
    fn a_plate_in_the_near_top_band_never_pops() {
        let vp = Vec2::new(640.0, 360.0);
        let band = DDC_NEAR_TOP * vp.length();
        for step in 0..2800 {
            #[allow(clippy::cast_precision_loss)]
            let top = step as f32 * 0.01;
            let got = SmartBucket::default().resolve(Seat::new(172.0, top, 245.0, top + 18.0), vp);
            let want = if top < band { band } else { top };
            assert!(
                (got.min.y - want).abs() < 1e-3,
                "a plate seeded at y={top} seats at {} — the band edge is {band}",
                got.min.y
            );
        }
    }
}

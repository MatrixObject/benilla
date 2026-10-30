//! Middle-layer player: benilla's locomotion, collision probe and camera.
//!
//! The client owns movement (`state`, `mover`)
//! and the third-person camera (`camera`) inside the Bevy app, which
//! cannot be called from here (it pulls the whole engine). This module keeps
//! the same constants, units (WoW yards) and laws - run 7 yd/s, backpedal
//! 4.5/7, walk 2.5/7, gravity 19.29, jump 7.96, terminal 60.15, 50 deg ground
//! slope, 1 yd step-up, ±89 deg pitch, 0.003 rad per mouse unit - and answers
//! the ground with the same ADT/WMO geometry benilla loads, so the UE host is
//! a pure renderer that just applies the returned transform.
//!
//! Player position is raw WoW world yards - the same space the ADT chunks and
//! MODF placements use - so `(x, -y, z) * 100` is the UE transform.

use std::cell::RefCell;
use std::collections::HashMap;

use benilla_formats::{adt_to_tile_mesh, world_to_chunk};

use crate::{placement_quat, q_rot, BnWorld};

// ---- state constants (read-only port) ----
const RUN_SPEED: f32 = 7.0;
const WALK_RATIO: f32 = 2.5 / 7.0;
const RUN_BACK_RATIO: f32 = 4.5 / 7.0;
const GRAVITY: f32 = 19.291_105;
const JUMP_SPEED: f32 = 7.955_547;
const TERMINAL_VELOCITY: f32 = 60.148_003;
/// How far below the last real floor the void guard lets a body fall before it
/// is restored there (yd). A real floor anywhere below is found by `ground`
/// long before this; only a true hole in the collision data gets here.
const VOID_GUARD_DEPTH: f32 = 100.0;
const GROUND_COS: f32 = 0.642_788;
const STEP_UP_HEIGHT: f32 = 1.0;
/// The step-vs-slide slope ratio, `[0x80c740]` (atan ≈ 61.6°): the foot cone's
/// bevels rise `radius · ratio` from the foot, and the step-up certifies its
/// landing `H · tan 50°` ahead (`benilla-app`'s `STEP_SLOPE_RATIO`).
const STEP_SLOPE_RATIO: f32 = 1.849_399;
/// The foot cone's height (yd): the solid is a cone below this and a box above
/// it, so an edge inside the cone is ridden up its skirt instead of blocking
/// (`0x631440`, `0x635c00`); a higher one takes the step-up. 0.616 yd with the
/// placeholder capsule radius (`benilla-app`'s `FOOT_CONE_HEIGHT`).
const FOOT_CONE_HEIGHT: f32 = CAPSULE_RADIUS * STEP_SLOPE_RATIO;
/// The step-up's certify advance (yd): `H · tan 50°`, 1.1918 at `H` = 1. The
/// reference sweeps this far along the travel to find the landing
/// (`benilla-app`'s `STEP_UP_ADVANCE`).
const STEP_UP_ADVANCE: f32 = STEP_UP_HEIGHT * 1.191_753_6;
/// The step-down snap's fixed slack (yd), `[0x7ff9d0]` = 1/36: a descent
/// reaches `travel · STEP_SLOPE_RATIO + slack` below the feet and settles only
/// onto a floor inside it; anything deeper is a drop, not a step
/// (`benilla-app`'s `STEP_SNAP_SLACK`).
const STEP_SNAP_SLACK: f32 = 1.0 / 36.0;
// The capsule the client traces with (`CAPSULE_RADIUS`/`CAPSULE_HEIGHT`);
// the probe below is a point at the feet plus the step window, which is the
// same corridor for a walker on the floors this data ships.
#[allow(dead_code)]
const CAPSULE_RADIUS: f32 = 1.0 / 3.0;
#[allow(dead_code)]
const CAPSULE_HEIGHT: f32 = 2.027_777_7;

// ---- camera ----
const CAM_PITCH_LIMIT: f32 = 89.0 * std::f32::consts::PI / 180.0;
/// `crates/benilla-app/src/player/camera.rs`'s `LOOK_SENSITIVITY`: radians of
/// camera rotation per raw mouse unit at the default move speeds and `mousespeed`
/// 1.0. The reference's law (`0x50fee0`) is `yaw_speed × Δx / 180` and
/// `pitch_speed × Δy / 90`, which at the default 180 and 90 both land here.
const LOOK_SENSITIVITY: f32 = 0.003;
const CAM_DIST_DEFAULT: f32 = 15.0; // camera_zoom's deviation, for a wider view
/// The shortest boom the collision probe will pull to: any closer and the eye
/// sits inside the body (the client's first-person cut is not modelled).
const CAM_BOOM_MIN: f32 = 1.0;
/// `benilla-world`'s `CAMERA_PROBE_RADIUS` (collision.rs): the camera's sweep
/// sphere, which keeps the near plane out of the surface it stops at.
const CAM_PROBE_RADIUS: f32 = 0.3;
/// `cameraYawSmoothSpeed` default 180°/s: the smart follow's return rate.
const FOLLOW_RATE: f32 = std::f32::consts::PI;
/// `creature_anim::select::STRAFE_BLEND_RATE`: the body heading eases to the
/// strafe offset at this rate (/s).
const STRAFE_BLEND_RATE: f32 = 17.26;
/// `TURN_RATE`: the keyboard turn rate, π rad/s (the server's own
/// value; the client ships no default).
const TURN_RATE: f32 = std::f32::consts::PI;
/// `TURN_RATE_MOVING`: the scale while translating or falling.
const TURN_RATE_MOVING: f32 = 0.75;
/// Once steering stops, the standing body closes on the aim at
/// `turnRate × 8` rad/s, clamped to the gap (`0x607ed0`); while steering,
/// only the 90° ceiling moves it (`0x60818a`-`0x6081bf`).
const STATIONARY_CHASE_RATE: f32 = 8.0;
/// Roam-mode flight speed (yd/s): a zone tour without waiting on terrain.
const FLY_SPEED: f32 = 30.0;

// ---- the upper-body one-shots ----
/// AnimationData ids: Death and SpellCast (the swing clip comes from the armed
/// main hand: `net::self_swing_anim`, the reference's `swing_anim_main`).
const ANIM_DEATH: u32 = 1;
const ANIM_SPELL_CAST: u32 = 53;
/// The attack overlay's visible window (s), the unarmed clip's own length.
const SWING_WINDOW: f32 = 0.7;
/// The overlay's blend time constant (s): the vanilla masked layer's ease.
const OVERLAY_BLEND: f32 = 0.12;

// ---- camera pivot presets ----
/// A scale-1 Human Male's framing pivot: standing 1.9002692, swimming
/// 0.3882572 lower (`model_pivot_height`, `cam+0x124`).
const CAM_PIVOT_STAND: f32 = 1.900_269_2;
const CAM_PIVOT_SWIM: f32 = 1.512_012;
/// The pivot channel eases at 1.2 yd/s (the client's `|Δh| / 1.2` s).
const CAM_PIVOT_SMOOTH: f32 = 1.2;
/// `model_pivot_height`'s `[5/6, 15]` band.
const CAM_PIVOT_FLOOR: f32 = 5.0 / 6.0;
const CAM_PIVOT_CEIL: f32 = 15.0;

// ---- camera_water ----
/// The resting corridor floor, and the pivot's depth under the surface while
/// submerged.
const WATER_REST: f32 = 5.0 / 6.0;
/// The surface band's width, and the pivot's lift above the waterline in it
const WATER_SURFACE_BAND: f32 = 2.0 / 9.0;
/// The submerge band's outer edge.
const WATER_SUBMERGE_EDGE: f32 = 5.0 / 9.0;
/// The least head-room the pivot keeps above the corridor floor.
const WATER_MIN_HEADROOM: f32 = 1.0 / 9.0;

/// Mount speed bonus in 1.12: +60% (the client's riding speed).
const MOUNT_SPEED_RATIO: f32 = 1.6;

// ---- swim (read-only port) ----
/// Stock `MOVE_SWIM` (yd/s), vmangos `Unit.cpp:71`.
const SWIM_SPEED: f32 = 4.722_222;
/// Stock `MOVE_SWIM_BACK` (yd/s), vmangos `Unit.cpp:72`.
const SWIM_BACK_SPEED: f32 = 2.5;
/// The swim jump's seed (yd/s), about 14 % over the land jump.
const SWIM_JUMP_SPEED: f32 = 9.096_748;
/// The share of the collision height the water must cover to swim.
const SWIM_DEPTH_FRAC: f32 = 0.75;
/// The enter/leave band: hold between `0.75·h - 1/36` and `0.75·h`.
const SWIM_HYSTERESIS: f32 = 1.0 / 36.0;

// ---- airborne bracket (`player::arc` + `creature_anim::select`) ----
/// The launch vertical speed above which an arc is a jump and not a step-off
/// (`benilla-app`'s `JUMP_ARC_MIN_UP`, the split `0x60e480` makes).
const JUMP_ARC_MIN_UP: f32 = 0.5;
/// How far below the launch a jumped arc has to drop before the fall counts as
/// far (`benilla-app`'s `FALL_FAR_DROP`, `[0x633240]`). A flat jump's arc comes
/// back to within a hand-span of its takeoff, so it never latches: the whole
/// jump stays in the Jump bracket and only a real descent takes the Fall clip.
const FALL_FAR_DROP: f32 = 0.111_11;
/// ... and for a step-off, the time airborne that does the same (`FALL_FAR_TIME`).
const FALL_FAR_TIME: f32 = 0.5;
/// JumpStart's authored length (ms). Every shipped player and mount M2 authors
/// 833 for it (`HumanMale`, `OrcMale`, `RidingHorse` all read seq start/end at
/// 833 through `bn_model_anim_by_id`), which is why a flat jump - 825 ms of arc -
/// shows the launch pose through the whole flight.
const JUMP_ENTER_MS: f32 = 833.0;
/// The landing one-shot's authored length (ms): JumpEnd is 1000 on Human and on
/// the mount (Orc authors 1500) and JumpLandRun 833 on Human, so one window fits
/// the arcs the player actually sees.
const LAND_MS: f32 = 1000.0;

/// The state the host applies (plain C; yaw/pitch radians, positions yards).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BnPlayerState {
    /// Feet position in WoW world yards.
    pub pos: [f32; 3],
    /// Rendered body facing (aim plus the eased strafe twist): 0 looks along
    /// +X, π/2 along +Y.
    pub yaw: f32,
    /// Camera eye in WoW world yards.
    pub cam_eye: [f32; 3],
    /// Camera look-at point (the framing pivot).
    pub cam_target: [f32; 3],
    pub cam_pitch: f32,
    pub cam_distance: f32,
    /// Horizontal speed, yd/s.
    pub speed: f32,
    /// 0/1.
    pub grounded: u32,
    /// 0/1.
    pub mounted: u32,
    /// The mount's CreatureDisplayInfo id (`UNIT_FIELD_MOUNTDISPLAYID`), 0
    /// standing. The host resolves the model and its DBC scale from it; a
    /// preview mount (no server behind it) reports 0 and the host falls back.
    pub mount_display: u32,
    /// The mount's own AnimationData id while mounted (0 Stand, 4 Walk, 5
    /// Run): the rider holds Mount (91) and the mount walks beneath it. 0
    /// when standing or unmounted.
    pub mount_anim: u32,
    /// AnimationData.dbc id to play (0 Stand, 4 Walk, 5 Run, 13 WalkBackwards,
    /// 16 AttackUnarmed, 37 JumpStart, 38 Jump, 39 JumpEnd, 40 Fall, 91 Mount,
    /// 187 JumpLandRun). A one-shot action the lower body cannot show (an attack
    /// or a cast while running) rides the overlay instead.
    pub anim: u32,
    /// The upper-body overlay's AnimationData id (16 attack, 53 cast), or 0
    /// for none: the vanilla masked SpineLow subtree split.
    pub overlay_anim: u32,
    /// The overlay's blend weight, 0..1.
    pub overlay_weight: f32,
    /// How far into the one-shot the overlay is, ms. A locomotion request
    /// mid-swing moves the clip to the torso at its elapsed position
    /// (`driver::transplant_up`, `0x5fe919`), so the swing finishes there
    /// instead of restarting from its first frame.
    pub overlay_anim_ms: u32,
    /// The strafe counter-twist, radians: how far SpineLow (key bone 4) turns
    /// the upper body back toward the aim, and Head (key bone 6) the rest
    /// (`0x607ed0`'s tail into the `0x711f10` bone channels). WoW yaw sign,
    /// capped at ±π/4 each; 0 when the body faces its aim. A mounted rider's
    /// spine share is 0 and only the head turns.
    pub twist_spine: f32,
    pub twist_head: f32,
    /// The aim the body chases, radians: [`Self::yaw`] plus the gap the twist
    /// spends. Readback for probes, never applied.
    pub aim_yaw: f32,
    /// The camera's own heading, radians (the smart follow's channel).
    pub cam_yaw: f32,
    /// The held axes this frame (forward, strafe, turn), readback for probes.
    pub axes: [f32; 3],
}

/// Wrap an angle to (-π, π].
fn wrap_pi(a: f32) -> f32 {
    let mut a = a % std::f32::consts::TAU;
    if a > std::f32::consts::PI {
        a -= std::f32::consts::TAU;
    } else if a <= -std::f32::consts::PI {
        a += std::f32::consts::TAU;
    }
    a
}

/// The water corridor's pivot height (`camera_water::classify` + `corridor` +
/// `pivot_height`), in yards above the feet. `target` is the model preset, `live`
/// the eased channel value; the client's head-room probe is not built, so the
/// reach is full. A surface swimmer's pivot rises to `surface + 2/9` and a diver's
/// sits `5/6` under the surface, which is what keeps the boom out of the water.
fn camera_pivot_height(target: f32, live: f32, surface: Option<f32>, feet: f32) -> f32 {
    let (floor, cap) = match surface {
        None => (WATER_REST, live),
        Some(s) => {
            let d = s - feet;
            let excess = d - target;
            // A tie at 2/9 lands in the submerge band and one at 5/9 in clear,
            // the client's own tie-breaks.
            if excess < WATER_SURFACE_BAND {
                let floor = d + WATER_SURFACE_BAND;
                (floor, live.max(floor))
            } else if excess < WATER_SUBMERGE_EDGE {
                (WATER_REST, (d - WATER_REST).max(WATER_REST))
            } else {
                (WATER_REST, live)
            }
        }
    };
    let reach = (target.max(live) - floor).max(WATER_MIN_HEADROOM);
    (floor + reach)
        .min(cap)
        .clamp(CAM_PIVOT_FLOOR, CAM_PIVOT_CEIL)
}

/// The counter-twist's shares of the aim-to-body gap (`creature_anim::twist`'s
/// `twist_shares`, the reference's `0x711f10` bone channels off `0x607ed0`'s
/// tail): SpineLow (key bone 4) takes half the gap and Head (key bone 6) the
/// rest, each capped at π/4, so a pure strafe's head lands back on the aim.
fn twist_shares(gap: f32) -> (f32, f32) {
    use std::f32::consts::FRAC_PI_4;
    let spine = (gap * 0.5).clamp(-FRAC_PI_4, FRAC_PI_4);
    let head = (gap - spine).clamp(-FRAC_PI_4, FRAC_PI_4);
    (spine, head)
}

/// [`twist_shares`] with the mount gate: the `0x607ed0` tail arms SpineLow only
/// while the unit has no mount model (`CGUnit+0xdc` null), and Head always, so a
/// rider's head keeps its share and stops short of the aim.
fn armed_twist_shares(gap: f32, mounted: bool) -> (f32, f32) {
    let (spine, head) = twist_shares(gap);
    (if mounted { 0.0 } else { spine }, head)
}

/// The standing body's own turn for one frame (`player/gait.rs`'s stationary
/// chase, `0x60818a`-`0x6081bf`): while the aim is steered the body holds a
/// 90° ceiling and only the excess moves it; once the steer stops it sweeps
/// onto the aim at [`STATIONARY_CHASE_RATE`] times the mover's turn rate,
/// never past the gap. `delta` is `wrap(aim − body)`.
fn stationary_chase_step(delta: f32, steering: bool, dt: f32) -> f32 {
    use std::f32::consts::FRAC_PI_2;
    let mut step = (delta.abs() - FRAC_PI_2).max(0.0);
    if !steering {
        step += dt * TURN_RATE * STATIONARY_CHASE_RATE;
    }
    step.min(delta.abs()).copysign(delta)
}

/// Ease the rendered yaw toward `aim + offset` through the aim-relative
/// offset, which swings round the front where the shortest arc ties at the
/// ±90° strafe flip (`creature_anim::select::ease_strafe_yaw`).
fn ease_strafe_yaw(current_yaw: f32, aim: f32, offset: f32, dt: f32) -> f32 {
    let cur = wrap_pi(current_yaw - aim);
    let eased = cur + (offset - cur) * (1.0 - (-STRAFE_BLEND_RATE * dt).exp());
    wrap_pi(aim + eased)
}

/// The strafe body offset from the aim (`creature_anim::select::strafe_body_offset`):
/// ±π/2 for a pure strafe, ±π/4 with forward or back, mirrored backpedaling,
/// left positive in WoW axes.
fn strafe_body_offset(forward: f32, strafe: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
    let left = strafe < -0.01; // A = -1 (left of the facing)
    let right = strafe > 0.01;
    if left == right {
        return 0.0;
    }
    let diagonal = forward.abs() > 0.01;
    let magnitude = if diagonal { FRAC_PI_4 } else { FRAC_PI_2 };
    let back = forward < -0.01;
    if left != back {
        magnitude
    } else {
        -magnitude
    }
}

/// One triangle in WoW world coords, with a computed unit normal.
#[derive(Clone, Copy)]
struct Tri {
    v: [[f32; 3]; 3],
    n: [f32; 3],
}

impl Tri {
    fn new(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Self {
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            u[1] * w[2] - u[2] * w[1],
            u[2] * w[0] - u[0] * w[2],
            u[0] * w[1] - u[1] * w[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let n = if len > 1.0e-9 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            [0.0, 0.0, 0.0]
        };
        Self { v: [a, b, c], n }
    }

    /// Height of the triangle under (x, y); `None` outside it or above `ceil`.
    fn height_at(&self, x: f32, y: f32, ceil: f32) -> Option<f32> {
        let (ax, ay) = (self.v[0][0], self.v[0][1]);
        let (bx, by) = (self.v[1][0], self.v[1][1]);
        let (cx, cy) = (self.v[2][0], self.v[2][1]);
        let d = (by - cy) * (ax - cx) + (cx - bx) * (ay - cy);
        if d.abs() < 1.0e-9 {
            return None; // vertical (a wall, never a floor)
        }
        let l1 = ((by - cy) * (x - cx) + (cx - bx) * (y - cy)) / d;
        let l2 = ((cy - ay) * (x - cx) + (ax - cx) * (y - cy)) / d;
        let l3 = 1.0 - l1 - l2;
        if l1 < -1.0e-4 || l2 < -1.0e-4 || l3 < -1.0e-4 {
            return None;
        }
        let z = l1 * self.v[0][2] + l2 * self.v[1][2] + l3 * self.v[2][2];
        (z <= ceil).then_some(z)
    }
}

/// WMO triangle grid over one ADT tile: the tile's own 32x32 half-chunk cells
/// plus a one-cell margin on each side, so geometry placed just across a tile
/// border still answers when the player stands on the neighbour.
const WMO_GRID: i32 = 34;
const WMO_MARGIN: i32 = 1;
/// One map cell is half a chunk (16.67 yd); the map corner is -17066.67.
const WMO_CELL: f32 = 33.333_336 / 2.0;

// ---- doodad collision hulls ----
// A map doodad collides by its M2 collision hull (the header's coarse
// `bounding_vertices`/`bounding_triangles`), never by its render mesh:
// `benilla-world`'s `placement_collider_data` traces exactly this gather. A
// grass tuft or a canopy ships no hull and is walked through; a trunk's hull is
// a handful of triangles the mover can certify a step onto. The old base-slice
// box stood in for every model alike - grass became solid, a trunk became a
// wall its canopy's footprint wide, and nothing could be stepped on.

fn parked_doodad_hulls(
) -> &'static std::sync::Mutex<HashMap<String, Option<benilla_formats::CollisionMesh>>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, Option<benilla_formats::CollisionMesh>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn parked_wmo_cols() -> &'static std::sync::Mutex<HashMap<String, benilla_formats::CollisionMesh>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, benilla_formats::CollisionMesh>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Park one model's collision hull (`None` = the model ships none).
pub(crate) fn doodad_hull_park(path: &str, hull: Option<benilla_formats::CollisionMesh>) {
    if let Ok(mut m) = parked_doodad_hulls().lock() {
        m.insert(path.to_string(), hull);
    }
}

fn doodad_hull_parked(path: &str) -> Option<Option<benilla_formats::CollisionMesh>> {
    parked_doodad_hulls().lock().ok()?.get(path).cloned()
}

/// A doodad's hull, parsed here when the tile worker never parked it.
fn m2_collision_hull(
    chain: &mut benilla_formats::Chain,
    raw_path: &str,
) -> Option<benilla_formats::CollisionMesh> {
    let hull = benilla_formats::load_m2_collision_hull(chain, raw_path).ok()?;
    (!hull.is_empty()).then_some(hull)
}

/// Park one WMO's collision mesh for the body tile that lists it.
pub(crate) fn wmo_collision_park(path: &str, mesh: benilla_formats::CollisionMesh) {
    if let Ok(mut m) = parked_wmo_cols().lock() {
        m.insert(path.to_string(), mesh);
    }
}

pub(crate) fn wmo_collision_parked(path: &str) -> bool {
    parked_wmo_cols()
        .lock()
        .map(|m| m.contains_key(path))
        .unwrap_or(false)
}

fn wmo_collision_take(path: &str) -> Option<benilla_formats::CollisionMesh> {
    parked_wmo_cols().lock().ok()?.remove(path)
}

/// One ADT tile's collision: terrain triangles bucketed by the chunk they
/// came from (16x16 buckets = the chunks), plus the WMO floors once built.
pub(crate) struct TileCol {
    buckets: Vec<Vec<Tri>>,
    /// Terrain triangles by chunk bucket - `buckets.len == 256`.
    /// WMO triangles bucketed on a [`WMO_GRID`] grid of the tile; a triangle
    /// lands in every cell its XY bounds overlap, so the huge city floors
    /// answer from any point instead of a full-scan every frame.
    wmo_cells: Vec<Vec<Tri>>,
    /// Doodad collision-hull triangles on the same grid: each placement's M2
    /// hull, placed and scaled (`load_m2_collision_hull`).
    doodad_cells: Vec<Vec<Tri>>,
    /// MODF placements, kept from the ADT so the WMO floors can be built once.
    wmos: Vec<benilla_formats::WmoInstance>,
    /// MDDF placements, whose M2 collision hulls become world triangles.
    doodads: Vec<benilla_formats::Doodad>,
    /// MCLQ surfaces, sampled for the swim latch and the floating resolver.
    liquids: Vec<benilla_formats::LiquidMesh>,
    /// WMO MLIQ surfaces (the canals and fountains) with their placement, in
    /// model space: a query is mirrored into model space instead of baking.
    wmo_liquids: Vec<(benilla_formats::WmoInstance, benilla_formats::LiquidMesh)>,
    wmo_ready: bool,
    /// WMO fold cursor: instances are baked into the grid a budgeted slice at
    /// a time, so adopting a city tile never spends 40 ms in one tick. The
    /// vertex transform is sliced by `wmo_vtx` for the same reason (a city
    /// WMO's collision mesh runs to tens of thousands of vertices).
    wmo_inst: usize,
    wmo_tri: usize,
    wmo_vtx: usize,
    wmo_world: Vec<[f32; 3]>,
    /// WMO MLIQ cursor: one model's water groups per slice (each is an MPQ
    /// read and parse; a city tile holds a dozen).
    wmo_mliq: usize,
    /// Doodad cursor: hulls are folded in a few models at a time (the first
    /// cold parse of ~200 unique M2 meshes costs half a second), so the frame
    /// never stalls on a tile load.
    doodad_next: usize,
}

impl TileCol {
    fn new() -> Self {
        Self {
            buckets: (0..256).map(|_| Vec::new()).collect(),
            wmo_cells: (0..(WMO_GRID * WMO_GRID) as usize)
                .map(|_| Vec::new())
                .collect(),
            doodad_cells: (0..(WMO_GRID * WMO_GRID) as usize)
                .map(|_| Vec::new())
                .collect(),
            wmos: Vec::new(),
            doodads: Vec::new(),
            liquids: Vec::new(),
            wmo_liquids: Vec::new(),
            wmo_ready: false,
            wmo_inst: 0,
            wmo_tri: 0,
            wmo_vtx: 0,
            wmo_world: Vec::new(),
            wmo_mliq: 0,
            doodad_next: 0,
        }
    }

    /// Tile-local grid coordinate of a world point (`WMO_MARGIN` outside the
    /// tile is still inside the grid). Grid X follows world Y and grid Y
    /// follows world X, as `world_to_chunk` names its axes.
    fn wmo_local(tx: i32, ty: i32, x: f32, y: f32) -> (i32, i32) {
        let gx = ((17066.666 - y) / WMO_CELL).floor() as i32;
        let gy = ((17066.666 - x) / WMO_CELL).floor() as i32;
        (gx - tx * 32 + WMO_MARGIN, gy - ty * 32 + WMO_MARGIN)
    }

    /// Cell index of a world point, `None` outside this tile's grid.
    fn wmo_cell(tx: i32, ty: i32, x: f32, y: f32) -> Option<usize> {
        let (lx, ly) = Self::wmo_local(tx, ty, x, y);
        if !(0..WMO_GRID).contains(&lx) || !(0..WMO_GRID).contains(&ly) {
            return None;
        }
        Some((ly * WMO_GRID + lx) as usize)
    }

    /// Every grid cell a triangle's XY bounds overlap, clamped to the grid.
    fn wmo_span(tx: i32, ty: i32, t: &Tri) -> (i32, i32, i32, i32) {
        let minx = t.v[0][0].min(t.v[1][0]).min(t.v[2][0]);
        let maxx = t.v[0][0].max(t.v[1][0]).max(t.v[2][0]);
        let miny = t.v[0][1].min(t.v[1][1]).min(t.v[2][1]);
        let maxy = t.v[0][1].max(t.v[1][1]).max(t.v[2][1]);
        let (ax, ay) = Self::wmo_local(tx, ty, minx, miny);
        let (bx, by) = Self::wmo_local(tx, ty, maxx, maxy);
        (
            ax.min(bx).clamp(0, WMO_GRID - 1),
            ay.min(by).clamp(0, WMO_GRID - 1),
            ax.max(bx).clamp(0, WMO_GRID - 1),
            ay.max(by).clamp(0, WMO_GRID - 1),
        )
    }
}

/// Bake WMO walking triangles + MLIQ surfaces into the tile's grid, one
/// budgeted slice per tick. A city tile's floors are tens of thousands of
/// triangles and folding them all in one tick was a 38-47 ms frame on the
/// tick the body adopted the tile; the cursor resumes next tick.
/// Returns true when this step contributed geometry (the caller then re-snaps
/// the body so it does not start under a floor).
fn build_wmo_collision_step(
    t: &mut TileCol,
    tx: i32,
    ty: i32,
    chain: &mut benilla_formats::Chain,
    cache: &mut HashMap<String, benilla_formats::CollisionMesh>,
    budget: std::time::Duration,
) -> bool {
    let start = std::time::Instant::now();
    let mut built = false;
    while t.wmo_inst < t.wmos.len() {
        let model = t.wmos[t.wmo_inst].model.clone();
        if !cache.contains_key(&model) {
            // The tile worker parks this model's collision mesh beside the
            // tile; a parse here runs on the tick that adopts the tile, so it
            // waits for a fresh slice instead of overrunning the budget.
            let mesh = match wmo_collision_take(&model) {
                Some(m) => m,
                None => {
                    if start.elapsed() >= budget {
                        break;
                    }
                    match benilla_formats::load_wmo_collision_tris(chain, &model) {
                        Ok(m) => m,
                        Err(_) => {
                            t.wmo_inst += 1;
                            t.wmo_tri = 0;
                            t.wmo_vtx = 0;
                            continue;
                        }
                    }
                }
            };
            cache.insert(model.clone(), mesh);
        }
        let total = cache[&model].indices.len() / 3;
        if total == 0 || cache[&model].positions.is_empty() {
            t.wmo_inst += 1;
            t.wmo_tri = 0;
            t.wmo_vtx = 0;
            t.wmo_world.clear();
            continue;
        }
        // Start the instance: transform its vertices once, in slices (a city
        // WMO runs to tens of thousands), then fold its triangles in chunks.
        if t.wmo_tri == 0 {
            let mesh = &cache[&model];
            if t.wmo_vtx == 0 {
                t.wmo_world.clear();
                t.wmo_world.reserve(mesh.positions.len());
            }
            let (q, base) = {
                let inst = &t.wmos[t.wmo_inst];
                (placement_quat(inst.rotation), inst.position)
            };
            let end = (t.wmo_vtx + 4096).min(mesh.positions.len());
            for p in &mesh.positions[t.wmo_vtx..end] {
                let r = q_rot(q, *p);
                t.wmo_world
                    .push([r[0] + base[0], r[1] + base[1], r[2] + base[2]]);
            }
            t.wmo_vtx = end;
            if t.wmo_vtx < mesh.positions.len() {
                break; // more vertices next tick
            }
        }
        let mut i = t.wmo_tri;
        {
            let world = &t.wmo_world;
            let mesh = &cache[&model];
            let cells = &mut t.wmo_cells;
            while i < total {
                let mesh_i = &mesh.indices[i * 3..i * 3 + 3];
                let (a, b, c) = (mesh_i[0] as usize, mesh_i[1] as usize, mesh_i[2] as usize);
                let (Some(&va), Some(&vb), Some(&vc)) = (world.get(a), world.get(b), world.get(c))
                else {
                    i += 1;
                    continue;
                };
                let tri_c = Tri::new(va, vb, vc);
                let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &tri_c);
                for ly in y0..=y1 {
                    for lx in x0..=x1 {
                        cells[(ly * WMO_GRID + lx) as usize].push(tri_c);
                    }
                }
                built = true;
                i += 1;
                if i % 128 == 0 && start.elapsed() >= budget {
                    break;
                }
            }
        }
        t.wmo_tri = i;
        if i < total {
            break; // budget spent mid-instance
        }
        t.wmo_inst += 1;
        t.wmo_tri = 0;
        t.wmo_vtx = 0;
        t.wmo_world.clear();
        if start.elapsed() >= budget {
            break;
        }
    }
    if t.wmo_inst >= t.wmos.len() && !t.wmo_ready {
        // WMO MLIQ: the canals' and fountains' water, model space + placement
        // (a rotation-safe query, so an arbitrary yaw still samples). The
        // worker parks each model's groups, so this is a cache hit in the
        // normal path; a tile nobody prefetched reads one model here.
        let mliq = std::time::Instant::now();
        while t.wmo_mliq < t.wmos.len() {
            if mliq.elapsed() >= budget {
                break;
            }
            let inst = t.wmos[t.wmo_mliq].clone();
            let key = crate::wmo_key(&inst.model);
            let meshes = match crate::wmo_liquid_lookup(&key) {
                Some(m) => m,
                None => {
                    let meshes = std::sync::Arc::new(crate::read_wmo_group_liquids(
                        Some(&mut *chain),
                        &inst.model,
                    ));
                    crate::wmo_liquid_store(&key, meshes.clone());
                    meshes
                }
            };
            for m in meshes.iter() {
                t.wmo_liquids.push((inst.clone(), m.clone()));
            }
            t.wmo_mliq += 1;
        }
        if t.wmo_mliq >= t.wmos.len() {
            t.wmo_ready = true;
            t.wmo_world = Vec::new();
        }
    }
    built
}

/// The liquid surface height under (x, y) in one liquid mesh, sampled
/// bilinearly over the containing cell as the client does.
/// `None` outside the mesh or over a dry cell.
///
/// The cell axes are taken from the vertex grid itself (vertex 0 to vertex 1
/// along the columns, vertex 0 to vertex `cols` along the rows), so the ADT's
/// south/east stepping grid and the WMO's north/west stepping one both sample
/// correctly, in any placement rotation.
fn liquid_height(m: &benilla_formats::LiquidMesh, x: f32, y: f32) -> Option<f32> {
    let cols = m.grid[0] as usize;
    let rows = m.grid[1] as usize;
    if cols < 2 || rows < 2 || m.positions.len() < cols * rows {
        return None;
    }
    let p0 = m.positions[0];
    let pc = m.positions[1];
    let pr = m.positions[cols];
    let proj = |v: [f32; 3]| -> f32 {
        let (dx, dy) = (x - p0[0], y - p0[1]);
        let (ax, ay) = (v[0] - p0[0], v[1] - p0[1]);
        let len2 = ax * ax + ay * ay;
        if len2 < 1.0e-9 {
            return -1.0;
        }
        (dx * ax + dy * ay) / len2
    };
    let (fc, fr) = (proj(pc), proj(pr));
    if fr < 0.0 || fc < 0.0 {
        return None;
    }
    let (row, col) = (fr.floor(), fc.floor());
    let (row, col) = (row as usize, col as usize);
    if row + 1 >= rows || col + 1 >= cols {
        return None;
    }
    if !m.wet[row * (cols - 1) + col] {
        return None;
    }
    let (u, v) = (fr - row as f32, fc - col as f32);
    let tl = m.positions[row * cols + col][2];
    let tr = m.positions[row * cols + col + 1][2];
    let bl = m.positions[(row + 1) * cols + col][2];
    let br = m.positions[(row + 1) * cols + col + 1][2];
    Some(tl * (1.0 - u) * (1.0 - v) + tr * (1.0 - u) * v + bl * u * (1.0 - v) + br * u * v)
}

/// Ray/triangle intersection (Möller-Trumbore, both faces): the ray length to
/// the hit, if any.
fn ray_tri(o: [f32; 3], d: [f32; 3], t: &Tri) -> Option<f32> {
    let [a, b, c] = t.v;
    let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let cross = |u: [f32; 3], v: [f32; 3]| {
        [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ]
    };
    let dot = |u: [f32; 3], v: [f32; 3]| u[0] * v[0] + u[1] * v[1] + u[2] * v[2];
    let pv = cross(d, e2);
    let det = dot(e1, pv);
    if det.abs() < 1.0e-8 {
        return None; // parallel
    }
    let inv = 1.0 / det;
    let tv = [o[0] - a[0], o[1] - a[1], o[2] - a[2]];
    let u = dot(tv, pv) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let qv = cross(tv, e1);
    let v = dot(d, qv) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = dot(e2, qv) * inv;
    (dist > 1.0e-4).then_some(dist)
}

/// Keep the nearer of two candidate hit distances, within `max`.
fn keep_nearest(best: &mut Option<f32>, d: Option<f32>, max: f32) {
    if let Some(d) = d {
        if d <= max && best.map_or(true, |b| d < b) {
            *best = Some(d);
        }
    }
}

/// Closest point on a triangle to `p` (Ericson's barycentric regions).
fn closest_on_tri(p: [f32; 3], t: &Tri) -> [f32; 3] {
    let [a, b, c] = t.v;
    let sub = |u: [f32; 3], v: [f32; 3]| [u[0] - v[0], u[1] - v[1], u[2] - v[2]];
    let dot = |u: [f32; 3], v: [f32; 3]| u[0] * v[0] + u[1] * v[1] + u[2] * v[2];
    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(p, a);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = sub(p, b);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return [a[0] + ab[0] * v, a[1] + ab[1] * v, a[2] + ab[2] * v];
    }
    let cp = sub(p, c);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return [a[0] + ac[0] * w, a[1] + ac[1] * w, a[2] + ac[2] * w];
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return [
            b[0] + (c[0] - b[0]) * w,
            b[1] + (c[1] - b[1]) * w,
            b[2] + (c[2] - b[2]) * w,
        ];
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    [
        a[0] + ab[0] * v + ac[0] * w,
        a[1] + ab[1] * v + ac[1] * w,
        a[2] + ab[2] * v + ac[2] * w,
    ]
}

/// Input held this frame (axes -1..1, buttons as 0/1).
#[derive(Default)]
struct Input {
    forward: f32,
    strafe: f32,
    /// The keyboard turn axis, left-positive (`TURN_LEFT` keys); the host sends
    /// it only while the right mouse button is up, when A/D turn instead of
    /// strafing.
    turn: f32,
    jump: bool,
    walk: bool,
    /// Vertical axis for roam/fly mode (-1..1): Space up, Ctrl down.
    up: f32,
}

pub struct Player {
    map: String,
    /// Feet, WoW world yards.
    pos: [f32; 3],
    /// Body facing (the aim): 0 looks along +X, π/2 along +Y. Only the right
    /// mouse button and the turn keys move it - WASD never does.
    yaw: f32,
    /// Rendered body heading: `yaw` plus the eased strafe counter-twist.
    model_yaw: f32,
    vel: [f32; 3],
    grounded: bool,
    /// Latched by the water depth over the feet; gravity is
    /// bypassed while set and the feet float at `0.75·h` under the surface.
    swimming: bool,
    /// Seconds since the last swim breach. A launch keeps the swim latch off
    /// until it halves (`0x7c5de0`: `vel_y > 0` within `speed/(2·g)`), so a
    /// deep breach hops and re-latches instead of flying a whole land arc.
    swim_hop_s: f32,
    input: Input,
    jump_was_down: bool,
    /// Camera yaw in the UE convention (0 = +X, clockwise positive) and pitch
    /// up-positive, radians.
    cam_yaw: f32,
    cam_pitch: f32,
    cam_distance: f32,
    cam_target_distance: f32,
    /// The framing pivot's live height above the feet, easing between the
    /// standing and swimming presets (`cam+0xfc`, 1.2 yd/s).
    pivot_height: f32,
    /// The liquid surface under the feet sampled this tick, kept for the
    /// camera's water corridor and boom clip.
    surface: Option<f32>,
    /// The live boom length: snaps in when geometry intrudes, eases back out.
    cam_live: f32,
    /// `UNIT_FIELD_MOUNTDISPLAYID` for our own guid, re-read each tick: the
    /// mount the server rides us on, 0 standing.
    server_mount: u32,
    /// The M key's preview mount, for a run with no server behind it.
    mount_preview: bool,
    mounted: bool,
    attack_left: f32,
    /// The server's combat latch (`SMSG_ATTACKSTART` with no stop): the swing
    /// cycle keeps repeating while set.
    attacking: bool,
    /// Seconds into the current weapon cycle; the attack overlay shows for the
    /// first [`SWING_WINDOW`].
    swing_phase: f32,
    /// The live cast the server started for us: (spell, elapsed, duration).
    cast: Option<(u32, f32, f32)>,
    /// Our own descriptor reads dead: the Death anim, and no locomotion.
    dead: bool,
    /// The overlay's eased weight, 0..1.
    overlay_weight: f32,
    /// The stock UI's held movement (forward, strafe, turn) while a binding owns
    /// the axes: `None` hands them back to the host's raw key state.
    ui_input: Option<[f32; 3]>,
    /// A `Jump()` binding's one-shot, spent on the next tick.
    ui_jump: bool,
    // ---- the airborne arc and its animation bracket (`player::arc`) ----
    /// Feet Z at the arc's launch (`StartFalling`'s `+0x7c`): the pre-step height,
    /// so a flat jump's descent never reads as a far fall.
    fall_start_z: f32,
    /// The arc's launch vertical speed (`+0xa0`): the jump speed, or 0 for a
    /// step-off, which is what splits the Jump bracket from a kept gait.
    jump_zspeed: f32,
    /// Seconds spent airborne on this arc.
    airborne_s: f32,
    /// The `FALLINGFAR` latch: `FALL_FAR_DROP` under the launch for a jumped arc,
    /// `FALL_FAR_TIME` airborne for a step-off; only a landing clears it.
    fall_far: bool,
    /// Seconds of this arc inside JumpStart's clip; past it the Jump hang plays.
    jump_t: f32,
    /// An arc is running (its rising and falling edges).
    airborne: bool,
    /// The landing one-shot's id (`jump_land_pick`'s 39 or 187), 0 when none.
    land_id: u32,
    /// Seconds of it left to play.
    land_left_s: f32,
    /// The walk toggle's state, `None` until a `ToggleRun()` binding sets it.
    ui_walk: Option<bool>,
    tiles: HashMap<(i32, i32), TileCol>,
    /// Doodad collision hulls, parsed once per model path (`None` = the model
    /// ships no hull, like grass or a canopy).
    hulls: HashMap<String, Option<benilla_formats::CollisionMesh>>,
    /// Tiles already asked for and found missing in the archives.
    missing: Vec<(i32, i32)>,
    /// Set when a tile's WMO floors are built: the spawn sits on the terrain
    /// under a city, so climb to the lowest walkable WMO floor over it.
    snap_pending: bool,
    /// A server pose was adopted and the body has not settled on a floor since.
    /// While the tile's WMO set is still folding, the room's floor is not in it
    /// yet; the body holds the server's Z instead of dropping through the hole
    /// the ADT leaves under an interior.
    hold_pose: bool,
    /// Last feet position that stood on a floor. The void guard restores it if
    /// the body ever falls far below: a hole in the collision data must never
    /// drop the player out of the world.
    last_ground: [f32; 3],
    /// Seconds spent falling with no floor under the feet (diagnostics).
    void_time: f32,
    /// Seconds since the last falling-trace line (diagnostics).
    fall_time: f32,
    /// Roam mode (F8): no gravity, no collision, no water - free flight for
    /// inspecting the world. The ICC/M2 body still follows the pose.
    fly: bool,
    /// A mouse button was held for a look drag this frame: the smart follow
    /// must not fight the drag by swinging the camera back behind the body.
    look_active: bool,
    /// The drag that owns the body this frame: the TurnOrAction channel (right
    /// button or `MouselookStart`), the reference's `mouselook`. Together with
    /// a keyboard turn it is the `steering` the standing chase freezes on.
    look_steer: bool,
    /// The body's own turn this frame (WoW yaw radians, left-positive), from
    /// the standing chase; 0 unless standing. The shuffle animation keys on
    /// it (`0x5fd3f0`), not on the turn keys.
    body_turn_step: f32,
}

impl Player {
    pub fn new(map: &str, x: f32, y: f32) -> Self {
        Self {
            map: map.to_string(),
            pos: [x, y, 0.0],
            yaw: 0.0,
            model_yaw: 0.0,
            vel: [0.0; 3],
            grounded: false,
            swimming: false,
            swim_hop_s: f32::MAX,
            input: Input::default(),
            jump_was_down: false,
            // UE yaw of "facing +X" is 0; looking down the +X axis in WoW.
            cam_yaw: 0.0,
            cam_pitch: 0.0,
            cam_distance: CAM_DIST_DEFAULT,
            cam_target_distance: CAM_DIST_DEFAULT,
            pivot_height: CAM_PIVOT_STAND,
            surface: None,
            cam_live: CAM_DIST_DEFAULT,
            server_mount: 0,
            mount_preview: false,
            mounted: false,
            attack_left: 0.0,
            attacking: false,
            swing_phase: 0.0,
            cast: None,
            dead: false,
            overlay_weight: 0.0,
            ui_input: None,
            ui_jump: false,
            fall_start_z: 0.0,
            jump_zspeed: 0.0,
            airborne_s: 0.0,
            fall_far: false,
            jump_t: 0.0,
            airborne: false,
            land_id: 0,
            land_left_s: 0.0,
            ui_walk: None,
            tiles: HashMap::new(),
            hulls: HashMap::new(),
            missing: Vec::new(),
            snap_pending: false,
            hold_pose: false,
            last_ground: [x, y, 0.0],
            void_time: 0.0,
            fall_time: 0.0,
            fly: false,
            look_active: false,
            look_steer: false,
            body_turn_step: 0.0,
        }
    }

    fn tile_of(x: f32, y: f32) -> (i32, i32) {
        let (cx, cy) = world_to_chunk(x, y);
        ((cx / 16) as i32, (cy / 16) as i32)
    }

    /// The continent this body's tiles were resolved against.
    fn map_of(&self) -> &str {
        &self.map
    }

    /// Adopt a server-authoritative pose: feet, heading, and a clean predictor
    /// (no carried velocity, no stale grounded/water latch).
    fn set_server_pose(&mut self, pos: [f32; 3], yaw: f32) {
        self.pos = pos;
        self.yaw = yaw;
        self.model_yaw = yaw;
        self.vel = [0.0; 3];
        self.grounded = false;
        self.swimming = false;
        // The server's feet can sit under the surface (an old saved pose, a
        // teleport into a spot the maps cut a step below); the snap lifts the
        // body once the floor set under it is whole.
        self.snap_pending = true;
        self.hold_pose = true;
        self.last_ground = pos;
        self.void_time = 0.0;
    }

    /// Load the tile under the player plus its neighbours, at most one new ADT
    /// per call (a full 3x3 would hitch on the first frame), then evict
    /// anything more than one tile away.
    fn ensure_tiles(&mut self, archives: &[benilla_mpq::Archive]) {
        let (tx, ty) = Self::tile_of(self.pos[0], self.pos[1]);
        let mut wanted = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                let k = (tx + dx, ty + dy);
                if k.0 < 0 || k.1 < 0 || k.0 > 63 || k.1 > 63 {
                    continue;
                }
                if self.tiles.contains_key(&k) || self.missing.contains(&k) {
                    continue;
                }
                wanted.push(k);
            }
        }
        // Closest first.
        wanted.sort_by_key(|k| (k.0 - tx).abs() + (k.1 - ty).abs());
        if let Some(&k) = wanted.first() {
            // The tile worker parks the collision tile beside the render one;
            // only a tile it never saw (spawn, teleport) is parsed here.
            let t0 = std::time::Instant::now();
            match crate::collision_tile_take(&self.map, k.0, k.1) {
                Some(t) => {
                    eprintln!("bn_player: collision tile ({},{}) parked", k.0, k.1);
                    self.tiles.insert(k, t);
                }
                None => match load_tile(archives, &self.map, k.0, k.1) {
                    Some(t) => {
                        eprintln!(
                            "bn_player: collision tile ({},{}) SYNC {:.1}ms",
                            k.0,
                            k.1,
                            t0.elapsed().as_secs_f64() * 1000.0
                        );
                        self.tiles.insert(k, t);
                    }
                    None => self.missing.push(k),
                },
            }
        }
        let evicted: Vec<(i32, i32)> = self
            .tiles
            .keys()
            .copied()
            .filter(|k| (k.0 - tx).abs() > 1 || (k.1 - ty).abs() > 1)
            .collect();
        for k in evicted {
            // Parked again for a walk back across the border.
            if let Some(t) = self.tiles.remove(&k) {
                crate::collision_tile_put(&self.map, k.0, k.1, t);
            }
        }
    }

    /// Fill the player tile's WMO floor/wall triangles once, from the same
    /// walking-collision gather benilla traces (every face but MOPY `DETAIL`),
    /// cached per model path.
    fn ensure_wmo(
        &mut self,
        chain: &mut Option<benilla_formats::Chain>,
        cache: &mut HashMap<String, benilla_formats::CollisionMesh>,
    ) {
        let (tx, ty) = Self::tile_of(self.pos[0], self.pos[1]);
        let Some(t) = self.tiles.get_mut(&(tx, ty)) else {
            return;
        };
        let Some(chain) = chain.as_mut() else { return };
        if !t.wmo_ready {
            let owed = t.wmo_inst < t.wmos.len();
            build_wmo_collision_step(t, tx, ty, chain, cache, std::time::Duration::from_millis(2));
            // The spawn snap is one decision, taken when the floor set is whole.
            // Re-running it on every folded slice climbed a body placed inside a
            // building onto its roof, one floor at a time.
            if owed && t.wmo_inst >= t.wmos.len() {
                self.snap_pending = true;
            }
        }
        // Doodad hulls: each placement folds its model's M2 collision hull in
        // as world triangles, the gather `placement_collider_data` builds. A
        // model with no hull (grass, a canopy) is walked through and stepped
        // under; a trunk's hull is solid and its sloped faces can be climbed.
        // The first tile cold-parses ~200 unique models, so this runs on a 2ms
        // budget per tick instead of all at once.
        let start = std::time::Instant::now();
        while t.doodad_next < t.doodads.len() {
            let i = t.doodad_next;
            if !self.hulls.contains_key(&t.doodads[i].model) {
                let parked = doodad_hull_parked(&t.doodads[i].model);
                if parked.is_none() && start.elapsed().as_millis() >= 2 {
                    // A cold model with nothing parked: leave the cursor on it
                    // for the next tick instead of parsing mid-frame.
                    break;
                }
                let model = t.doodads[i].model.clone();
                let hull = parked.unwrap_or_else(|| m2_collision_hull(chain, &model));
                self.hulls.insert(model, hull);
            }
            t.doodad_next = i + 1;
            let d = &t.doodads[i];
            let Some(Some(hull)) = self.hulls.get(&d.model) else {
                continue;
            };
            let q = placement_quat(d.rotation);
            let s = d.scale;
            let world: Vec<[f32; 3]> = hull
                .positions
                .iter()
                .map(|p| {
                    let r = q_rot(q, [p[0] * s, p[1] * s, p[2] * s]);
                    [
                        r[0] + d.position[0],
                        r[1] + d.position[1],
                        r[2] + d.position[2],
                    ]
                })
                .collect();
            for tri in hull.indices.as_chunks::<3>().0 {
                let (Some(&va), Some(&vb), Some(&vc)) = (
                    world.get(tri[0] as usize),
                    world.get(tri[1] as usize),
                    world.get(tri[2] as usize),
                ) else {
                    continue;
                };
                let tri_c = Tri::new(va, vb, vc);
                let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &tri_c);
                for ly in y0..=y1 {
                    for lx in x0..=x1 {
                        t.doodad_cells[(ly * WMO_GRID + lx) as usize].push(tri_c);
                    }
                }
            }
            if start.elapsed().as_millis() >= 2 {
                break;
            }
        }
    }

    /// The highest walkable surface under (x, y) at or below `ceil`.
    pub(crate) fn ground(&self, x: f32, y: f32, ceil: f32) -> Option<(f32, f32)> {
        let (tx, ty) = Self::tile_of(x, y);
        let mut best: Option<(f32, f32)> = None;
        let mut consider = |t: &Tri| {
            if let Some(z) = t.height_at(x, y, ceil) {
                if best.map_or(true, |(bz, _)| z > bz) {
                    best = Some((z, t.n[2].abs()));
                }
            }
        };
        if let Some(tile) = self.tiles.get(&(tx, ty)) {
            let (cx, cy) = world_to_chunk(x.clamp(-17066.0, 17066.0), y.clamp(-17066.0, 17066.0));
            let b = ((cy % 16) * 16 + (cx % 16)) as usize;
            for t in &tile.buckets[b] {
                consider(t);
            }
            if let Some(i) = TileCol::wmo_cell(tx, ty, x, y) {
                for t in &tile.wmo_cells[i] {
                    consider(t);
                }
                for t in &tile.doodad_cells[i] {
                    consider(t);
                }
            }
        } else {
            // Crossing a border before the neighbour finished loading: any
            // loaded tile still answers in world coords.
            for (&(k0, k1), tile) in &self.tiles {
                if let Some(i) = TileCol::wmo_cell(k0, k1, x, y) {
                    for t in &tile.wmo_cells[i] {
                        consider(t);
                    }
                    for t in &tile.doodad_cells[i] {
                        consider(t);
                    }
                }
                for bucket in &tile.buckets {
                    for t in bucket {
                        consider(t);
                    }
                }
            }
        }
        best
    }

    /// Whether the tile under (x, y) still owes its WMO floor set.
    /// `build_wmo_collision_step` folds a big model over many ticks (the
    /// 4096-vertex transform slice binds), and the terrain under an interior
    /// is holed in the ADT: until the fold finishes, a room's floor is not in
    /// the set and the probe answers with whatever lies below the building.
    /// A tile the archives lack never arrives, and is not owed.
    fn floors_folding(&self, x: f32, y: f32) -> bool {
        let k = Self::tile_of(x, y);
        match self.tiles.get(&k) {
            Some(t) => t.wmo_inst < t.wmos.len(),
            None => !self.missing.contains(&k),
        }
    }

    /// The terrain surface under (x, y): the ADT's own triangles, never a WMO
    /// floor or a doodad's hull. The portal cull's seed races this against the
    /// building floor (`0x6822a2`), so an interior under a hill stays inside
    /// while one below the open ground is dropped.
    pub(crate) fn terrain_z(&self, x: f32, y: f32) -> Option<f32> {
        let (tx, ty) = Self::tile_of(x, y);
        let mut best: Option<f32> = None;
        let mut consider = |t: &Tri| {
            if let Some(z) = t.height_at(x, y, f32::MAX) {
                best = Some(best.map_or(z, |bz| bz.max(z)));
            }
        };
        if let Some(tile) = self.tiles.get(&(tx, ty)) {
            let (cx, cy) = world_to_chunk(x.clamp(-17066.0, 17066.0), y.clamp(-17066.0, 17066.0));
            let b = ((cy % 16) * 16 + (cx % 16)) as usize;
            for t in &tile.buckets[b] {
                consider(t);
            }
        } else {
            // Crossing a border before the neighbour finished loading (or off
            // this body's tiles): every loaded bucket answers in world coords.
            for tile in self.tiles.values() {
                for bucket in &tile.buckets {
                    for t in bucket {
                        consider(t);
                    }
                }
            }
        }
        best
    }

    /// The floor a step-up would land on, advanced `STEP_UP_ADVANCE` from
    /// (`x`, `y`) along the travel: the reference's certify sweep (`0x636147`),
    /// settled onto a walkable floor higher than the feet. `None` means the
    /// steep face is a wall, not a step.
    fn step_up_landing(&self, x: f32, y: f32, dirx: f32, diry: f32) -> Option<f32> {
        if dirx == 0.0 && diry == 0.0 {
            return None;
        }
        let (ax, ay) = (x + dirx * STEP_UP_ADVANCE, y + diry * STEP_UP_ADVANCE);
        let (z, nz) = self.ground(ax, ay, self.pos[2] + STEP_UP_HEIGHT)?;
        if nz < GROUND_COS {
            return None; // only a standable floor certifies the step
        }
        (z - self.pos[2] > 0.05).then_some(z)
    }

    /// The liquid surface over (x, y), the lowest of any stacked blocks
    /// so a stream over the sea reads as the
    /// stream). Magma and slime are swum in like water.
    fn water(&self, x: f32, y: f32) -> Option<f32> {
        let (tx, ty) = Self::tile_of(x, y);
        let mut best: Option<f32> = None;
        let mut consider = |tile: &TileCol| {
            for m in &tile.liquids {
                if let Some(z) = liquid_height(m, x, y) {
                    best = Some(best.map_or(z, |b: f32| b.min(z)));
                }
            }
            for (inst, m) in &tile.wmo_liquids {
                // Mirror the query into the model, sample there, and rotate the
                // surface point back out: rotation-independent, so a building
                // placed at any yaw still answers.
                let q = placement_quat(inst.rotation);
                let local = q_rot(
                    [-q[0], -q[1], -q[2], q[3]],
                    [x - inst.position[0], y - inst.position[1], 0.0],
                );
                if let Some(lz) = liquid_height(m, local[0], local[1]) {
                    let wz = q_rot(q, [local[0], local[1], lz])[2] + inst.position[2];
                    best = Some(best.map_or(wz, |b: f32| b.min(wz)));
                }
            }
        };
        match self.tiles.get(&(tx, ty)) {
            Some(tile) => consider(tile),
            // Border frame with the neighbour still loading.
            None => self.tiles.values().for_each(consider),
        }
        best
    }

    /// A steep solid face within `radius` of `p` (a wall; floors are excluded).
    /// Every face the reference's body trace meets is here: terrain (and the
    /// impassable chunks' fences), WMO walk faces and doodad hulls, one
    /// audience. Faces entirely below `step_base + STEP_UP_HEIGHT` are steps,
    /// not walls: the client's mover climbs them, so its wall test must not
    /// see them. Pass `f32::NEG_INFINITY` to keep every face (the camera's
    /// probe).
    fn hit_steep(&self, p: [f32; 3], radius: f32, step_base: f32) -> bool {
        let (tx, ty) = Self::tile_of(p[0], p[1]);
        let r2 = radius * radius;
        let steep = |t: &Tri| -> bool {
            if t.n[2].abs() >= GROUND_COS {
                return false;
            }
            let top = t.v[0][2].max(t.v[1][2]).max(t.v[2][2]);
            if top <= step_base + STEP_UP_HEIGHT {
                return false; // a curb the mover steps over
            }
            let q = closest_on_tri(p, t);
            let d = [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
            d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < r2
        };
        let probe = |ktx: i32, kty: i32, tile: &TileCol| -> bool {
            let Some(i) = TileCol::wmo_cell(ktx, kty, p[0], p[1]) else {
                return false;
            };
            let (lx, ly) = (
                (i % WMO_GRID as usize) as i32,
                (i / WMO_GRID as usize) as i32,
            );
            for gy in (ly - 1)..=(ly + 1) {
                for gx in (lx - 1)..=(lx + 1) {
                    if !(0..WMO_GRID).contains(&gx) || !(0..WMO_GRID).contains(&gy) {
                        continue;
                    }
                    let cell = (gy * WMO_GRID + gx) as usize;
                    if tile.wmo_cells[cell].iter().any(steep)
                        || tile.doodad_cells[cell].iter().any(steep)
                    {
                        return true;
                    }
                }
            }
            // Terrain, and the fences of its impassable chunks, live in the
            // chunk buckets: the three-by-three chunks around the body.
            let (cx, cy) =
                world_to_chunk(p[0].clamp(-17066.0, 17066.0), p[1].clamp(-17066.0, 17066.0));
            let (cx, cy) = (cx as i32, cy as i32);
            for y in (cy - 1)..=(cy + 1) {
                for x in (cx - 1)..=(cx + 1) {
                    // The west/south map edge has no neighbour chunk, and the
                    // neighbour tile answers for its own chunks.
                    if x < 0 || y < 0 || x / 16 != ktx || y / 16 != kty {
                        continue;
                    }
                    let b = ((y % 16) * 16 + (x % 16)) as usize;
                    if tile.buckets[b].iter().any(steep) {
                        return true;
                    }
                }
            }
            false
        };
        if let Some(tile) = self.tiles.get(&(tx, ty)) {
            return probe(tx, ty, tile);
        }
        // Border frame with the tile still loading: scan what is loaded.
        self.tiles
            .iter()
            .any(|(&(k0, k1), tile)| probe(k0, k1, tile))
    }

    /// Horizontal blocking by a steep solid face: the client's capsule (a
    /// radius `CAPSULE_RADIUS` sweep between the feet and the head) against
    /// walls. Floors (`|n.z| >= GROUND_COS`) are left to [`Self::ground`].
    fn wall_hit(&self, x: f32, y: f32, feet: f32) -> bool {
        let lo = feet + CAPSULE_RADIUS;
        let hi = feet + CAPSULE_HEIGHT - CAPSULE_RADIUS;
        (0..3).any(|k| {
            self.hit_steep(
                [x, y, lo + (hi - lo) * (k as f32 / 2.0)],
                CAPSULE_RADIUS,
                feet,
            )
        })
    }

    /// The framing pivot for this frame: the standing/swimming preset above
    /// the feet, run through the water corridor.
    fn camera_pivot(&self) -> [f32; 3] {
        let target = if self.swimming {
            CAM_PIVOT_SWIM
        } else {
            CAM_PIVOT_STAND
        }
        .clamp(CAM_PIVOT_FLOOR, CAM_PIVOT_CEIL);
        let h = camera_pivot_height(target, self.pivot_height, self.surface, self.pos[2]);
        [self.pos[0], self.pos[1], self.pos[2] + h]
    }

    /// The orbit's forward (up-positive pitch) in WoW axes: the eye sits at
    /// `pivot - fwd * boom`.
    pub(crate) fn camera_fwd(&self) -> [f32; 3] {
        let (sp, cp) = self.cam_pitch.sin_cos();
        let (sy, cy) = self.cam_yaw.sin_cos();
        [cp * cy, -cp * sy, sp]
    }

    /// Where the boom first meets something solid, `max` yards out. Every WMO
    /// face counts, a roof exactly like a wall: leaving ceilings to a floor
    /// probe let the eye rise through a roof (and through the city wall's
    /// arch stones) instead of stopping under it. The terrain buckets and the
    /// doodads' hulls are solid the same way, so the camera's ray sweeps one
    /// world, not just the building the player stands in.
    fn boom_hit(&self, pivot: [f32; 3], dir: [f32; 3], max: f32) -> Option<f32> {
        let end = [
            pivot[0] + dir[0] * max,
            pivot[1] + dir[1] * max,
            pivot[2] + dir[2] * max,
        ];
        let mut best: Option<f32> = None;
        let mut scan = |tx: i32, ty: i32, tile: &TileCol| {
            let (ax, ay) = TileCol::wmo_local(tx, ty, pivot[0], pivot[1]);
            let (bx, by) = TileCol::wmo_local(tx, ty, end[0], end[1]);
            for gy in (ay.min(by) - WMO_MARGIN)..=(ay.max(by) + WMO_MARGIN) {
                for gx in (ax.min(bx) - WMO_MARGIN)..=(ax.max(bx) + WMO_MARGIN) {
                    if !(0..WMO_GRID).contains(&gx) || !(0..WMO_GRID).contains(&gy) {
                        continue;
                    }
                    for t in &tile.wmo_cells[(gy * WMO_GRID + gx) as usize] {
                        keep_nearest(&mut best, ray_tri(pivot, dir, t), max);
                    }
                    for d in &tile.doodad_cells[(gy * WMO_GRID + gx) as usize] {
                        keep_nearest(&mut best, ray_tri(pivot, dir, d), max);
                    }
                }
            }
        };
        let k0 = Self::tile_of(pivot[0], pivot[1]);
        let k1 = Self::tile_of(end[0], end[1]);
        match self.tiles.get(&k0) {
            Some(tile) => {
                scan(k0.0, k0.1, tile);
                if k1 != k0 {
                    if let Some(tile) = self.tiles.get(&k1) {
                        scan(k1.0, k1.1, tile);
                    }
                }
            }
            // Border frame with the tile still loading: scan what is loaded.
            None => {
                for (&(tx, ty), tile) in &self.tiles {
                    scan(tx, ty, tile);
                }
            }
        }
        // Terrain: the chunks the segment's XY span crosses, in whichever
        // tile owns them. A hillside stops the eye like a wall does.
        let (c0x, c0y) = world_to_chunk(
            pivot[0].clamp(-17066.0, 17066.0),
            pivot[1].clamp(-17066.0, 17066.0),
        );
        let (c1x, c1y) = world_to_chunk(
            end[0].clamp(-17066.0, 17066.0),
            end[1].clamp(-17066.0, 17066.0),
        );
        for cy in c0y.min(c1y)..=c0y.max(c1y) {
            for cx in c0x.min(c1x)..=c0x.max(c1x) {
                let (tx, ty) = (cx / 16, cy / 16);
                let Some(tile) = self.tiles.get(&(tx as i32, ty as i32)) else {
                    continue;
                };
                let b = ((cy % 16) * 16 + (cx % 16)) as usize;
                for t in &tile.buckets[b] {
                    keep_nearest(&mut best, ray_tri(pivot, dir, t), max);
                }
            }
        }
        best
    }

    /// The longest clear boom up to `max`: the first solid face along it, less
    /// the probe's radius (`CAMERA_PROBE_RADIUS`), so the eye keeps clear of
    /// the surface it stops at; floored at [`CAM_BOOM_MIN`] so the eye never
    /// ends up inside the body.
    fn solid_boom(&self, pivot: [f32; 3], fwd: [f32; 3], max: f32) -> f32 {
        if max <= 0.0 {
            return 0.0;
        }
        let dir = [-fwd[0], -fwd[1], -fwd[2]];
        let floor = CAM_BOOM_MIN.min(max);
        match self.boom_hit(pivot, dir, max) {
            Some(t) => (t - CAM_PROBE_RADIUS).max(floor),
            None => max,
        }
    }

    /// The lowest upward-facing WMO surface over (x, y) between `floor` and
    /// `floor + max_rise`: the street a spawn under a city should stand on
    /// (ADT terrain under a WMO footprint is holed, so the raw terrain height
    /// leaves the player below the city).
    fn lowest_floor_above(&self, x: f32, y: f32, floor: f32, max_rise: f32) -> Option<f32> {
        let (tx, ty) = Self::tile_of(x, y);
        let tile = self.tiles.get(&(tx, ty))?;
        let i = TileCol::wmo_cell(tx, ty, x, y)?;
        let mut best: Option<f32> = None;
        for t in &tile.wmo_cells[i] {
            if t.n[2] < GROUND_COS {
                continue; // a ceiling or a wall is not a street
            }
            if let Some(z) = t.height_at(x, y, floor + max_rise) {
                if z > floor + 0.1 && best.map_or(true, |b| z < b) {
                    best = Some(z);
                }
            }
        }
        best
    }

    /// Roam mode: fly where the body faces (the mouse still turns the aim),
    /// W/A/S/D level, Space/Ctrl vertical, with no gravity, collision or water.
    /// The camera keeps the third-person framing but skips the boom probe, so
    /// walls and floors never pull it into the body.
    fn tick_fly(&mut self, dt: f32) {
        let forward = self.input.forward.clamp(-1.0, 1.0);
        let strafe = self.input.strafe.clamp(-1.0, 1.0);
        let up = self.input.up.clamp(-1.0, 1.0);
        let turn = self.input.turn.clamp(-1.0, 1.0);
        if turn != 0.0 {
            let d = turn * TURN_RATE * TURN_RATE_MOVING * dt;
            self.yaw = wrap_pi(self.yaw + d);
            self.cam_yaw = wrap_pi(self.cam_yaw - d);
        }
        let (sy, cy) = self.yaw.sin_cos();
        let mut v = [cy * forward + sy * strafe, sy * forward - cy * strafe, up];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if len > 1.0 {
            for c in &mut v {
                *c /= len;
            }
        }
        for i in 0..3 {
            self.pos[i] += v[i] * FLY_SPEED * dt;
        }
        self.vel = [0.0; 3];
        self.grounded = false;
        self.swimming = false;
        self.surface = None;
        self.void_time = 0.0;
        self.fall_time = 0.0;
        // Body: the aim plus the walking side-step swing; a level flight
        // otherwise faces where it flies.
        let offset = strafe_body_offset(forward, strafe);
        if offset != 0.0 {
            self.model_yaw = ease_strafe_yaw(self.model_yaw, self.yaw, offset, dt);
        } else {
            self.model_yaw = self.yaw;
        }
        self.body_turn_step = 0.0;
        // Smart follow behind the aim, as on the ground.
        if forward.abs() + strafe.abs() > 0.01 {
            let target = wrap_pi(-self.yaw);
            let d = wrap_pi(target - self.cam_yaw);
            let max = FOLLOW_RATE * dt;
            self.cam_yaw = wrap_pi(self.cam_yaw + d.clamp(-max, max));
        }
        let glide = CAM_PIVOT_SMOOTH * dt;
        self.pivot_height += (CAM_PIVOT_STAND - self.pivot_height).clamp(-glide, glide);
        self.attack_left = (self.attack_left - dt).max(0.0);
        let step_d = 8.33 * dt;
        self.cam_distance += (self.cam_target_distance - self.cam_distance).clamp(-step_d, step_d);
        self.cam_live = self.cam_distance;
    }

    /// Read the server's combat, cast and death state for our own guid, and
    /// advance the swing cycle and the overlay's weight.
    fn sample_server_state(&mut self, dt: f32) {
        // The mount is the server's (`UNIT_FIELD_MOUNTDISPLAYID`); the local
        // M-key preview only stands in when no server field does.
        self.server_mount = crate::net::ui_self_mount_display();
        self.mounted = self.server_mount != 0 || self.mount_preview;
        self.attacking = crate::net::ui_self_attacking();
        self.cast = crate::net::ui_self_cast();
        self.dead = crate::net::ui_self_fields().is_some_and(|f| f.unit_is_dead());
        // One weapon cycle per swing (`BnNetPlayer.attack_time`); the overlay
        // shows for the cycle's first window.
        let period = crate::net::ui_player()
            .map(|p| p.attack_time as f32 / 1000.0)
            .unwrap_or(2.0)
            .max(0.4);
        if self.attacking && !self.mounted {
            self.swing_phase = (self.swing_phase + dt) % period;
        } else {
            self.swing_phase = 0.0;
        }
        let want = if !self.dead
            && !self.mounted
            && self.grounded
            && self.lower_body_committed()
            && (self.cast_active() || self.swing_active())
        {
            1.0
        } else {
            0.0
        };
        self.overlay_weight += (want - self.overlay_weight) * (1.0 - (-dt / OVERLAY_BLEND).exp());
        if self.overlay_weight < 0.01 {
            self.overlay_weight = 0.0;
        }
    }

    /// One frame: integrate benilla's locomotion, resolve the floor, then place
    /// the camera. `dt` is clamped like the client's frame slice.
    fn tick(&mut self, dt: f32) {
        // Consume the drag latch first: the host reports the held look button
        // on every frame, including frames with no pointer delta.
        let looking = self.look_active;
        self.look_active = false;
        let look_steer = self.look_steer;
        self.look_steer = false;
        let dt = dt.clamp(0.0, 0.1);
        if dt <= 0.0 {
            return;
        }
        self.sample_server_state(dt);
        self.swim_hop_s += dt;
        if self.dead {
            // A corpse neither walks nor jumps; the camera still follows.
            self.input = Input::default();
            self.vel = [0.0; 3];
            self.attack_left = 0.0;
            self.ui_jump = false;
        } else {
            // The stock UI's bindings own the movement axes while they hold one:
            // the world frame runs their Start/Stop bodies, and the host's raw
            // key state only fills in when none is held.
            if let Some([forward, strafe, turn]) = self.ui_input {
                self.input.forward = forward;
                self.input.strafe = strafe;
                self.input.turn = turn;
            }
            if let Some(walk) = self.ui_walk {
                self.input.walk = walk;
            }
            if self.ui_jump {
                self.input.jump = true;
                self.ui_jump = false;
            }
        }
        if self.fly {
            self.tick_fly(dt);
            return;
        }
        // The spawn snap waits for the floor set under the body: at login the
        // tile is still streaming, and a one-shot taken on a missing tile
        // lifts nothing and is then lost.
        if self.snap_pending && !self.floors_folding(self.pos[0], self.pos[1]) {
            self.snap_pending = false;
            // A body with a floor within a step under its feet is inside the
            // building, not under it: the idle cast settles it onto that floor
            // and the climb would take it to the next storey up.
            let near = self
                .ground(self.pos[0], self.pos[1], self.pos[2] + STEP_UP_HEIGHT)
                .is_some_and(|(z, _)| self.pos[2] - z <= STEP_UP_HEIGHT);
            if !near {
                // The street over a spawn under a city is the lowest WMO floor;
                // over open ground the surface itself is the terrain. Take the
                // lower of the two, so a spawn under a bridge does not climb
                // onto its deck when the bank beside it is nearer.
                let lift = [
                    self.lowest_floor_above(self.pos[0], self.pos[1], self.pos[2], 60.0),
                    self.terrain_z(self.pos[0], self.pos[1])
                        .filter(|z| *z > self.pos[2] + 0.1 && *z < self.pos[2] + 60.0),
                ]
                .into_iter()
                .flatten()
                .min_by(f32::total_cmp);
                if let Some(z) = lift {
                    self.pos[2] = z;
                    self.vel[2] = 0.0;
                    self.grounded = true;
                    self.last_ground = self.pos;
                }
            }
        }
        let feet = self.pos;
        let forward = self.input.forward.clamp(-1.0, 1.0);
        let strafe = self.input.strafe.clamp(-1.0, 1.0);

        // The swim latch: the depth over the feet against
        // `0.75·h`, held in a 1/36-yd band. A launch is not caught while it is
        // still rising inside `speed/(2·g)` of the take-off (`0x7c5de0`): a
        // swim jump hops about that far and re-latches there, so a deep breach
        // is a hop in the water, not a land jump out of it.
        let surface = self.water(self.pos[0], self.pos[1]);
        let enter = SWIM_DEPTH_FRAC * CAPSULE_HEIGHT;
        let hop_blocked = self.vel[2] > 0.0 && self.swim_hop_s < SWIM_JUMP_SPEED / (2.0 * GRAVITY);
        self.swimming = match surface {
            None => false,
            Some(s) => {
                let depth = s - self.pos[2];
                if self.swimming {
                    depth >= enter - SWIM_HYSTERESIS
                } else {
                    depth > enter && !hop_blocked
                }
            }
        };
        if self.swimming {
            // The client dismounts on entering deep water. A server mount is
            // cleared through its own field; only the preview drops here.
            self.mount_preview = false;
            self.mounted = false;
        }
        // The pivot channel: the standing preset, the swim drop in
        // water, eased at 1.2 yd/s.
        let pivot_target = if self.swimming {
            CAM_PIVOT_SWIM
        } else {
            CAM_PIVOT_STAND
        }
        .clamp(CAM_PIVOT_FLOOR, CAM_PIVOT_CEIL);
        let glide = CAM_PIVOT_SMOOTH * dt;
        self.pivot_height += (pivot_target - self.pivot_height).clamp(-glide, glide);
        self.surface = surface;
        let backpedal = forward < 0.0 && strafe.abs() < 0.01;

        // The 1.12 control model: A/D turn the character unless the
        // right mouse button is held, when the host reports them as strafe. A
        // keyboard turn is left-positive and carries the aim and the camera
        // rigidly (`camera`); the rendered body is the standing chase's and
        // lags behind (`player/gait.rs`); translating or falling scales the
        // turn by 0.75.
        let turn = self.input.turn.clamp(-1.0, 1.0);
        if turn != 0.0 {
            let rate = TURN_RATE
                * if forward != 0.0 || strafe != 0.0 || !self.grounded {
                    TURN_RATE_MOVING
                } else {
                    1.0
                };
            let d = turn * rate * dt;
            self.yaw = wrap_pi(self.yaw + d);
            self.cam_yaw = wrap_pi(self.cam_yaw - d);
        }

        // WASD moves in the aim's basis, as the client does (`state`:
        // "the facing... is the basis WASD move in"); orbiting the camera
        // does not steer the body. WoW axes: +X north, +Y west, so facing
        // yaw gives forward (cos, sin) and the player's right (sin, -cos).
        let (sy, cy) = self.yaw.sin_cos();
        let fwd = [cy, sy];
        let right = [sy, -cy];
        let mag;
        if self.swimming {
            // The travel basis: forward is the facing pitched by
            // the aim, strafe stays level, and there is no vertical input.
            let (sp, cp) = self.cam_pitch.sin_cos();
            let mut v = [
                fwd[0] * cp * forward + right[0] * strafe,
                fwd[1] * cp * forward + right[1] * strafe,
                sp * forward,
            ];
            let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if len > 1.0 {
                for c in &mut v {
                    *c /= len;
                }
            }
            mag = len.min(1.0);
            let speed = if forward < -0.01 {
                SWIM_BACK_SPEED
            } else {
                SWIM_SPEED
            };
            self.vel = [v[0] * speed, v[1] * speed, v[2] * speed];
            // The rise cap: the stroke's upward speed is clamped
            // to what reaches the rest line this frame; the surplus turns into
            // level travel at the same speed.
            if let Some(s) = surface {
                let cap = ((s - enter - self.pos[2]) / dt).max(0.0);
                if self.vel[2] > cap {
                    let speed = (self.vel[0] * self.vel[0]
                        + self.vel[1] * self.vel[1]
                        + self.vel[2] * self.vel[2])
                        .sqrt();
                    let hl = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
                    let level = (speed * speed - cap * cap).max(0.0).sqrt();
                    if hl > 1.0e-6 {
                        self.vel[0] *= level / hl;
                        self.vel[1] *= level / hl;
                    } else {
                        self.vel[0] = 0.0;
                        self.vel[1] = 0.0;
                    }
                    self.vel[2] = cap;
                }
            }
        } else {
            let mut dir = [
                fwd[0] * forward + right[0] * strafe,
                fwd[1] * forward + right[1] * strafe,
            ];
            let len = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
            if len > 1.0 {
                dir[0] /= len;
                dir[1] /= len;
            }
            mag = len.min(1.0);
            let mut speed = RUN_SPEED;
            if self.mounted {
                speed *= MOUNT_SPEED_RATIO;
            } else if self.input.walk {
                speed *= WALK_RATIO;
            } else if backpedal {
                speed *= RUN_BACK_RATIO;
            }
            self.vel[0] = dir[0] * speed;
            self.vel[1] = dir[1] * speed;
        }

        // Smart follow (`cameraSmoothStyle` default): while the character is
        // driven, the camera returns behind the aim at `cameraYawSmoothSpeed`.
        if mag > 0.01 && !looking {
            let target = wrap_pi(-self.yaw); // UE yaw is the mirror of WoW yaw
            let d = wrap_pi(target - self.cam_yaw);
            let max = FOLLOW_RATE * dt;
            self.cam_yaw = wrap_pi(self.cam_yaw + d.clamp(-max, max));
        }

        // Jump: an edge. In water Space is the swim jump, which
        // clears the latch with a stronger seed at any depth.
        let jump = self.input.jump && !self.jump_was_down;
        self.jump_was_down = self.input.jump;
        if jump {
            if self.swimming {
                self.swimming = false;
                self.vel[2] = SWIM_JUMP_SPEED;
                self.grounded = false;
                self.swim_hop_s = 0.0;
                self.pos[2] += 0.05;
            } else if self.grounded {
                self.vel[2] = JUMP_SPEED;
                self.grounded = false;
                self.pos[2] += 0.05;
            }
        }

        // Horizontal step with the client's wall response: a floor more
        // than a step above the feet blocks the axis; the other one slides.
        let step = [self.vel[0] * dt, self.vel[1] * dt];
        if step[0] != 0.0 || step[1] != 0.0 {
            let len = (step[0] * step[0] + step[1] * step[1]).sqrt();
            let (dirx, diry) = (step[0] / len, step[1] / len);
            let blocked = |dx: f32, dy: f32| -> bool {
                let (nx, ny) = (self.pos[0] + dx, self.pos[1] + dy);
                if self.wall_hit(nx, ny, self.pos[2]) {
                    return true;
                }
                let Some((z, nz)) = self.ground(nx, ny, self.pos[2] + STEP_UP_HEIGHT) else {
                    // Nothing within reach at the destination. If the terrain
                    // there stands above the step reach, it is a face too steep
                    // to climb and the body must not enter it: walking it in
                    // sinks the capsule under the surface, where `ground` finds
                    // no floor above the feet and the body drops out of the
                    // world. Impassable chunks add a fence for the same reason
                    // (`0x6aae2a`); walkable steep faces are stopped here, the
                    // way the reference's slope test stops them. A destination
                    // over a drop answers below the feet instead, so walking off
                    // a cliff still falls.
                    return self
                        .terrain_z(nx, ny)
                        .is_some_and(|t| t > self.pos[2] + STEP_UP_HEIGHT);
                };
                let rise = z - self.pos[2];
                if rise > STEP_UP_HEIGHT - 1.0e-3 {
                    return true;
                }
                // A walkable face never blocks, and neither does a steep one at
                // or under the feet: walking off a kerb follows its bevel down
                // (`0x6367b0`'s snap, not the wall test).
                if nz >= GROUND_COS || rise <= 0.0 {
                    return false;
                }
                // A steep edge rising inside the foot cone is ridden up its
                // skirt; a higher one must certify a step-up landing ahead.
                rise > FOOT_CONE_HEIGHT && self.step_up_landing(nx, ny, dirx, diry).is_none()
            };
            if !blocked(step[0], step[1]) {
                self.pos[0] += step[0];
                self.pos[1] += step[1];
            } else if step[0] != 0.0 && !blocked(step[0], 0.0) {
                self.pos[0] += step[0];
            } else if step[1] != 0.0 && !blocked(0.0, step[1]) {
                self.pos[1] += step[1];
            } else {
                self.vel[0] = 0.0;
                self.vel[1] = 0.0;
            }
        }

        if self.swimming {
            // The floating resolver: no gravity, the vertical only
            // from the stroke. The feet sink to the rest line
            // `surface - 0.75·h` when the surface falls, a shallow bottom still
            // holds them higher, and the bed is solid from either side: a dive
            // stops on its surface instead of sinking through the terrain.
            let mut z = self.pos[2] + self.vel[2] * dt;
            if let Some(s) = surface {
                let rest = s - enter;
                // Read the bed from the feet's own ceiling, so a fast dive
                // cannot carry the candidate under it and miss it.
                let bed = self
                    .ground(self.pos[0], self.pos[1], self.pos[2] + STEP_UP_HEIGHT)
                    .map(|(b, _)| b);
                z = z.min(rest);
                if let Some(b) = bed {
                    z = z.max(b);
                }
            }
            self.pos[2] = z;
            self.grounded = false;
            self.vel[2] = 0.0;
            // Water ends the arc: the swim picks are their own clips and the
            // bracket clock (`tick_arc`) does not run here.
            self.airborne = false;
        } else {
            // Vertical: gravity then the floor probe (benilla's order).
            // The probe's ceiling has to cover the whole step, not just the
            // new feet: at terminal velocity a hitched 0.1s frame drops 6 yd,
            // so probing from `pos.z + STEP_UP_HEIGHT` lets the feet pass a
            // floor by more than 1 yd in one tick - the floor is then above
            // the ceiling, `ground` misses it and the body leaves the world.
            // Probing from the pre-gravity feet finds every surface crossed
            // this frame, and the `pos.z <= z` test below snaps onto it.
            let prev_z = self.pos[2];
            self.vel[2] = (self.vel[2] - GRAVITY * dt).max(-TERMINAL_VELOCITY);
            self.pos[2] += self.vel[2] * dt;
            // A step-down follows the surface only as far as the foot cone
            // reaches this frame (`0x6367b0`: `travel · STEP_SLOPE_RATIO +
            // 1/36`); a floor deeper than a step is a drop, and the body falls
            // onto it. The one-frame snap buried the capsule in the face and
            // the wall test then held it there.
            let travel = ((self.pos[0] - feet[0]).powi(2) + (self.pos[1] - feet[1]).powi(2)).sqrt();
            let cone = travel * STEP_SLOPE_RATIO + STEP_SNAP_SLACK;
            let ceil = self.pos[2].max(prev_z) + STEP_UP_HEIGHT;
            let found = self.ground(self.pos[0], self.pos[1], ceil);
            // A tile's WMO floor set folds over many ticks and the terrain under
            // an interior is holed, so a body that walks into a room during the
            // fold finds no floor (or only the ground far below it) and drops
            // through a hole the data does not have. While the set is still
            // owed, a standing body holds its seat - the rule `bn_ground_clamp_z`
            // gives units - and `snap_pending` settles it on the baked floor.
            let stale = (self.grounded || self.hold_pose)
                && self.floors_folding(self.pos[0], self.pos[1])
                && found.map_or(true, |(z, _)| prev_z - z > STEP_UP_HEIGHT);
            if stale {
                self.pos[2] = prev_z;
                self.vel[2] = 0.0;
                self.grounded = true;
                self.last_ground = self.pos;
                self.void_time = 0.0;
            } else {
                self.hold_pose = false;
                match found {
                    Some((z, _)) => {
                        self.void_time = 0.0;
                        let step_down =
                            self.grounded && z <= prev_z && prev_z - z <= STEP_UP_HEIGHT;
                        if self.pos[2] <= z || step_down {
                            // A step-down follows the surface at the cone's slope,
                            // one frame at a time, and only settles onto it once the
                            // feet are close; a whole ledge at once buried the
                            // capsule in the face.
                            self.pos[2] = if self.pos[2] <= z {
                                z
                            } else {
                                z.max(prev_z - cone)
                            };
                            self.vel[2] = 0.0;
                            self.grounded = true;
                            self.last_ground = self.pos;
                        } else {
                            self.grounded = false;
                        }
                    }
                    None => {
                        // No floor under the feet at all: a hole in the terrain
                        // (a tunnel mouth), a tile that has not streamed in yet, or
                        // a body that is already below the world. Never keep
                        // falling forever - report the spot once and restore the
                        // last floor when the fall is clearly out of the map.
                        self.grounded = false;
                        self.void_time += dt;
                        if self.void_time > 0.5 && self.void_time - dt <= 0.5 {
                            let (tx, ty) = Self::tile_of(self.pos[0], self.pos[1]);
                            let state = match self.tiles.get(&(tx, ty)) {
                                None => "missing",
                                Some(t) if t.wmo_ready => "ready",
                                Some(_) => "loading",
                            };
                            eprintln!(
                                "bn_player: no ground at ({:.1},{:.1}) z={:.1} tile=({tx},{ty}) \
                             {state} resident={}",
                                self.pos[0],
                                self.pos[1],
                                self.pos[2],
                                self.tiles.len()
                            );
                        }
                        if self.pos[2] < self.last_ground[2] - VOID_GUARD_DEPTH
                            // A tile whose floor set is complete (or will never
                            // arrive) holds every surface it will ever have: a
                            // body with nothing under it there is out of the
                            // world, and the 100-yd plunge only makes the exit
                            // longer.
                            || (self.void_time > 0.5
                                && !self.floors_folding(self.pos[0], self.pos[1]))
                        {
                            // The last seat may itself be in the void - a login
                            // pose saved under the terrain by a client that had
                            // already left the map - so restore onto a floor
                            // that is still real there, not blindly there.
                            let seat = self
                                .ground(
                                    self.last_ground[0],
                                    self.last_ground[1],
                                    self.last_ground[2] + STEP_UP_HEIGHT,
                                )
                                .or_else(|| {
                                    self.ground(self.last_ground[0], self.last_ground[1], f32::MAX)
                                })
                                .map_or(self.last_ground, |(z, _)| {
                                    [self.last_ground[0], self.last_ground[1], z]
                                });
                            eprintln!(
                            "bn_player: void guard restored ({:.1},{:.1},{:.1}) -> ({:.1},{:.1},{:.1})",
                            self.pos[0], self.pos[1], self.pos[2],
                            seat[0], seat[1], seat[2]
                        );
                            self.pos = seat;
                            self.last_ground = seat;
                            self.vel = [0.0; 3];
                            self.grounded = true;
                            self.void_time = 0.0;
                        }
                    }
                }
            }
            if self.grounded {
                self.pos[2] = self.pos[2].max(feet[2] - STEP_UP_HEIGHT);
            }
            self.tick_arc(dt, jump, prev_z);
            // Falling-trace: a body that drops through a city floor lands on the
            // terrain, where `ground` still answers - so the no-ground warning
            // never fires. Log the trajectory (with the tile's WMO cell count)
            // every half second instead, so a "fell through here" report has
            // coordinates and tells us whether the cell held any geometry.
            self.fall_time = if self.grounded {
                0.0
            } else {
                self.fall_time + dt
            };
            if !self.grounded && self.vel[2] < -10.0 && self.fall_time >= 0.5 {
                self.fall_time = 0.0;
                let (tx, ty) = Self::tile_of(self.pos[0], self.pos[1]);
                let (ready, wmos, cell_tris) = match self.tiles.get(&(tx, ty)) {
                    Some(t) => (
                        t.wmo_ready,
                        t.wmos.len(),
                        TileCol::wmo_cell(tx, ty, self.pos[0], self.pos[1])
                            .map_or(0, |i| t.wmo_cells[i as usize].len()),
                    ),
                    None => (false, 0, 0),
                };
                eprintln!(
                    "bn_player: falling ({:.1},{:.1}) z={:.2} floor={:?} tile=({tx},{ty}) \
                     wmo_ready={ready} wmos={wmos} celltris={cell_tris}",
                    self.pos[0],
                    self.pos[1],
                    self.pos[2],
                    found.map(|(z, _)| z)
                );
            }
        }

        // The rendered body heading (`player/gait.rs`'s `drive_body_heading`,
        // the `0x607ed0` tail): a swimmer faces its aim; a strafing body
        // eases to the aim plus the side-step offset (±π/2 pure, ±π/4
        // diagonal, mirrored backpedaling); forward/back travel and a fall
        // snap it; standing, it holds a 90° ceiling behind a steered aim and
        // sweeps onto it at 8× the turn rate once the steer stops. Run after
        // the floor resolve, on this frame's airborne state, as the reference
        // does.
        let strafe_offset = strafe_body_offset(forward, strafe);
        if self.swimming {
            self.model_yaw = self.yaw;
            self.body_turn_step = 0.0;
        } else if strafe_offset != 0.0 {
            self.model_yaw = ease_strafe_yaw(self.model_yaw, self.yaw, strafe_offset, dt);
            self.body_turn_step = 0.0;
        } else if forward.abs() > 0.01 || strafe.abs() > 0.01 || !self.grounded {
            self.model_yaw = self.yaw;
            self.body_turn_step = 0.0;
        } else {
            self.body_turn_step = stationary_chase_step(
                wrap_pi(self.yaw - self.model_yaw),
                turn != 0.0 || look_steer,
                dt,
            );
            self.model_yaw = wrap_pi(self.model_yaw + self.body_turn_step);
        }

        self.attack_left = (self.attack_left - dt).max(0.0);

        // Zoom glide (`camera_zoom`: constant 8.33 yd/s toward the target).
        let step_d = 8.33 * dt;
        self.cam_distance += (self.cam_target_distance - self.cam_distance).clamp(-step_d, step_d);

        // Boom collision: snap in at once when geometry intrudes (a wall the
        // player turns into), ease back out at the zoom glide's 8.33 yd/s.
        let want = self.solid_boom(self.camera_pivot(), self.camera_fwd(), self.cam_distance);
        self.cam_live = if want <= self.cam_live {
            want
        } else {
            (self.cam_live + 8.33 * dt).min(want)
        };
    }

    /// The camera eye/target for this frame: the framing pivot at the model
    /// preset above the feet (clamped by the water corridor), the eye on the
    /// orbit at the current pitch/yaw, at the boom length the collision probe
    /// settled on, clipped by the waterline.
    pub(crate) fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let pivot = self.camera_pivot();
        let fwd = self.camera_fwd();
        let mut dist = self.cam_live.min(self.cam_distance);
        let mut eye = [
            pivot[0] - fwd[0] * dist,
            pivot[1] - fwd[1] * dist,
            pivot[2] - fwd[2] * dist,
        ];
        // `cameraWaterCollision`'s ray: the waterline is solid, so a boom that
        // would put the eye under the surface stops at it instead.
        if let Some(s) = self.surface {
            if eye[2] < s && pivot[2] > s {
                let t = (pivot[2] - s) / (pivot[2] - eye[2]);
                dist *= t.max(0.0);
                eye = [
                    pivot[0] - fwd[0] * dist,
                    pivot[1] - fwd[1] * dist,
                    pivot[2] - fwd[2] * dist,
                ];
            }
        }
        (eye, pivot)
    }

    /// Mouse look: `dx`/`dy` are raw pointer counts (`LOOK_SENSITIVITY` rad
    /// each). Called every frame a button is held, so a still drag latches
    /// `look_active`. With `turn_body` the aim turns with the camera, as
    /// `camera` does on the turn-and-action button; the rendered body is the
    /// standing chase's to move (`player/gait.rs`), so it lags here. Without
    /// it only the orbit moves.
    fn look(&mut self, dx: f32, dy: f32, turn_body: bool) {
        // The button is held this frame (the host calls even for a zero
        // delta), so the smart follow yields to the drag.
        self.look_active = true;
        self.look_steer = turn_body;
        let turn = dx * LOOK_SENSITIVITY;
        if turn_body {
            // `cam_yaw` is UE yaw, the mirror of the WoW aim: the same turn
            // has the opposite sign, so the camera stays locked to the aim.
            self.yaw = wrap_pi(self.yaw - turn);
        }
        self.cam_yaw = wrap_pi(self.cam_yaw + turn);
        // `mouseInvertPitch` is off by default, so a downward drag gives a negative
        // pitch and the view drops: `let d_pitch = -dy * pitch_rate`
        // (`player/camera.rs`'s look rotation).
        self.cam_pitch =
            (self.cam_pitch - dy * LOOK_SENSITIVITY).clamp(-CAM_PITCH_LIMIT, CAM_PITCH_LIMIT);
    }

    /// Whether the lower body is committed to locomotion, the vanilla
    /// `ROUTE_COMMITTED_MOVE` gate that masks a one-shot onto the upper body:
    /// translating, turning, swimming or airborne.
    fn lower_body_committed(&self) -> bool {
        self.swimming
            || !self.grounded
            || self.input.forward.abs() > 0.01
            || self.input.strafe.abs() > 0.01
            || self.input.turn.abs() > 0.01
            || (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt() > 0.1
    }

    /// The swing edge: the server's attack latch at the head of a weapon
    /// cycle, or the host's local unarmed test press.
    fn swing_active(&self) -> bool {
        (self.attacking && self.swing_phase < SWING_WINDOW) || self.attack_left > 0.0
    }

    /// A cast with a cast time, still running.
    fn cast_active(&self) -> bool {
        matches!(self.cast, Some((_, elapsed, duration)) if duration > 0.0 && elapsed < duration)
    }

    /// The upper-body overlay `(anim, weight, elapsed)`: a one-shot the
    /// committed lower body cannot play full-body rides the masked spine
    /// subtree instead, resumed at `elapsed` seconds into the clip. The clock
    /// is the one that drove the whole-body clip (the swing's phase into the
    /// weapon cycle, the cast's elapsed, or the local press's spent window), so
    /// the torso picks the swing up where the legs interrupted it.
    fn overlay(&self) -> (u32, f32, f32) {
        if self.dead || self.mounted || !self.grounded || !self.lower_body_committed() {
            return (0, 0.0, 0.0);
        }
        let (anim, elapsed) = if self.cast_active() {
            (ANIM_SPELL_CAST, self.cast.map_or(0.0, |(_, e, _)| e))
        } else if self.swing_active() {
            let elapsed = if self.attacking && self.swing_phase < SWING_WINDOW {
                self.swing_phase
            } else {
                (SWING_WINDOW - self.attack_left).max(0.0)
            };
            (crate::net::self_swing_anim(), elapsed)
        } else {
            return (0, 0.0, 0.0);
        };
        (anim, self.overlay_weight, elapsed)
    }

    /// The airborne arc and its bracket clock, once per non-swim frame after the
    /// mover has written `pos` and `grounded` (`player::arc::advance_airborne_arc`).
    /// `launch_z` is the pre-step feet, the true takeoff height: the post-step one
    /// would read a flat jump's descent as a far fall.
    fn tick_arc(&mut self, dt: f32, jumped: bool, launch_z: f32) {
        let airborne = !self.grounded;
        // The arc starts at the launch, not only at the airborne edge: on broken
        // ground a jump can land and relaunch within one frame.
        if airborne && (!self.airborne || jumped) {
            self.airborne = true;
            self.airborne_s = 0.0;
            self.jump_t = 0.0;
            // `StartFalling`'s `+0xa0`: the jump speed for a launch, exactly 0 for
            // a step-off, which is the split the selector makes.
            self.jump_zspeed = if jumped { JUMP_SPEED } else { 0.0 };
            self.fall_start_z = launch_z;
            self.fall_far = false;
            self.land_id = 0;
            self.land_left_s = 0.0;
        } else if !airborne && self.airborne {
            self.airborne = false;
            self.jump_zspeed = 0.0;
            self.fall_far = false;
            self.airborne_s = 0.0;
            // Touchdown from a bracketed arc takes `jump_land_pick`'s one-shot
            // (`0x602c60`): JumpEnd 39 stopped, JumpLandRun 187 running forward,
            // nothing backpedaling or walking; an arc that kept its gait lands in it.
            self.land_id = if jumped {
                0
            } else {
                let hs = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
                if hs < 0.1 {
                    39
                } else if self.input.forward > 0.01 && !self.input.walk {
                    187
                } else {
                    0
                }
            };
            self.land_left_s = if self.land_id != 0 {
                LAND_MS / 1000.0
            } else {
                0.0
            };
        }
        if airborne {
            self.airborne_s += dt;
            if self.jump_zspeed > JUMP_ARC_MIN_UP {
                self.jump_t += dt;
            }
            // FALLINGFAR splits on the launch: a jump by the drop below its
            // takeoff, a step-off by the time airborne. Only a landing clears it.
            self.fall_far |= if self.jump_zspeed != 0.0 {
                self.pos[2] <= self.fall_start_z - FALL_FAR_DROP
            } else {
                self.airborne_s >= FALL_FAR_TIME
            };
        } else if self.land_left_s > 0.0 {
            self.land_left_s -= dt;
            if self.land_left_s <= 0.0 {
                self.land_id = 0;
                self.land_left_s = 0.0;
            }
        }
    }

    /// The mount's own gait: the rider holds Mount (91) while the mount walks
    /// or runs beneath it, on the same speed split the walk cycle uses.
    fn mount_anim(&self) -> u32 {
        if !self.mounted {
            return 0;
        }
        let hs = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
        if hs < 0.1 {
            0
        } else if hs > 2.0 * 2.5 {
            5 // Run
        } else {
            4 // Walk
        }
    }

    fn anim(&self) -> u32 {
        if self.dead {
            return ANIM_DEATH;
        }
        if self.mounted {
            return 91; // Mount
        }
        let f = self.input.forward;
        let s = self.input.strafe;
        let turning = self.input.turn;
        if self.swimming {
            // `creature_anim::select`'s swim picks: turn -> SwimIdle 41,
            // strafe -> SwimLeft 43 / SwimRight 44, back -> SwimBackwards 45,
            // forward -> Swim 42.
            if turning != 0.0 {
                return 41;
            }
            if s < -0.01 {
                return 43;
            }
            if s > 0.01 {
                return 44;
            }
            if f < -0.01 {
                return 45;
            }
            if f > 0.01 {
                return 42;
            }
            return 41;
        }
        // A one-shot with the legs free plays full-body; with them committed
        // it moves to `overlay()` and the locomotion anim stays the base.
        if !self.lower_body_committed() {
            if self.cast_active() {
                return ANIM_SPELL_CAST;
            }
            if self.swing_active() {
                return crate::net::self_swing_anim();
            }
        }
        if self.airborne {
            // `creature_anim::select::current_special`: the Fall loop plays only
            // once the fall is far; a jumped arc stays in the Jump bracket for its
            // whole arc - JumpStart (37) until that clip's authored length, then
            // the Jump hang (38) - and a step-off that is not yet far keeps its
            // gait (`0x5fd8e8`), so it falls through to the pick below.
            if self.fall_far {
                return 40;
            }
            if self.jump_zspeed > JUMP_ARC_MIN_UP {
                return if self.jump_t * 1000.0 < JUMP_ENTER_MS {
                    37
                } else {
                    38
                };
            }
        } else if self.land_id != 0 {
            return self.land_id;
        }
        let hs = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
        if hs < 0.1 {
            // Turning in place plays the shuffle (11 left, 12 right), keyed on
            // the body's own rotation (`0x5fd3f0`), not the turn keys: the
            // body is planted below the 90° ceiling, and the release sweep
            // keeps shuffling until it lands on the aim.
            if self.body_turn_step > 1.0e-5 {
                return 11;
            }
            if self.body_turn_step < -1.0e-5 {
                return 12;
            }
            return 0; // Stand
        }
        if f < -0.01 {
            13 // WalkBackwards dominates strafe
        } else if hs > 2.0 * 2.5 {
            5 // Run above twice the walk speed
        } else {
            4 // Walk
        }
    }

    fn state(&self) -> BnPlayerState {
        let (eye, target) = self.camera();
        let (overlay_anim, overlay_weight, overlay_elapsed) = self.overlay();
        let (twist_spine, twist_head) =
            armed_twist_shares(wrap_pi(self.yaw - self.model_yaw), self.mounted);
        BnPlayerState {
            pos: self.pos,
            yaw: self.model_yaw,
            cam_eye: eye,
            cam_target: target,
            cam_pitch: self.cam_pitch,
            cam_distance: self.cam_distance,
            speed: (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt(),
            grounded: self.grounded as u32,
            mounted: self.mounted as u32,
            mount_display: self.server_mount,
            mount_anim: self.mount_anim(),
            anim: self.anim(),
            overlay_anim,
            overlay_weight,
            overlay_anim_ms: (overlay_elapsed * 1000.0) as u32,
            twist_spine,
            twist_head,
            aim_yaw: self.yaw,
            cam_yaw: self.cam_yaw,
            axes: [self.input.forward, self.input.strafe, self.input.turn],
        }
    }
}

/// Parse one ADT into collision triangles (world coords), bucketed by chunk.
fn load_tile(archives: &[benilla_mpq::Archive], map: &str, tx: i32, ty: i32) -> Option<TileCol> {
    let path = format!("World\\Maps\\{map}\\{map}_{tx}_{ty}.adt");
    let bytes = archives.iter().rev().find_map(|a| {
        if a.contains(&path) {
            a.read_file(&path).ok()
        } else {
            None
        }
    })?;
    Some(tile_col_from_mesh(&adt_to_tile_mesh(&bytes).ok()?))
}

/// The collision half of an ADT, from the mesh the tile worker already parsed.
pub(crate) fn tile_col_from_mesh(mesh: &benilla_formats::TileMesh) -> TileCol {
    let mut col = TileCol::new();
    col.wmos = mesh.wmos.clone();
    col.doodads = mesh.doodads.clone();
    for c in &mesh.chunks {
        col.liquids.extend(c.liquids.iter().cloned());
        if c.impassable {
            add_impassable_fences(c, &mut col);
        }
        if c.positions.len() < 3 || c.indices.len() < 3 {
            continue;
        }
        for tri in c.indices.chunks_exact(3) {
            let ia = tri[0] as usize;
            let ib = tri[1] as usize;
            let ic = tri[2] as usize;
            let (Some(a), Some(b), Some(cc)) = (
                c.positions.get(ia).copied(),
                c.positions.get(ib).copied(),
                c.positions.get(ic).copied(),
            ) else {
                continue;
            };
            let t = Tri::new(a, b, cc);
            let mx = (a[0] + b[0] + cc[0]) / 3.0;
            let my = (a[1] + b[1] + cc[1]) / 3.0;
            let (cx, cy) = world_to_chunk(mx, my);
            col.buckets[((cy % 16) * 16 + (cx % 16)) as usize].push(t);
        }
    }
    col
}

/// How far an impassable chunk's fence rises from the chunk AABB's min z
/// (`chunk+0x4c`): the reference's `(0, 0, 32000.0)` (`0x46fa0000` at
/// `0x6ab599`, emitter `0x6ab530`). Nothing extends below, so a mover under the
/// chunk floor (a mine, the sea bed) passes beneath.
const FENCE_REACH: f32 = 32000.0;

/// The four side quads of an impassable chunk (`MCNK` header bit 1, read at
/// `0x6aae2a`): without them the mover walks straight into a mountain chunk,
/// sinks under its surface and is left inside the world. Built from the 9x9
/// outer grid's corners (row `r` starts at `r·17`) from the chunk's min z up
/// [`FENCE_REACH`]; vertical, so they never answer as a floor.
fn add_impassable_fences(c: &benilla_formats::ChunkMesh, col: &mut TileCol) {
    let Some(floor) = c.positions.iter().map(|p| p[2]).reduce(f32::min) else {
        return; // a chunk with no vertices has no AABB to stand a fence on
    };
    const SIDES: [[usize; 2]; 4] = [
        [8, 0],     // north (row 0), walked east → west
        [136, 144], // south (row 8), walked west → east
        [0, 136],   // west (col 0), walked north → south
        [144, 8],   // east (col 8), walked south → north
    ];
    // The chunk's own bucket, from its centre vertex (row 4, col 4).
    let Some(centre) = c.positions.get(72) else {
        return;
    };
    let (cx, cy) = world_to_chunk(centre[0], centre[1]);
    let bucket = ((cy % 16) * 16 + (cx % 16)) as usize;
    let top = floor + FENCE_REACH;
    for [from, to] in SIDES {
        let (Some(a), Some(b)) = (c.positions.get(from), c.positions.get(to)) else {
            continue;
        };
        col.buckets[bucket].push(Tri::new(
            [a[0], a[1], floor],
            [b[0], b[1], floor],
            [b[0], b[1], top],
        ));
        col.buckets[bucket].push(Tri::new(
            [a[0], a[1], floor],
            [b[0], b[1], top],
            [a[0], a[1], top],
        ));
    }
}

// ---- C ABI ----

fn player_mut(world: *mut BnWorld) -> Option<&'static mut Player> {
    if world.is_null() {
        return None;
    }
    // SAFETY: the host owns the handle and calls from one thread.
    let w = unsafe { &mut *world };
    w.player.as_mut()
}

/// Start the middle-layer player: `map`, feet position in WoW yards (X, Y).
/// Loads the ADT under it and drops the feet on the first floor found.
#[no_mangle]
pub extern "C" fn bn_player_reset(
    world: *mut BnWorld,
    map: *const std::ffi::c_char,
    x: f32,
    y: f32,
) -> u32 {
    if world.is_null() || map.is_null() {
        return 0;
    }
    let Some(map) = crate::cstr_to_str(map) else {
        return 0;
    };
    let w = unsafe { &mut *world };
    let mut p = Player::new(map, x, y);
    p.ensure_tiles(&w.archives);
    // Drop to the floor under the spawn (city pavement over the ADT).
    if let Some((z, _)) = p.ground(x, y, f32::MAX) {
        p.pos[2] = z;
        p.grounded = true;
        p.last_ground = p.pos;
    }
    w.player = Some(p);
    1
}

/// Place the middle layer's body at a server position (feet, WoW yards) and
/// heading. Unlike `bn_player_reset` the feet keep the server Z (the client's
/// own `MSG_MOVE_TELEPORT_ACK` path does the same) and the local predictor's
/// velocity is dropped so the next tick starts clean. The map is switched when
/// it differs from the resident one (a login can land in another continent).
#[no_mangle]
pub extern "C" fn bn_player_teleport(
    world: *mut BnWorld,
    map: *const std::ffi::c_char,
    x: f32,
    y: f32,
    z: f32,
    yaw: f32,
) -> u32 {
    if world.is_null() {
        return 0;
    }
    let Some(map) = crate::cstr_to_str(map) else {
        return 0;
    };
    let w = unsafe { &mut *world };
    let Some(mut p) = w.player.take() else {
        return 0;
    };
    if p.map_of() != map {
        p = Player::new(map, x, y);
    }
    p.set_server_pose([x, y, z], yaw);
    p.ensure_tiles(&w.archives);
    w.player = Some(p);
    1
}

/// Held movement input: forward/strafe/turn -1..1 (in the aim's basis; turn is
/// left-positive and the host sends it only while the right button is up), jump
/// and walk as 0/1, and `up` (-1..1) for roam mode's Space/Ctrl. The host
/// forwards the raw key state each frame.
#[no_mangle]
pub extern "C" fn bn_player_input(
    world: *mut BnWorld,
    forward: f32,
    strafe: f32,
    turn: f32,
    jump: u32,
    walk: u32,
    up: f32,
) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.input.forward = forward;
    p.input.strafe = strafe;
    p.input.turn = turn;
    p.input.jump = jump != 0;
    p.input.walk = walk != 0;
    p.input.up = up;
    1
}

/// The stock UI's held movement axes, and whether a binding owns them at all:
/// the world frame's `MoveForwardStart`/... bodies drive this instead of the
/// host's raw key state, as the 1.12 bindings drive the real client.
pub(crate) fn ui_move(world: *mut BnWorld, forward: f32, strafe: f32, turn: f32, held: bool) {
    if let Some(p) = player_mut(world) {
        p.ui_input = held.then_some([forward, strafe, turn]);
    }
}

/// A `Jump()` binding's press: spent on the next locomotion tick.
pub(crate) fn ui_jump(world: *mut BnWorld) {
    if let Some(p) = player_mut(world) {
        p.ui_jump = true;
    }
}

/// A `ToggleRun()` binding's press: the walk flag the tick then holds.
pub(crate) fn ui_toggle_walk(world: *mut BnWorld) {
    if let Some(p) = player_mut(world) {
        let next = !p.input.walk;
        p.ui_walk = Some(next);
        p.input.walk = next;
    }
}

/// A `CameraZoomIn`/`CameraZoomOut` binding: positive notches pull the eye in.
pub(crate) fn ui_zoom(world: *mut BnWorld, notches: f32) {
    if let Some(p) = player_mut(world) {
        p.cam_target_distance = (p.cam_target_distance - notches).clamp(0.0, CAM_DIST_DEFAULT);
    }
}

/// The body's feet position, for the UI's nearest-unit picks.
pub(crate) fn ui_pos(world: *mut BnWorld) -> Option<[f32; 3]> {
    unsafe { world.as_ref() }.and_then(|w| w.player.as_ref().map(|p| p.pos))
}

/// The reference's up-edge click predicate (`0x514ae0`), as
/// `crates/benilla-app/src/player/camera.rs` states it: a click is a release within
/// 200 ms, or within 800 ms while the drag has asked for under 2.25° of yaw and
/// 2.0° of pitch. Travel sums absolute motion per axis (`0x514400`), charged before
/// any clamp, so a drag pinned at the pitch limit still spends the budget.
const CLICK_FREE_WINDOW: f32 = 0.200;
const CLICK_HOLD_CEILING: f32 = 0.800;
const CLICK_YAW_TRAVEL: f32 = 2.25 * std::f32::consts::PI / 180.0;
const CLICK_PITCH_TRAVEL: f32 = 2.0 * std::f32::consts::PI / 180.0;

/// An undecided primary press: the reference's `[0xbe1148]` word, press time at
/// `+0x14` and the two travel accumulators.
#[derive(Clone, Copy)]
struct PressGesture {
    at: f32,
    yaw_travel: f32,
    pitch_travel: f32,
}

impl PressGesture {
    fn new(at: f32) -> Self {
        Self {
            at,
            yaw_travel: 0.0,
            pitch_travel: 0.0,
        }
    }

    fn is_click(&self, now: f32) -> bool {
        let elapsed = now - self.at;
        elapsed < CLICK_FREE_WINDOW
            || (elapsed < CLICK_HOLD_CEILING
                && self.yaw_travel < CLICK_YAW_TRAVEL
                && self.pitch_travel < CLICK_PITCH_TRAVEL)
    }
}

/// The mouse buttons the world owns, latched at the press
/// (`crates/benilla-app/src/player/camera.rs`'s `WorldMouse`). 1.12 reads the
/// `TURNORACTION` and `CAMERAORSELECTORMOVE` bindings through the world frame, and a press a UI
/// frame captured never dispatches one: no look session, no turn channel. The world's down sets a
/// held bit and that button's up clears it, so a frame appearing under the cursor cannot steal it.
#[derive(Default)]
pub(crate) struct WorldMouse {
    /// `[0]` the left button's, `[1]` the right's: held by the world right now.
    held: [bool; 2],
    /// `MouselookStart` holds the TurnOrAction channel (`0x514231`) with no button held.
    scripted: bool,
    /// One undecided press per primary button, settled by its release
    /// (`0x514ae0`): the world only picks what this says is a click.
    clicks: [Option<PressGesture>; 2],
}

impl WorldMouse {
    fn slot(button: &str) -> Option<usize> {
        match button {
            "LeftButton" => Some(0),
            "RightButton" => Some(1),
            _ => None,
        }
    }

    /// The TurnOrAction channel, `[InputControl+4] & 1`, which `IsMouselooking()` reads: the
    /// world's right button, or a `MouselookStart` with none.
    pub(crate) fn turn(&self) -> bool {
        self.held[1] || self.scripted
    }

    /// Both primaries held: a chord, never a select (`0x514ac1`).
    fn both(&self) -> bool {
        self.turn() && self.held[0]
    }

    /// One button edge. A down edge belongs to the world when no frame took it, or when a look
    /// session already owns the cursor: a chord's second button is the world's too. The press
    /// arms its click test unless the other channel is held (`0x51491f` engages, the release
    /// decides), and the release answers with the settled click as `[1 << slot]`.
    pub(crate) fn edge(&mut self, button: &str, down: bool, frame_took: bool, now: f32) -> u32 {
        let Some(i) = Self::slot(button) else {
            return 0;
        };
        if !down {
            self.held[i] = false;
            return match self.clicks[i].take() {
                Some(gesture) if gesture.is_click(now) => 1 << i,
                _ => 0,
            };
        }
        if !frame_took || self.turn() || self.held[0] {
            self.held[i] = true;
        }
        if self.both() {
            self.clicks = [None, None];
        } else {
            // The other channel owns the gesture: a right press with left held and a
            // left press on the turn channel each arm nothing (`0x514ac1`, `0x51481a`).
            let other_held = if i == 1 { self.held[0] } else { self.turn() };
            self.clicks[i] = (!frame_took && !other_held).then(|| PressGesture::new(now));
        }
        0
    }

    /// Charge this frame's travel to every undecided press (`0x514400`).
    fn note_travel(&mut self, dx: f32, dy: f32) {
        let (yaw, pitch) = (dx.abs() * LOOK_SENSITIVITY, dy.abs() * LOOK_SENSITIVITY);
        for gesture in self.clicks.iter_mut().flatten() {
            gesture.yaw_travel += yaw;
            gesture.pitch_travel += pitch;
        }
    }

    /// The TurnOrAction channel's rise or fall. A `MouselookStart`/`Stop` the VM ran (`disarm`)
    /// also drops the undecided presses (`0x514810(0)` stores mode 0 unguarded), which is why a
    /// stop's release and the button after it select nothing; the right button's own binding does
    /// not, or a right click could never attack.
    fn set_channel(&mut self, on: bool, disarm: bool) {
        if disarm {
            self.clicks = [None, None];
        }
        if on {
            self.scripted = true;
        } else {
            self.scripted = false;
            self.held[1] = false;
        }
    }
}

thread_local! {
    static BN_LOOK: RefCell<WorldMouse> = const { RefCell::new(WorldMouse {
        held: [false; 2],
        scripted: false,
        clicks: [None; 2],
    }) };
    static BN_LOOK_AT: std::time::Instant = std::time::Instant::now();
}

/// Seconds since this thread's first look event, the clock the press predicate
/// measures on (`GetTickCount` in the reference).
fn look_now() -> f32 {
    BN_LOOK_AT.with(|t| t.elapsed().as_secs_f32())
}

/// Whether a look session owns the cursor at all: the right button's channel or
/// a left drag the world took. With neither, the pointer belongs to the UI and a
/// delta moves nothing.
pub(crate) fn look_session() -> bool {
    BN_LOOK.with(|l| l.borrow().turn() || l.borrow().held[0])
}

/// A mouse button edge, from `bn_ui_mouse_button`: `frame_took` is that frame's verdict, the one
/// thing that decides whether the world owns the press. The release answers with the click the
/// reference's predicate settles: bit0 the left button's, bit1 the right's.
pub(crate) fn ui_mouse_edge(button: &str, down: bool, frame_took: bool) -> u32 {
    BN_LOOK.with(|l| l.borrow_mut().edge(button, down, frame_took, look_now()))
}

/// Charge this frame's pointer travel to the undecided presses, so a drag spends its click budget
/// (`0x514400`) exactly as it spends the camera's rotation.
pub(crate) fn note_travel(dx: f32, dy: f32) {
    BN_LOOK.with(|l| l.borrow_mut().note_travel(dx, dy));
}

/// Whether the TurnOrAction channel is held: the drag turns the body, not just the camera.
pub(crate) fn look_turn() -> bool {
    BN_LOOK.with(|l| l.borrow().turn())
}

/// The `TURNORACTION` binding's Start/Stop, which is the right button's own channel: it holds the
/// channel without touching the undecided click, or a right click could never attack.
pub(crate) fn ui_turn_held(on: bool) {
    BN_LOOK.with(|l| l.borrow_mut().set_channel(on, false));
}

/// The `MouselookStart`/`Stop` calls the VM made since the last pass, applied in call order, and
/// the channel's state handed to `IsMouselooking` for it to read.
pub(crate) fn drive_mouselook(script: &mut benilla_ui::script::UiScript) {
    for on in script.take_mouselook_calls() {
        BN_LOOK.with(|l| l.borrow_mut().set_channel(on, true));
    }
    script.set_turn_or_action_held(look_turn());
}

/// Roam mode on/off: free flight for touring the map (no gravity, collision or
/// water). Returns 1 with the flag applied. Turning it off leaves the body to
/// gravity from wherever it is, so the void guard still catches a bad drop.
#[no_mangle]
pub extern "C" fn bn_player_fly(world: *mut BnWorld, on: u32) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.fly = on != 0;
    if !p.fly {
        p.vel = [0.0; 3];
        p.grounded = false;
        p.snap_pending = true;
    }
    1
}

/// Mouse look: raw pointer delta in pixels. The client's law is 0.003 rad
/// per unit on both axes (`LOOK_SENSITIVITY`), pitch clamped to ±89°.
/// `turn_body` != 0 is the right button: `camera` turns the character with
/// the camera, so the aim and the rendered body turn too. The left button
/// only orbits, leaving the aim (and WASD's basis) where it was.
#[no_mangle]
pub extern "C" fn bn_player_mouse(world: *mut BnWorld, dx: f32, dy: f32) -> u32 {
    // The host reports the pointer's travel every frame, in the pixels the law is
    // measured in. It is charged to an undecided press before anything else, as the
    // reference does: a drag spends its click budget whether or not it turns.
    note_travel(dx, dy);
    // Only a look session the world owns moves anything.
    if !look_session() {
        return 0;
    }
    let Some(p) = player_mut(world) else { return 0 };
    // Which channel the drag runs on is the world mouse's, not the host's: the
    // TurnOrAction bit turns the body, a left drag only orbits the camera.
    p.look(dx, dy, look_turn());
    1
}

/// Whether a look session owns the cursor right now. The host has no window
/// grab, so it hides and pins the pointer for the session the way
/// `engage_look`'s `take_cursor` does, and a drag that reaches the window edge
/// keeps turning instead of stalling.
#[no_mangle]
pub extern "C" fn bn_player_look_session() -> u32 {
    look_session() as u32
}

/// Wheel zoom: `notches` positive = closer (CameraZoomIn 1.0 per notch).
#[no_mangle]
pub extern "C" fn bn_player_zoom(world: *mut BnWorld, notches: f32) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.cam_target_distance = (p.cam_target_distance - notches).clamp(0.0, CAM_DIST_DEFAULT);
    1
}

/// Unarmed attack edge. Ignored in the air or mounted, like the client.
#[no_mangle]
pub extern "C" fn bn_player_attack(world: *mut BnWorld) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    if !p.grounded || p.mounted {
        return 0;
    }
    // The clip the body swings: the equipped main hand's, the same table the
    // server's attack latch resolves at `SMSG_ATTACKSTART`.
    crate::net::refresh_self_swing_anim();
    p.attack_left = 0.7;
    1
}

/// Mount / dismount preview (the same flag drives speed and the Mount idle).
/// A server mount is the server's to end: while `UNIT_FIELD_MOUNTDISPLAYID`
/// is set the key changes nothing.
#[no_mangle]
pub extern "C" fn bn_player_toggle_mount(world: *mut BnWorld) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    if p.server_mount != 0 {
        return 1;
    }
    p.mount_preview = !p.mount_preview;
    p.mounted = p.mount_preview;
    if p.mounted {
        p.attack_left = 0.0;
    }
    p.mounted as u32
}

/// Step the player and fill `out` with the pose the host renders.
#[no_mangle]
pub extern "C" fn bn_player_tick(world: *mut BnWorld, dt: f32, out: *mut BnPlayerState) -> u32 {
    if out.is_null() {
        return 0;
    }
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &mut *world };
    // Ask the tile worker for the body's own 3x3 window before touching the
    // player: the host's stream window can lag the tile the body walks into,
    // and a sync ADT parse in the tick that adopts it is the border hitch.
    if let Some(p) = w.player.as_ref() {
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let map = p.map.clone();
        crate::player_prefetch_window(w, &map, tx, ty);
    }
    let Some(p) = w.player.as_mut() else { return 0 };
    // Split borrows: the tile load reads the archives, the WMO build the chain
    // and its shared model cache.
    let t0 = std::time::Instant::now();
    p.ensure_tiles(&w.archives);
    let t1 = std::time::Instant::now();
    p.ensure_wmo(&mut w.chain, &mut w.wmo_collision);
    let t2 = std::time::Instant::now();
    p.tick(dt);
    let t3 = std::time::Instant::now();
    let tiles_ms = (t1 - t0).as_secs_f64() * 1000.0;
    let wmo_ms = (t2 - t1).as_secs_f64() * 1000.0;
    let tick_ms = (t3 - t2).as_secs_f64() * 1000.0;
    if tiles_ms + wmo_ms + tick_ms > 8.0 {
        eprintln!("bn_player_tick: tiles={tiles_ms:.1} wmo={wmo_ms:.1} tick={tick_ms:.1}");
    }
    let s = p.state();
    unsafe {
        *out = s;
    }
    1
}

/// Current pose without stepping `dt`.
#[no_mangle]
pub extern "C" fn bn_player_state(world: *const BnWorld, out: *mut BnPlayerState) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(p) = w.player.as_ref() else { return 0 };
    unsafe {
        *out = p.state();
    }
    1
}

/// The ADT terrain height under (x, y) in WoW yards, `NaN` where no tile answers: the portal
/// cull's seed races this against the building floor (`0x6822a2`).
#[no_mangle]
pub extern "C" fn bn_terrain_wow_z(world: *const BnWorld, x: f32, y: f32) -> f32 {
    if world.is_null() {
        return f32::NAN;
    }
    let w = unsafe { &*world };
    w.player
        .as_ref()
        .and_then(|p| p.terrain_z(x, y))
        .unwrap_or(f32::NAN)
}

/// The Z a body standing at (`x`, `y`) with its seat at `seat_z` ends at, in WoW yards, or `NaN`
/// where the cast finds no floor and the seat stands. This is the reference's idle ground cast
/// (`0x636dcd`-`0x636e45`) over this body's own surface set (ADT walking faces, WMO floors, doodad
/// hulls): the ray starts 1/36 above the seat and reaches 1/36 + 4 yd down, one-sided. A creature
/// behind a counter, on a dock or inside a building lands on that floor instead of the terrain
/// below it, and a unit on a floor the set is missing (an upper storey, a balcony) holds the
/// server's Z rather than falling to the ground under the building.
#[no_mangle]
pub extern "C" fn bn_ground_clamp_z(world: *const BnWorld, x: f32, y: f32, seat_z: f32) -> f32 {
    if world.is_null() || !seat_z.is_finite() {
        return f32::NAN;
    }
    let w = unsafe { &*world };
    w.player
        .as_ref()
        .and_then(|p| ground_clamp(p, x, y, seat_z))
        .unwrap_or(f32::NAN)
}

/// `ground_clamp_creatures`' idle arm (benilla-app/src/net/motion/spline.rs): one cast from
/// `IDLE_UP_BAND` above the seat, reaching `IDLE_UP_BAND + GROUND_CLAMP_DOWN` down. The band is
/// also the airborne gate, so nothing further below the seat answers.
fn ground_clamp(p: &Player, x: f32, y: f32, seat_z: f32) -> Option<f32> {
    const IDLE_UP_BAND: f32 = 1.0 / 36.0;
    const GROUND_CLAMP_DOWN: f32 = 4.0;
    p.ground(x, y, seat_z + IDLE_UP_BAND)
        .map(|(z, _)| z)
        .filter(|&z| z >= seat_z - GROUND_CLAMP_DOWN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A player with one vertical wall at x = -8800 spanning y -810..-790,
    /// 4 yd tall.
    fn player_with_wall() -> Player {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let mut col = TileCol::new();
        let wall = Tri::new(
            [-8800.0, -810.0, 0.0],
            [-8800.0, -790.0, 0.0],
            [-8800.0, -810.0, 4.0],
        );
        let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &wall);
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(wall);
            }
        }
        p.tiles.insert((tx, ty), col);
        p
    }

    #[test]
    fn the_capsule_stops_at_a_wall() {
        let p = player_with_wall();
        assert!(
            p.wall_hit(-8799.7, -800.0, 0.0),
            "within the capsule radius"
        );
        assert!(!p.wall_hit(-8798.0, -800.0, 0.0), "clear of the wall");
        assert!(!p.wall_hit(-8799.7, -800.0, 5.0), "above the wall top");
    }

    #[test]
    fn floors_are_not_walls() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let mut col = TileCol::new();
        let floor = Tri::new(
            [-8810.0, -810.0, 0.0],
            [-8790.0, -810.0, 0.0],
            [-8810.0, -790.0, 0.0],
        );
        let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &floor);
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(floor);
            }
        }
        p.tiles.insert((tx, ty), col);
        assert!(!p.wall_hit(-8800.0, -800.0, 0.0), "a floor never blocks");
        assert_eq!(p.ground(-8800.0, -800.0, 100.0), Some((0.0, 1.0)));
    }

    #[test]
    fn a_floor_out_of_the_cast_s_band_holds_the_seat() {
        // The idle cast reaches 4 yd below the seat and no further: a unit on a surface the
        // collision set is missing holds the server's Z instead of falling to the ground.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let mut col = TileCol::new();
        let floor = Tri::new(
            [-8810.0, -810.0, 0.0],
            [-8790.0, -810.0, 0.0],
            [-8810.0, -790.0, 0.0],
        );
        let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &floor);
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(floor);
            }
        }
        p.tiles.insert((tx, ty), col);
        assert_eq!(ground_clamp(&p, -8800.0, -800.0, 3.9), Some(0.0));
        assert_eq!(ground_clamp(&p, -8800.0, -800.0, 5.0), None);
    }

    #[test]
    fn a_hole_in_the_collision_data_never_drops_the_body_out_of_the_world() {
        // No geometry at all under the feet, in a tile whose set is complete:
        // the body falls, and the void guard restores the last real floor once
        // it is clearly through the map.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.pos[2] = 10.0;
        p.last_ground = [p.pos[0], p.pos[1], 10.0];
        p.grounded = true;
        p.tiles
            .insert(Player::tile_of(p.pos[0], p.pos[1]), TileCol::new());
        let mut restored = false;
        for _ in 0..400 {
            p.tick(0.1);
            assert!(
                p.pos[2] > -100.0,
                "never falls out of the world: {}",
                p.pos[2]
            );
            if p.grounded && (p.pos[2] - 10.0).abs() < 1.0e-3 {
                restored = true;
            }
        }
        assert!(restored, "the void guard put the body back on the floor");
    }

    #[test]
    fn a_body_under_the_terrain_walks_no_deeper_and_is_lifted_out() {
        // The logged Goldshire fall (`run_qm24.txt`): the body walked Crystal
        // Lake's bed with the surface more than a step over its head, then met
        // the rising shore and fell out of the map. A body in that state must
        // not walk further into the hill, and the guard must put it back on the
        // real surface instead of restoring it into the void.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let (cx, cy) = world_to_chunk(p.pos[0], p.pos[1]);
        let b = ((cy % 16) * 16 + (cx % 16)) as usize;
        let mut col = TileCol::new();
        col.buckets[b].push(Tri::new(
            [-8820.0, -820.0, 0.0],
            [-8780.0, -820.0, 0.0],
            [-8780.0, -780.0, 0.0],
        ));
        col.buckets[b].push(Tri::new(
            [-8820.0, -820.0, 0.0],
            [-8780.0, -780.0, 0.0],
            [-8820.0, -780.0, 0.0],
        ));
        p.tiles.insert((tx, ty), col);
        p.pos[2] = -2.5; // more than a step under the surface
        p.grounded = false;
        p.last_ground = [p.pos[0], p.pos[1], 0.0];
        p.yaw = 0.0;
        p.model_yaw = 0.0;
        p.input.forward = 1.0;
        for _ in 0..30 {
            p.tick(1.0 / 60.0);
        }
        assert!(
            (p.pos[0] + 8800.0).abs() < 0.01,
            "the surface over the head is a wall, not a slope to walk into: {}",
            p.pos[0]
        );
        for _ in 0..120 {
            p.tick(1.0 / 60.0);
        }
        assert!(p.grounded, "the guard restored the body");
        assert!(
            (p.pos[2] - 0.0).abs() < 1.0e-3,
            "onto the surface, not into the void: {}",
            p.pos[2]
        );
    }

    #[test]
    fn the_mount_is_the_servers_and_the_preview_reports_no_display() {
        // `UNIT_FIELD_MOUNTDISPLAYID` picks the creature the host draws; the
        // M-key preview has no server row behind it and reports 0 so the host
        // can fall back to its stock horse.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.mount_preview = true;
        p.mounted = true;
        let s = p.state();
        assert_eq!(s.mounted, 1);
        assert_eq!(s.mount_display, 0);
        p.mount_preview = false;
        p.server_mount = 14374;
        let s = p.state();
        assert_eq!(s.mounted, 1);
        assert_eq!(s.mount_display, 14374);
        assert_eq!(s.mount_anim, 0, "a standing mount keeps its stand clip");
        p.vel = [0.0, 11.2, 0.0];
        assert_eq!(p.state().mount_anim, 5, "a moving mount runs");
        p.vel = [0.0, 2.5, 0.0];
        assert_eq!(p.state().mount_anim, 4, "a walk-paced mount walks");
    }

    #[test]
    fn a_pure_strafe_twists_the_spine_half_and_the_head_the_rest() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        let (spine, head) = twist_shares(-FRAC_PI_2);
        assert!((spine + FRAC_PI_4).abs() < 1.0e-6, "half the gap: {spine}");
        assert!((head + FRAC_PI_4).abs() < 1.0e-6, "the rest: {head}");
        assert!(
            (spine + head + FRAC_PI_2).abs() < 1.0e-6,
            "the head lands on the aim"
        );
    }

    #[test]
    fn a_diagonal_gap_is_split_evenly_and_a_wide_one_is_capped() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        let (spine, head) = twist_shares(FRAC_PI_4);
        assert!((spine - FRAC_PI_4 * 0.5).abs() < 1.0e-6);
        assert!((head - FRAC_PI_4 * 0.5).abs() < 1.0e-6);
        // A gap past a pure strafe caps each channel at 45°, so the head stops
        // short of the aim.
        let (spine, head) = twist_shares(FRAC_PI_2 + 0.3);
        assert!((spine - FRAC_PI_4).abs() < 1.0e-6);
        assert!((head - FRAC_PI_4).abs() < 1.0e-6);
    }

    #[test]
    fn a_mounted_rider_keeps_only_the_head_share() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        let (spine, head) = armed_twist_shares(FRAC_PI_2, true);
        assert_eq!(spine, 0.0, "SpineLow disarms under a mount");
        assert!((head - FRAC_PI_4).abs() < 1.0e-6, "the head still turns");
        let (spine, head) = armed_twist_shares(FRAC_PI_2, false);
        assert!((spine - FRAC_PI_4).abs() < 1.0e-6);
        assert!((head - FRAC_PI_4).abs() < 1.0e-6);
    }

    #[test]
    fn a_room_whose_floor_set_is_still_folding_holds_the_body() {
        // Walking into an interior before the tile's WMO fold finishes: the
        // room's floor is not in the set yet and the terrain under it is holed,
        // so the body holds its seat instead of dropping out of the room.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.pos[2] = 63.8;
        p.last_ground = p.pos;
        p.grounded = true;
        let mut col = TileCol::new();
        col.wmos.push(benilla_formats::WmoInstance {
            model: "world\\wmo\\test\\room.wmo".to_string(),
            position: [-8800.0, -800.0, 56.5],
            rotation: [0.0; 3],
            unique_id: 1,
            doodad_set: 0,
            name_set: 0,
        });
        p.tiles.insert(Player::tile_of(p.pos[0], p.pos[1]), col);
        for _ in 0..100 {
            p.tick(0.1);
            assert!(
                (p.pos[2] - 63.8).abs() < 1.0e-3,
                "held the seat at {}",
                p.pos[2]
            );
        }
        assert!(p.grounded, "still standing");
    }

    #[test]
    fn a_login_inside_a_room_holds_the_server_z_until_the_floor_set_bakes() {
        // `set_server_pose` leaves the body airborne, and a fresh tile's WMO
        // fold has not run: the room's floor is absent and the terrain under it
        // is holed. The body holds the server's Z, then settles on the floor
        // once the set bakes.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.set_server_pose([-8800.0, -800.0, 63.8], 0.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let mut col = TileCol::new();
        col.wmos.push(benilla_formats::WmoInstance {
            model: "world\\wmo\\test\\room.wmo".to_string(),
            position: [-8800.0, -800.0, 56.5],
            rotation: [0.0; 3],
            unique_id: 1,
            doodad_set: 0,
            name_set: 0,
        });
        p.tiles.insert((tx, ty), col);
        for _ in 0..50 {
            p.tick(0.1);
            assert!(
                (p.pos[2] - 63.8).abs() < 1.0e-3,
                "held the server Z at {}",
                p.pos[2]
            );
        }
        // The fold reaches the floor: the set is no longer owed, and the body
        // settles onto the room's floor.
        let floor = Tri::new(
            [-8810.0, -810.0, 63.8],
            [-8790.0, -810.0, 63.8],
            [-8810.0, -790.0, 63.8],
        );
        let col = p.tiles.get_mut(&(tx, ty)).unwrap();
        col.wmo_inst = col.wmos.len();
        let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &floor);
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(floor);
            }
        }
        p.tick(0.1);
        assert!(p.grounded, "settled on the floor");
        assert!((p.pos[2] - 63.8).abs() < 1.0e-3, "feet on it: {}", p.pos[2]);
    }

    #[test]
    fn a_terminal_velocity_fall_lands_instead_of_tunnelling() {
        // A hitched frame (dt clamped to 0.1) at terminal velocity drops 6 yd:
        // probing only `pos.z + STEP_UP_HEIGHT` misses the floor once the feet
        // pass it by more than that, and the body leaves the map.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let mut col = TileCol::new();
        let a = Tri::new(
            [-8820.0, -820.0, 0.0],
            [-8780.0, -820.0, 0.0],
            [-8820.0, -780.0, 0.0],
        );
        let b = Tri::new(
            [-8780.0, -820.0, 0.0],
            [-8780.0, -780.0, 0.0],
            [-8820.0, -780.0, 0.0],
        );
        for t in [a, b] {
            let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &t);
            for ly in y0..=y1 {
                for lx in x0..=x1 {
                    col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(t);
                }
            }
        }
        p.tiles.insert((tx, ty), col);
        p.pos[2] = 500.0;
        p.vel[2] = -TERMINAL_VELOCITY;
        p.grounded = false;
        for _ in 0..120 {
            p.tick(0.1);
        }
        assert!(p.grounded, "landed, not still falling at {}", p.pos[2]);
        assert!(p.pos[2].abs() < 1.0e-3, "feet on the floor: {}", p.pos[2]);
    }

    #[test]
    fn the_boom_snaps_in_at_a_wall_and_eases_back_out() {
        // Facing +X with the wall 3 yd west of the feet, the default 15 yd boom
        // would put the eye behind the wall: it must stop just short of it, and
        // never shrink to zero (that would put the eye inside the body).
        let mut p = player_with_wall();
        p.pos = [-8797.0, -800.0, 0.0];
        p.tick(0.1);
        assert!(
            (p.cam_live - 2.6).abs() < 0.5,
            "the boom stops at the wall: {}",
            p.cam_live
        );
        assert!(p.cam_live >= CAM_BOOM_MIN, "never inside the body");
        // Turn the orbit away from the wall: the boom eases out at 8.33 yd/s,
        // not in one frame.
        p.cam_yaw = std::f32::consts::FRAC_PI_2;
        p.tick(0.1);
        assert!(p.cam_live > 2.6, "released from the wall");
        assert!(p.cam_live < CAM_DIST_DEFAULT, "eased, not snapped");
    }

    /// World x of the profiles' own x = 0; every profile is authored around it.
    const PROFILE_X0: f32 = -8800.0;

    /// A `(x, z)` profile extruded across y, the shape benilla-app's mover tests
    /// build their kerbs from: each segment is a quad in the tile's WMO grid, so
    /// `ground` and `wall_hit` see the same triangles the reference walks.
    fn terrain_profile(profile: &[(f32, f32)]) -> TileCol {
        let (tx, ty) = Player::tile_of(PROFILE_X0, -800.0);
        let mut col = TileCol::new();
        let mut add = |t: Tri| {
            let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &t);
            for ly in y0..=y1 {
                for lx in x0..=x1 {
                    col.wmo_cells[(ly * WMO_GRID + lx) as usize].push(t);
                }
            }
        };
        for w in profile.windows(2) {
            let ((x0, z0), (x1, z1)) = (w[0], w[1]);
            let (x0, x1) = (x0 + PROFILE_X0, x1 + PROFILE_X0);
            add(Tri::new(
                [x0, -900.0, z0],
                [x1, -900.0, z1],
                [x0, -700.0, z0],
            ));
            add(Tri::new(
                [x1, -900.0, z1],
                [x1, -700.0, z1],
                [x0, -700.0, z0],
            ));
        }
        col
    }

    /// A body on a flat synthetic floor, standing at the profile's origin.
    fn player_on_flat() -> Player {
        let mut p = Player::new("Azeroth", PROFILE_X0, -800.0);
        let flat = [(-2.0f32, 0.0f32), (60.0, 0.0)];
        p.tiles
            .insert(Player::tile_of(p.pos[0], p.pos[1]), terrain_profile(&flat));
        p.pos[2] = p.ground(p.pos[0], -800.0, f32::MAX).map_or(0.0, |(z, _)| z);
        p.grounded = true;
        p.last_ground = p.pos;
        p
    }

    /// `player::arc` + `creature_anim::select::current_special`: a jump stays in
    /// the Jump bracket for its whole arc, because a flat jump never drops
    /// `FALL_FAR_DROP` below its takeoff and so never arms the Fall loop at all.
    /// The arc is shorter than JumpStart's own clip (825 ms of 833), so the launch
    /// pose plays through and the hang (38) only shows on a longer arc; touchdown
    /// standing still takes JumpEnd (39), then the gait.
    #[test]
    fn a_flat_jump_plays_the_jump_bracket_not_the_fall_loop() {
        let mut p = player_on_flat();
        let z0 = p.pos[2];
        let dt = 1.0 / 60.0;
        p.input.jump = true;
        p.tick(dt);
        p.input.jump = false;
        assert_eq!(p.anim(), 37, "JumpStart at the launch");
        let mut ids: Vec<u32> = vec![p.anim()];
        let mut apex = 0.0f32;
        let mut frames = 1;
        while p.airborne && frames < 200 {
            p.tick(dt);
            frames += 1;
            apex = apex.max(p.pos[2] - z0);
            if p.airborne {
                ids.push(p.anim());
            }
        }
        let air_s = frames as f32 * dt;
        assert!(
            ids.iter().all(|a| *a == 37 || *a == 38),
            "a jump's own arc never takes the Fall loop: {ids:?}"
        );
        assert!(
            ids.iter().all(|a| *a == 37),
            "the arc is shorter than JumpStart's clip, so the hang (38) waits for \
             a longer one: {ids:?}"
        );
        assert!(
            (apex - JUMP_SPEED * JUMP_SPEED / (2.0 * GRAVITY)).abs() < 0.1,
            "apex {apex:.3} yd over 0.825 s, not {air_s:.3} s"
        );
        assert!(
            (air_s - 2.0 * JUMP_SPEED / GRAVITY).abs() < 0.1,
            "airtime {air_s:.3} s"
        );
        assert_eq!(p.anim(), 39, "JumpEnd on touchdown");
        p.land_id = 0;
        assert_eq!(p.anim(), 0, "and then the gait, standing still");
    }

    /// The transplant (`creature_anim::driver::transplant_up`, `0x5fe919`): a
    /// locomotion request mid-swing moves the clip onto the torso at its id,
    /// rate and elapsed position with no cross-fade, so the swing finishes
    /// there instead of restarting from its first frame.
    #[test]
    fn a_swing_moved_to_the_torso_resumes_where_it_played() {
        let mut p = player_on_flat();
        let dt = 0.1;
        p.attack_left = SWING_WINDOW;
        for _ in 0..3 {
            p.tick(dt);
        }
        assert_eq!(
            p.anim(),
            crate::net::self_swing_anim(),
            "the free-legged body plays the swing whole"
        );
        assert_eq!(p.overlay().0, 0, "and needs no torso layer");
        // The legs take a walk request mid-swing: the clip moves to the torso.
        p.input.forward = 1.0;
        p.tick(dt);
        let (id, weight, elapsed) = p.overlay();
        assert_eq!(
            id,
            crate::net::self_swing_anim(),
            "the swing rides the torso"
        );
        assert!(weight > 0.0, "and is armed: {weight}");
        let spent = SWING_WINDOW - p.attack_left;
        assert!(spent > 0.3, "the swing had played before the walk: {spent}");
        assert!(
            (elapsed - spent).abs() < 1.0e-4,
            "resumed where it played ({elapsed} vs {spent}), not from the top"
        );
        assert_eq!(
            p.state().overlay_anim_ms,
            (spent * 1000.0) as u32,
            "the host reads the same position in ms"
        );
    }

    /// A step-off is not a jump: no bracket, so the gait keeps playing, and Fall
    /// (40) only after `FALL_FAR_TIME` airborne (`0x5fd8e8`, `[0x633240]`).
    #[test]
    fn a_step_off_keeps_its_gait_until_the_fall_is_far() {
        let mut p = player_on_flat();
        // A ledge at the profile's origin with an 8 yd drop behind it.
        let cliff = [(-2.0f32, 0.0f32), (0.0, 0.0), (0.002, -8.0), (60.0, -8.0)];
        p.tiles
            .insert(Player::tile_of(p.pos[0], p.pos[1]), terrain_profile(&cliff));
        p.pos[2] = 0.0;
        p.grounded = true;
        p.last_ground = p.pos;
        p.input.forward = 1.0;
        p.yaw = 0.0;
        p.model_yaw = 0.0;
        let dt = 1.0 / 60.0;
        let mut air = 0;
        let mut at_the_edge = 0;
        let mut after_the_latch = 0;
        for _ in 0..200 {
            p.tick(dt);
            if p.grounded {
                continue;
            }
            air += 1;
            if air == 1 {
                at_the_edge = p.anim();
            }
            if p.airborne_s > FALL_FAR_TIME + 0.02 {
                after_the_latch = p.anim();
                break;
            }
        }
        assert!(air > 30, "the drop is long enough to pass the latch");
        assert_eq!(at_the_edge, 5, "a step-off at a run keeps the Run clip");
        assert_eq!(after_the_latch, 40, "the far fall takes the Fall loop");
    }

    /// Walk `frames` frames along the profile, reporting (feet z, x, grounded).
    fn walk_profile(
        profile: &[(f32, f32)],
        start_x: f32,
        yaw: f32,
        frames: usize,
    ) -> Vec<(f32, f32, bool)> {
        let start_x = PROFILE_X0 + start_x;
        let mut p = Player::new("Azeroth", start_x, -800.0);
        p.tiles.insert(
            Player::tile_of(p.pos[0], p.pos[1]),
            terrain_profile(profile),
        );
        p.pos[2] = p.ground(start_x, -800.0, f32::MAX).map_or(0.0, |(z, _)| z);
        p.grounded = true;
        p.last_ground = p.pos;
        p.yaw = yaw;
        p.model_yaw = yaw;
        p.input.forward = 1.0;
        let mut out = Vec::new();
        for _ in 0..frames {
            p.tick(1.0 / 60.0);
            out.push((p.pos[2], p.pos[0], p.grounded));
        }
        out
    }

    /// A Stormwind Trade District kerb: a 0.28 yd sidewalk behind a ~61° bevel
    /// (benilla-app's `world_with_kerb`).
    const KERB: [(f32, f32); 4] = [(-2.0, 0.0), (0.29, 0.0), (0.446, 0.28), (3.0, 0.28)];
    /// Booty Bay's 0.40 yd vertical riser onto a ~22° ramp (benilla-app's
    /// `world_with_vertical_step`).
    const RISER: [(f32, f32); 4] = [(-2.0, 0.0), (0.30, 0.0), (0.302, 0.40), (3.0, 1.48)];
    /// A Stormwind step: a 0.91 yd top behind a 66° bevel (benilla-app's
    /// `a_tall_step_behind_an_unwalkable_face_is_climbed`).
    const TALL_STEP: [(f32, f32); 4] = [(-2.0, -0.02), (0.28, -0.02), (0.694, 0.91), (3.0, 0.91)];
    /// A 66° face walked off at a run (benilla-app's steep-face descent).
    const STEEP_DROP: [(f32, f32); 4] = [(-2.0, 0.0), (0.60, 0.0), (1.0, -0.91), (3.0, -0.91)];

    #[test]
    fn a_kerb_bevel_is_climbed_not_blocked() {
        let frames = walk_profile(&KERB, -1.6, 0.0, 24);
        println!("kerb up: {frames:?}");
        assert!(
            frames.iter().any(|f| f.0 > 0.27 && f.1 > PROFILE_X0 + 0.5),
            "the body must reach the sidewalk past the bevel: {frames:?}"
        );
    }

    #[test]
    fn stepping_off_a_kerb_follows_the_surface_down() {
        let frames = walk_profile(&KERB, 1.6, std::f32::consts::PI, 45);
        println!("kerb down: {frames:?}");
        assert!(
            frames.last().unwrap().0 < 0.05,
            "the walk-off must reach the street: {frames:?}"
        );
    }

    #[test]
    fn a_vertical_step_is_climbed_not_stalled() {
        let frames = walk_profile(&RISER, -1.6, 0.0, 45);
        println!("riser up: {frames:?}");
        assert!(
            frames.last().unwrap().0 > 0.40,
            "the body must finish on the step: {frames:?}"
        );
    }

    #[test]
    fn a_tall_step_behind_a_steep_face_is_climbed() {
        let frames = walk_profile(&TALL_STEP, -1.6, 0.0, 24);
        println!("tall step up: {frames:?}");
        assert!(
            frames.iter().any(|f| f.0 > 0.88 && f.1 > PROFILE_X0 + 0.7),
            "the body must reach the 0.91 yd top: {frames:?}"
        );
    }

    #[test]
    fn a_steep_face_is_walked_off_not_held() {
        let frames = walk_profile(&STEEP_DROP, -0.8, 0.0, 40);
        println!("steep drop: {frames:?}");
        assert!(
            frames.last().unwrap().0 < -0.85,
            "the walk-off must descend the face: {frames:?}"
        );
    }

    /// A 0.9 yd ledge on a bevel so steep it is a drop, not a slope.
    const LEDGE: [(f32, f32); 4] = [(-2.0, 0.9), (0.0, 0.9), (0.05, 0.0), (3.0, 0.0)];

    /// Walking off a ledge must not snap the feet the whole way down in one
    /// frame - the capsule then wedges in the face and the body never moves
    /// again. The reference follows the surface one cone's worth a frame
    /// (`0x6367b0`), so the walk continues, and the same face is climbable
    /// back: the "up but cannot get down" report.
    #[test]
    fn a_ledge_is_walked_off_and_climbed_back() {
        let start_x = PROFILE_X0 - 1.0;
        let mut p = Player::new("Azeroth", start_x, -800.0);
        p.tiles
            .insert(Player::tile_of(start_x, -800.0), terrain_profile(&LEDGE));
        p.pos[2] = 0.9;
        p.grounded = true;
        p.last_ground = p.pos;
        p.yaw = 0.0;
        p.model_yaw = 0.0;
        p.input.forward = 1.0;
        for _ in 0..30 {
            p.tick(1.0 / 60.0);
        }
        println!("ledge off: {:?} grounded={}", p.pos, p.grounded);
        assert!(
            p.pos[0] > PROFILE_X0 + 1.0,
            "the walk must continue past the edge: {:?}",
            p.pos
        );
        assert!(p.pos[2] < 0.05, "and reach the lower ground: {:?}", p.pos);
        p.yaw = std::f32::consts::PI;
        p.model_yaw = p.yaw;
        for _ in 0..30 {
            p.tick(1.0 / 60.0);
        }
        println!("ledge back: {:?} grounded={}", p.pos, p.grounded);
        assert!(
            p.pos[2] > 0.85,
            "the same face must be climbable back: {:?}",
            p.pos
        );
    }

    #[test]
    fn a_doodad_hull_is_a_wall_and_its_cap_is_a_floor() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let (cx, cy) = (p.pos[0] + 2.0, p.pos[1]);
        let mut tile = TileCol::new();
        let ci = TileCol::wmo_cell(tx, ty, cx, cy).unwrap();
        // A trunk's hull quad, standing on the ground: the vertical faces of a
        // placed `bounding_triangles` mesh land in the doodad grid exactly so.
        let wall = |y0: f32, y1: f32, z0: f32, z1: f32| {
            [
                Tri::new([cx, y0, z0], [cx, y1, z0], [cx, y1, z1]),
                Tri::new([cx, y0, z0], [cx, y1, z1], [cx, y0, z1]),
            ]
        };
        tile.doodad_cells[ci].extend(wall(cy - 1.0, cy + 1.0, 0.0, 3.0));
        p.tiles.insert((tx, ty), tile);
        assert!(p.wall_hit(cx, cy, 0.0), "the trunk face stops the capsule");
        assert!(!p.wall_hit(cx + 2.0, cy, 0.0), "clear of it");
        assert!(!p.wall_hit(cx, cy + 4.0, 0.0), "past its end");

        // A stump cap half a yard up is a step the mover certifies, not a
        // wall: the reference walks a short way onto a trunk's hull.
        let (sx, sy) = (cx + 6.0, cy);
        let si = TileCol::wmo_cell(tx, ty, sx, sy).unwrap();
        let tile = p.tiles.get_mut(&(tx, ty)).unwrap();
        tile.doodad_cells[si].push(Tri::new(
            [sx - 0.5, sy - 0.5, 0.5],
            [sx + 0.5, sy - 0.5, 0.5],
            [sx + 0.5, sy + 0.5, 0.5],
        ));
        tile.doodad_cells[si].push(Tri::new(
            [sx - 0.5, sy - 0.5, 0.5],
            [sx + 0.5, sy + 0.5, 0.5],
            [sx - 0.5, sy + 0.5, 0.5],
        ));
        let (z, nz) = p.ground(sx, sy, 5.0).expect("the cap answers as a floor");
        assert!(
            (z - 0.5).abs() < 1.0e-5 && nz >= GROUND_COS,
            "caps are floors"
        );
        assert!(
            !p.wall_hit(sx, sy, 0.0),
            "a half-yard cap is climbed, not blocked"
        );
    }

    #[test]
    fn right_drag_turns_the_aim_and_the_body_lags() {
        let mut p = player_on_flat();
        p.look(100.0, 0.0, true);
        let turn = 100.0 * LOOK_SENSITIVITY;
        assert!(
            (p.cam_yaw - turn).abs() < 1.0e-5,
            "cam_yaw follows the drag"
        );
        assert!(
            (p.yaw + turn).abs() < 1.0e-5,
            "the aim is the mirror of cam_yaw"
        );
        assert!(
            p.model_yaw.abs() < 1.0e-6,
            "the look does not turn the body"
        );
        // Under the 90° ceiling a steering frame leaves the body planted.
        p.tick(1.0 / 60.0);
        assert!(
            p.model_yaw.abs() < 1.0e-6,
            "the body holds while the aim is steered"
        );
        assert_eq!(p.anim(), 0, "and does not shuffle below the ceiling");
        // The Smart follow sees the camera already behind, so it does not move.
        let target = wrap_pi(-p.yaw);
        assert!(wrap_pi(target - p.cam_yaw).abs() < 1.0e-5);
    }

    #[test]
    fn left_drag_only_orbits() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.look(100.0, 0.0, false);
        assert!(p.yaw.abs() < 1.0e-6, "the aim stays put");
        assert!(p.model_yaw.abs() < 1.0e-6);
        assert!((p.cam_yaw - 100.0 * LOOK_SENSITIVITY).abs() < 1.0e-5);
    }

    #[test]
    fn keyboard_turn_carries_the_aim_and_the_camera_but_the_body_holds() {
        let mut p = player_on_flat();
        p.input.turn = 1.0;
        p.tick(0.1);
        let d = TURN_RATE * 0.1;
        assert!((p.yaw - d).abs() < 1.0e-5, "turning left grows the WoW yaw");
        assert!(
            p.model_yaw.abs() < 1.0e-6,
            "the body is planted below the ceiling"
        );
        assert!((p.cam_yaw + d).abs() < 1.0e-5, "the camera mirrors the yaw");
        // The smart follow sees the camera already behind.
        assert!(wrap_pi(wrap_pi(-p.yaw) - p.cam_yaw).abs() < 1.0e-5);
    }

    #[test]
    fn the_standing_chase_freezes_below_the_ceiling_and_sweeps_on_release() {
        use std::f32::consts::FRAC_PI_2;
        // Below the ceiling a steer moves the body not at all.
        assert_eq!(stationary_chase_step(0.5, true, 0.05), 0.0);
        // Past it, only the excess moves it, leaving a 90° trail.
        let step = stationary_chase_step(2.0, true, 0.05);
        assert!((step - (2.0 - FRAC_PI_2)).abs() < 1.0e-6);
        // A released 90° gap closes at 8× the turn rate, clamped to the gap.
        let step = stationary_chase_step(FRAC_PI_2, false, 0.01);
        assert!((step - 0.01 * TURN_RATE * STATIONARY_CHASE_RATE).abs() < 1.0e-6);
        assert_eq!(stationary_chase_step(0.05, false, 1.0), 0.05);
        // Turning right mirrors it.
        assert!(stationary_chase_step(-2.0, true, 0.05) < 0.0);
        assert!(stationary_chase_step(-FRAC_PI_2, false, 0.01) < 0.0);
    }

    #[test]
    fn a_standing_turn_holds_the_ninety_degree_ceiling_then_sweeps_back() {
        use std::f32::consts::FRAC_PI_2;
        let mut p = player_on_flat();
        p.input.turn = 1.0;
        for _ in 0..15 {
            p.tick(0.05); // 0.75 s at π rad/s: the aim passes the ceiling
        }
        let gap = wrap_pi(p.yaw - p.model_yaw);
        assert!(
            (gap.abs() - FRAC_PI_2).abs() < 1.0e-4,
            "the body trails the aim by exactly 90°: {gap}"
        );
        assert_eq!(p.anim(), 11, "the body following its ceiling shuffles left");
        // Release: no key, no look. The body sweeps onto the aim at 8× the
        // turn rate and keeps shuffling although nothing is held.
        let held = p.model_yaw;
        p.input.turn = 0.0;
        p.tick(0.01);
        let step = 0.01 * TURN_RATE * STATIONARY_CHASE_RATE;
        assert!(
            (wrap_pi(p.model_yaw - held) - step).abs() < 1.0e-4,
            "the sweep runs at 8× the turn rate"
        );
        assert_eq!(p.anim(), 11, "and still shuffles mid-sweep");
        for _ in 0..20 {
            p.tick(0.01);
        }
        assert!(
            wrap_pi(p.yaw - p.model_yaw).abs() < 1.0e-5,
            "the body lands on the aim"
        );
        assert_eq!(p.anim(), 0, "then stands");
    }

    #[test]
    fn turning_in_place_shuffles_once_the_body_moves_and_a_turn_in_water_is_the_swim_idle() {
        let mut p = player_on_flat();
        p.input.turn = 1.0;
        p.tick(0.05);
        assert_eq!(p.anim(), 0, "the planted body does not shuffle");
        for _ in 0..15 {
            p.tick(0.05);
        }
        assert_eq!(p.anim(), 11, "ShuffleLeft once the ceiling moves it");
        let mut q = player_on_flat();
        q.input.turn = -1.0;
        for _ in 0..15 {
            q.tick(0.05);
        }
        assert_eq!(q.anim(), 12, "ShuffleRight");
        q.swimming = true;
        assert_eq!(q.anim(), 41, "SwimIdle");
    }

    #[test]
    fn the_camera_pivot_lifts_to_the_surface_band_and_sinks_when_diving() {
        // A surface swimmer's feet 1.52 under the surface lands in the surface
        // band: the corridor lifts the pivot to `surface + 2/9`.
        let surface = 1.52;
        let pivot = camera_pivot_height(CAM_PIVOT_SWIM, CAM_PIVOT_SWIM, Some(surface), 0.0);
        assert!((pivot - (surface + WATER_SURFACE_BAND)).abs() < 1.0e-5);
        // The submerge band puts it `5/6` under the surface.
        let surface = 1.8;
        let pivot = camera_pivot_height(CAM_PIVOT_SWIM, CAM_PIVOT_SWIM, Some(surface), 0.0);
        assert!((pivot - (surface - WATER_REST)).abs() < 1.0e-5);
        // Dry land keeps the standing preset.
        let pivot = camera_pivot_height(CAM_PIVOT_STAND, CAM_PIVOT_STAND, None, 0.0);
        assert!((pivot - CAM_PIVOT_STAND).abs() < 1.0e-5);
    }

    #[test]
    fn forward_moves_along_the_aim() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        // Aim to WoW yaw π/2 (+Y), then run forward for a second (the tick
        // clamps dt to the client's frame slice, so step in slices).
        p.yaw = std::f32::consts::FRAC_PI_2;
        p.input.forward = 1.0;
        for _ in 0..10 {
            p.tick(0.1);
        }
        assert!(p.pos[0] > -8800.1, "no drift along the old facing");
        assert!(p.pos[0] < -8799.9);
        assert!(p.pos[1] > -800.0 + RUN_SPEED * 0.9, "run up +Y");
    }

    #[test]
    fn strafing_twists_the_body_but_not_the_aim() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.input.strafe = 1.0; // D: right of the facing
        for _ in 0..120 {
            p.tick(1.0 / 60.0);
        }
        assert!(p.yaw.abs() < 1.0e-6, "the aim never follows WASD");
        let offset = wrap_pi(p.model_yaw - p.yaw);
        assert!(
            (offset + std::f32::consts::FRAC_PI_2).abs() < 0.05,
            "a pure right strafe turns the body a quarter left in WoW yaw"
        );
    }

    /// One wet 2×2 liquid block at `z`, over the world's north-west corner.
    const LIQUID_UNIT: f32 = 33.333_336 / 8.0;

    fn flat_liquid(z: f32) -> benilla_formats::LiquidMesh {
        let mut positions = Vec::new();
        for r in 0..2 {
            for c in 0..2 {
                positions.push([
                    17066.666 - r as f32 * LIQUID_UNIT,
                    17066.666 - c as f32 * LIQUID_UNIT,
                    z,
                ]);
            }
        }
        benilla_formats::LiquidMesh {
            grid: [2, 2],
            wet: vec![true],
            shared: vec![false],
            positions,
            uvs: vec![[0.0, 0.0]; 4],
            depths: vec![0.0; 4],
            indices: vec![0, 1, 2, 1, 3, 2],
            sound_nibble: 0,
            material_id: None,
            kind: benilla_formats::LiquidKind::Still,
        }
    }

    fn swimmer(surface: f32, feet: f32) -> Player {
        let mut p = Player::new("Azeroth", 17064.0, 17064.0);
        p.pos[2] = feet;
        let mut tile = TileCol::new();
        tile.liquids.push(flat_liquid(surface));
        p.tiles.insert(Player::tile_of(p.pos[0], p.pos[1]), tile);
        p
    }

    #[test]
    fn water_samples_bilinearly_and_only_over_wet_cells() {
        let m = flat_liquid(12.0);
        let x = 17066.666 - 0.5 * LIQUID_UNIT;
        let y = 17066.666 - 0.5 * LIQUID_UNIT;
        assert!((liquid_height(&m, x, y).unwrap() - 12.0).abs() < 1.0e-4);
        assert_eq!(
            liquid_height(&m, 17066.666 + LIQUID_UNIT, y),
            None,
            "dry corner"
        );
    }

    #[test]
    fn deep_water_latches_the_swim_and_freezes_gravity() {
        // 1.6 yd over the feet is past the 0.75·h = 1.521 entry depth.
        let mut p = swimmer(0.0, -1.6);
        p.tick(0.1);
        assert!(p.swimming);
        assert!(!p.grounded);
        assert!(
            (p.pos[2] + 1.6).abs() < 1.0e-4,
            "an idle swimmer keeps its depth"
        );
        assert_eq!(p.anim(), 41, "SwimIdle");
    }

    #[test]
    fn shallow_water_is_waded() {
        let mut p = swimmer(0.0, -1.0);
        p.tick(0.1);
        assert!(!p.swimming, "1.0 yd is under the 0.75·h entry depth");
    }

    #[test]
    fn the_stroke_rises_to_the_rest_line_and_stops() {
        let mut p = swimmer(0.0, -3.0);
        p.cam_pitch = 1.4; // steep up
        p.input.forward = 1.0;
        for _ in 0..8 {
            p.tick(0.1);
        }
        assert!(p.swimming);
        let rest = -(SWIM_DEPTH_FRAC * CAPSULE_HEIGHT);
        assert!(
            (p.pos[2] - rest).abs() < 0.02,
            "held just under the surface: {}",
            p.pos[2]
        );
    }

    #[test]
    fn space_is_the_swim_jump() {
        let mut p = swimmer(0.0, -1.6);
        p.input.jump = true;
        p.tick(0.1);
        assert!(!p.swimming, "the launch clears the latch");
        assert!(p.vel[2] > 0.0, "rising");
    }

    #[test]
    fn leaving_the_water_releases_the_latch() {
        let mut p = swimmer(0.0, -1.6);
        p.tick(0.1);
        assert!(p.swimming);
        // The surface falls a yard: the 1/36 band lets go.
        p.pos[2] = -2.0;
        p.tiles.clear();
        p.tick(0.1);
        assert!(!p.swimming);
    }

    #[test]
    fn a_dive_stops_on_the_lakebed() {
        // Open water 8 yd deep over a flat bed: the capsule solver stops a dive
        // on the bed's surface, so the swimmer can never sink through the
        // terrain and out of the world.
        let mut p = swimmer(0.0, -1.6);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let (cx, cy) = world_to_chunk(p.pos[0], p.pos[1]);
        let b = ((cy % 16) * 16 + (cx % 16)) as usize;
        // The wet block and the bed both reach well past the swim: a dive
        // travels forward while it sinks, and leaving either one mid-test would
        // measure the fall, not the bed's hold.
        let mut wide = flat_liquid(0.0);
        wide.positions = vec![
            [17200.0, 17200.0, 0.0],
            [17200.0, 16900.0, 0.0],
            [16900.0, 17200.0, 0.0],
            [16900.0, 16900.0, 0.0],
        ];
        let tile = p.tiles.get_mut(&(tx, ty)).unwrap();
        tile.liquids.clear();
        tile.liquids.push(wide);
        tile.buckets[b].push(Tri::new(
            [16900.0, 16900.0, -8.0],
            [17200.0, 16900.0, -8.0],
            [17200.0, 17200.0, -8.0],
        ));
        tile.buckets[b].push(Tri::new(
            [16900.0, 16900.0, -8.0],
            [17200.0, 17200.0, -8.0],
            [16900.0, 17200.0, -8.0],
        ));
        p.cam_pitch = -1.2; // steep down
        p.input.forward = 1.0;
        let mut lowest = f32::INFINITY;
        for _ in 0..600 {
            p.tick(1.0 / 60.0);
            lowest = lowest.min(p.pos[2]);
        }
        assert!(p.swimming, "still in the water");
        assert!(
            lowest >= -8.0 - 1.0e-3,
            "the bed is solid from above: lowest {lowest}"
        );
        assert!(
            (p.pos[2] + 8.0).abs() < 0.05,
            "the dive settles on the bed: {}",
            p.pos[2]
        );
    }

    #[test]
    fn a_deep_breach_hops_and_re_latches_the_swim() {
        // `0x7c5de0`: the latch refuses a launch only while it rises inside
        // `speed/(2·g)` (~0.24 s, ~1.6 yd). Deep under the surface the hop
        // re-latches there - it does not fly a land jump out of the water.
        let mut p = swimmer(0.0, -5.0);
        p.tick(1.0 / 60.0);
        assert!(p.swimming);
        p.input.jump = true;
        p.tick(1.0 / 60.0);
        assert!(!p.swimming, "the breach clears the latch");
        let takeoff = p.pos[2];
        p.input.jump = false;
        let mut apex = takeoff;
        for _ in 0..40 {
            p.tick(1.0 / 60.0);
            apex = apex.max(p.pos[2]);
        }
        assert!(
            p.swimming,
            "re-latched at half the launch, not at the apex: z {}",
            p.pos[2]
        );
        assert!(
            (1.4..1.8).contains(&(apex - takeoff)),
            "a hop of about 1.6 yd: {takeoff} -> {apex}"
        );
        assert!(
            (p.vel[2]).abs() < 1.0e-6,
            "the launch's residual velocity is dropped"
        );
    }

    #[test]
    fn a_login_pose_under_the_terrain_is_lifted_onto_it() {
        // The server's saved pose can sit a few feet under the surface. Once
        // the floor set under the body is whole, the spawn snap puts the feet
        // on the surface instead of letting gravity carry them out of the map.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(-8800.0, -800.0);
        let (cx, cy) = world_to_chunk(-8800.0, -800.0);
        let b = ((cy % 16) * 16 + (cx % 16)) as usize;
        let mut col = TileCol::new();
        col.buckets[b].push(Tri::new(
            [-8820.0, -820.0, 10.0],
            [-8780.0, -820.0, 10.0],
            [-8780.0, -780.0, 10.0],
        ));
        col.buckets[b].push(Tri::new(
            [-8820.0, -820.0, 10.0],
            [-8780.0, -780.0, 10.0],
            [-8820.0, -780.0, 10.0],
        ));
        p.tiles.insert((tx, ty), col);
        p.set_server_pose([-8800.0, -800.0, 8.0], 0.0);
        p.tick(1.0 / 60.0);
        assert!(
            (p.pos[2] - 10.0).abs() < 1.0e-3,
            "lifted onto the surface: {}",
            p.pos[2]
        );
        for _ in 0..120 {
            p.tick(1.0 / 60.0);
        }
        assert!(p.grounded, "and standing there");
        assert!((p.pos[2] - 10.0).abs() < 1.0e-3, "no fall afterwards");
    }

    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn probe_city_fall_spot() {
        use crate::bn_init;
        let dir =
            std::ffi::CString::new(r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data").unwrap();
        let w = bn_init(dir.as_ptr());
        assert!(!w.is_null());
        let w = unsafe { &mut *w };
        let (sx, sy) = (-8975.69f32, 783.32f32);
        let (tx, ty) = Player::tile_of(sx, sy);
        println!("spot ({sx},{sy}) z=98.14 tile ({tx},{ty})");
        let mut tiles: HashMap<(i32, i32), TileCol> = HashMap::new();
        for dy in -1..=1 {
            for dx in -1..=1 {
                match load_tile(&w.archives, "Azeroth", tx + dx, ty + dy) {
                    Some(col) => {
                        println!(
                            "tile ({},{}) wmos={} doodads={}",
                            tx + dx,
                            ty + dy,
                            col.wmos.len(),
                            col.doodads.len()
                        );
                        for inst in &col.wmos {
                            println!(
                                "    wmo uid={} {} at {:?} rot {:?}",
                                inst.unique_id, inst.model, inst.position, inst.rotation
                            );
                        }
                        tiles.insert((tx + dx, ty + dy), col);
                    }
                    None => println!("tile ({},{}) MISSING", tx + dx, ty + dy),
                }
            }
        }
        let (cx, cy) = world_to_chunk(sx.clamp(-17066.0, 17066.0), sy.clamp(-17066.0, 17066.0));
        let b = ((cy % 16) * 16 + (cx % 16)) as usize;
        // Terrain-only floor at the spot: a fresh tile with no WMO baked.
        let terrain_only = {
            let mut q = Player::new("Azeroth", sx, sy);
            if let Some(c) = load_tile(&w.archives, "Azeroth", tx, ty) {
                q.tiles.insert((tx, ty), c);
            }
            q.ground(sx, sy, 1000.0)
        };
        println!("terrain-only z {terrain_only:?}");
        let mut cache = HashMap::new();
        let chain = w.chain.as_mut().unwrap();
        for (&(k0, k1), t) in tiles.iter_mut() {
            let mut built = false;
            let mut steps = 0;
            while t.wmo_inst < t.wmos.len() || !t.wmo_ready {
                built |= build_wmo_collision_step(
                    t,
                    k0,
                    k1,
                    chain,
                    &mut cache,
                    std::time::Duration::from_secs(3600),
                );
                steps += 1;
                if steps > 1000 {
                    break;
                }
            }
            println!(
                "baked tile ({k0},{k1}) built={built} steps={steps} wmo={} ready={}",
                t.wmos.len(),
                t.wmo_ready
            );
        }
        for (model, mesh) in cache.iter() {
            if !mesh.indices.is_empty() {
                continue;
            }
            println!("EMPTY collision {model}");
        }
        let mut p = Player::new("Azeroth", sx, sy);
        p.tiles = tiles;
        let tile = p.tiles.get(&(tx, ty)).unwrap();
        let cell = TileCol::wmo_cell(tx, ty, sx, sy);
        println!(
            "spot cell {:?} wmo tris {:?} terrain tris {}",
            cell,
            cell.map(|i| tile.wmo_cells[i as usize].len()),
            tile.buckets[b].len()
        );
        println!("collision z {:?}", p.ground(sx, sy, 1000.0));
        println!("collision z ceil 64 {:?}", p.ground(sx, sy, 64.0));
        let mut hits: Vec<f32> = cell
            .map(|i| {
                tile.wmo_cells[i as usize]
                    .iter()
                    .filter_map(|t| t.height_at(sx, sy, 1000.0))
                    .collect()
            })
            .unwrap_or_default();
        hits.sort_by(|a, b| a.partial_cmp(b).unwrap());
        hits.dedup_by(|a, b| (*a - *b).abs() < 0.01);
        println!("wmo floors at the spot: {:?}", hits);
        for ceil in [1000.0f32, 64.0] {
            println!("raster ceil {ceil}");
            for gy in (-40..=40).step_by(2) {
                let mut row = String::new();
                for gx in (-40..=40).step_by(2) {
                    let z = p.ground(sx + gx as f32, sy + gy as f32, ceil);
                    row.push_str(&match z {
                        Some((z, _)) => format!("{z:7.1}"),
                        None => "   none".to_string(),
                    });
                }
                println!("y{gy:+04}: {row}");
            }
        }
    }

    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn doodad_hulls_on_city_tiles() {
        use crate::{bn_doodad_count, bn_doodad_model_path, bn_init, bn_load_tile};
        let dir =
            std::ffi::CString::new(r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data").unwrap();
        let w = bn_init(dir.as_ptr());
        assert!(!w.is_null());
        let map = std::ffi::CString::new("Azeroth").unwrap();
        let (tx, ty) = Player::tile_of(-8800.0, -800.0);
        let mut buf = [0i8; 260];
        let mut paths: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut placed = 0;
        for dx in -1..=1 {
            for dy in -1..=1 {
                if bn_load_tile(w, map.as_ptr(), tx + dx, ty + dy) == 0 {
                    continue;
                }
                for i in 0..bn_doodad_count(w) {
                    if bn_doodad_model_path(w, i, buf.as_mut_ptr(), buf.len() as u32) == 0 {
                        continue;
                    }
                    placed += 1;
                    let path = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap() };
                    if seen.insert(path.to_string()) {
                        paths.push(path.to_string());
                    }
                }
            }
        }
        // The body traces exactly the M2 collision hull (`0x6bc9f0` gathers
        // `bounding_vertices`/`bounding_triangles`); a model that ships none -
        // grass, a canopy, a hanging lamp - is walked through.
        let start = std::time::Instant::now();
        let (mut solid, mut bare, mut tris) = (0usize, 0usize, 0usize);
        let mut biggest: Vec<(usize, String)> = Vec::new();
        for path in &paths {
            let chain = unsafe { (*w).chain.as_mut().unwrap() };
            let hull = benilla_formats::load_m2_collision_hull(chain, path).ok();
            match hull.filter(|h| !h.is_empty()) {
                Some(h) => {
                    solid += 1;
                    let n = h.indices.len() / 3;
                    tris += n;
                    biggest.push((n, path.clone()));
                }
                None => bare += 1,
            }
        }
        biggest.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
        println!(
            "placed {placed}, unique models {}, with hull {solid}, without {bare}, \
             {tris} hull triangles, parsed in {:?}",
            paths.len(),
            start.elapsed()
        );
        for (n, p) in biggest.iter().take(5) {
            println!("  {n} tris {p}");
        }
        assert!(solid > 0, "a city tile's props must carry hulls");
        assert!(bare > 0, "and hull-less props (grass) must appear too");
        crate::bn_shutdown(w);
    }

    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn terrain_buckets_answer_like_a_full_scan() {
        let dir =
            std::ffi::CString::new(r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data").unwrap();
        let w = crate::bn_init(dir.as_ptr());
        assert!(!w.is_null());
        let w = unsafe { &mut *w };
        let (x0, y0) = (-9463.0f32, 63.0f32);
        let (tx, ty) = Player::tile_of(x0, y0);
        let mut p = Player::new("Azeroth", x0, y0);
        for dy in -1..=1 {
            for dx in -1..=1 {
                if let Some(c) = load_tile(&w.archives, "Azeroth", tx + dx, ty + dy) {
                    p.tiles.insert((tx + dx, ty + dy), c);
                }
            }
        }
        let (mut total, mut mism) = (0, 0);
        for iy in -8..=8 {
            for ix in -8..=8 {
                let (x, y) = (x0 + ix as f32 * 5.0, y0 + iy as f32 * 5.0);
                let (tx, ty) = Player::tile_of(x, y);
                let Some(tile) = p.tiles.get(&(tx, ty)) else {
                    continue;
                };
                let mut best: Option<f32> = None;
                for b in &tile.buckets {
                    for t in b {
                        if let Some(z) = t.height_at(x, y, f32::INFINITY) {
                            best = Some(best.map_or(z, |bz: f32| bz.max(z)));
                        }
                    }
                }
                for c in &tile.wmo_cells {
                    for t in c {
                        if let Some(z) = t.height_at(x, y, f32::INFINITY) {
                            best = Some(best.map_or(z, |bz: f32| bz.max(z)));
                        }
                    }
                }
                let via = p.ground(x, y, f32::INFINITY).map(|(z, _)| z);
                total += 1;
                let bad = match (best, via) {
                    (Some(a), Some(b)) => (a - b).abs() > 0.05,
                    (None, None) => false,
                    _ => true,
                };
                if bad {
                    mism += 1;
                    if mism <= 20 {
                        println!("MISMATCH ({x:.1},{y:.1}) scan={best:?} ground={via:?}");
                    }
                }
            }
        }
        println!("samples {total} mismatched {mism}");
        assert!(total > 0, "the probe must land on loaded tiles");
        assert_eq!(mism, 0, "the bucketed ground probe must match a full scan");
        crate::bn_shutdown(w);
    }

    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn goldshire_walk_stays_on_the_terrain() {
        let dir =
            std::ffi::CString::new(r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data").unwrap();
        let w = crate::bn_init(dir.as_ptr());
        assert!(!w.is_null());
        let w = unsafe { &mut *w };
        let (x0, y0) = (-9463.0f32, 63.0f32);
        let mut p = Player::new("Azeroth", x0, y0);
        p.pos = [x0, y0, 200.0];
        p.snap_pending = true;
        for _ in 0..120 {
            p.ensure_tiles(&w.archives);
            p.tick(1.0 / 60.0);
        }
        println!(
            "settled ({:.1},{:.1},{:.2}) grounded={} floor={:?}",
            p.pos[0],
            p.pos[1],
            p.pos[2],
            p.grounded,
            p.ground(p.pos[0], p.pos[1], f32::INFINITY)
        );
        for leg in 0..8 {
            p.yaw = leg as f32 * std::f32::consts::PI / 4.0;
            p.input.forward = 1.0;
            for i in 0..300 {
                p.ensure_tiles(&w.archives);
                p.tick(1.0 / 60.0);
                if p.grounded {
                    // Grounded means the feet are on the floor: the terrain
                    // and the hulls answer the same probe the mover rides.
                    if let Some((z, _)) = p.ground(p.pos[0], p.pos[1], p.pos[2] + 2.0) {
                        assert!(
                            (z - p.pos[2]).abs() < 0.1,
                            "leg {leg} tick {i}: feet {:.2} vs floor {z:.2}",
                            p.pos[2]
                        );
                    }
                }
                if i % 60 == 0 || i == 299 {
                    println!(
                        "leg {leg} t={:.1}s ({:.1},{:.1},{:.2}) grounded={} floor={:?} vel={:.2}",
                        i as f32 / 60.0,
                        p.pos[0],
                        p.pos[1],
                        p.pos[2],
                        p.grounded,
                        p.ground(p.pos[0], p.pos[1], p.pos[2] + 60.0)
                            .map(|(z, _)| (z, (z - p.pos[2]).round())),
                        p.vel[2]
                    );
                }
            }
            p.input.forward = 0.0;
        }
        crate::bn_shutdown(w);
    }

    /// At Goldshire: a hull-bearing prop's own spot is solid to the body, and a
    /// hull-less one (grass, a canopy) stands in no body's way. Both are read
    /// off the installed tile's own placements, not a synthetic quad.
    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn goldshire_props_collide_by_their_hulls() {
        let dir =
            std::ffi::CString::new(r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data").unwrap();
        let w = crate::bn_init(dir.as_ptr());
        assert!(!w.is_null());
        let w = unsafe { &mut *w };
        let (x0, y0) = (-9463.0f32, 63.0f32);
        let mut p = Player::new("Azeroth", x0, y0);
        p.pos = [x0, y0, 200.0];
        p.ensure_tiles(&w.archives);
        // Fold every resident tile's doodad hulls and WMO floors: `ensure_wmo`
        // works on the tile the body stands in, so the body visits each one.
        let keys: Vec<(i32, i32)> = p.tiles.keys().copied().collect();
        let mut cache = HashMap::new();
        for k in keys {
            let Some(t) = p.tiles.get(&k) else { continue };
            let Some(d) = t
                .doodads
                .iter()
                .find(|d| Player::tile_of(d.position[0], d.position[1]) == k)
            else {
                continue;
            };
            p.pos[0] = d.position[0];
            p.pos[1] = d.position[1];
            for _ in 0..2000 {
                p.ensure_wmo(&mut w.chain, &mut cache);
                let done = p
                    .tiles
                    .get(&k)
                    .is_some_and(|t| t.wmo_ready && t.doodad_next >= t.doodads.len());
                if done {
                    break;
                }
            }
        }
        let (tx, ty) = Player::tile_of(x0, y0);
        let tile = p.tiles.get(&(tx, ty)).expect("Goldshire tile resident");
        let (mut solid, mut bare) = (0usize, 0usize);
        let mut blocked_rings = 0usize;
        let mut blocked_props = 0usize;
        let mut bare_wrong = 0usize;
        for d in &tile.doodads {
            match p.hulls.get(&d.model) {
                Some(Some(_)) => {
                    solid += 1;
                    // The body approaches at ground level from eight sides: a
                    // trunk's hull has to answer from at least one of them.
                    let Some((feet, _)) = p.ground(d.position[0], d.position[1], f32::INFINITY)
                    else {
                        continue;
                    };
                    let mut hit = false;
                    for k in 0..8 {
                        let a = k as f32 * std::f32::consts::PI / 4.0;
                        for r in [0.4, 1.0, 2.0, 3.0] {
                            let (hx, hy) =
                                (d.position[0] + a.cos() * r, d.position[1] + a.sin() * r);
                            if p.wall_hit(hx, hy, feet) {
                                blocked_rings += 1;
                                hit = true;
                            }
                        }
                    }
                    if hit {
                        blocked_props += 1;
                    }
                }
                // No hull in the model: nothing is folded, so the body walks
                // its spot. A model the hull table missed would be a parsing
                // bug, and every one here would show up in `bare_wrong`.
                None | Some(None) => {
                    bare += 1;
                    if p.hulls.get(&d.model).is_some_and(|h| h.is_some()) {
                        bare_wrong += 1;
                    }
                }
            }
        }
        println!(
            "goldshire: {solid} hulled placements ({blocked_props} stop the body, \
             {blocked_rings} blocked probes), {bare} hull-less ({bare_wrong} wrongly hulled)"
        );
        assert!(solid > 0 && bare > 0, "both kinds must be placed here");
        assert!(blocked_props > 0, "a hulled prop must stop the body");
        assert_eq!(bare_wrong, 0, "a hull-less prop is never solid");
        crate::bn_shutdown(w);
    }

    /// The logged fall south of Goldshire (`run_qm24.txt`): the body walked
    /// into Crystal Lake, kept walking its bed under the surface, met the
    /// rising far shore where no floor answered above the feet and dropped out
    /// of the map. The swim latch must carry it instead: from the logged
    /// north-shore heartbeat at (-9404.8, -114.1) the body swims the logged
    /// heading, wades out on the far bank and never leaves the surface.
    #[test]
    fn swimming_carries_the_body_across_crystal_lake() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut archives = Vec::new();
        for e in std::fs::read_dir(&data).expect("data dir").flatten() {
            let path = e.path();
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mpq"))
            {
                if let Ok(a) = benilla_mpq::Archive::open(&path) {
                    archives.push(a);
                }
            }
        }
        assert!(!archives.is_empty(), "the install's MPQs open");
        let mut chain = benilla_formats::Chain::open(&data).ok();
        let mut cache = HashMap::new();

        let mut p = Player::new("Azeroth", -9404.8, -114.1);
        p.pos = [-9404.8, -114.1, 57.9];
        p.grounded = true;
        p.last_ground = p.pos;
        for _ in 0..9 {
            p.ensure_tiles(&archives);
        }
        p.input.forward = 1.0;
        p.yaw = (-148.9f32 + 114.1).atan2(-9392.8f32 + 9404.8);
        p.model_yaw = p.yaw;

        let mut swim_ticks = 0;
        let mut min_z = f32::INFINITY;
        for _ in 0..1200 {
            p.ensure_tiles(&archives);
            p.ensure_wmo(&mut chain, &mut cache);
            p.tick(1.0 / 60.0);
            if p.swimming {
                swim_ticks += 1;
            }
            min_z = min_z.min(p.pos[2]);
            if let (Some(t), true) = (p.terrain_z(p.pos[0], p.pos[1]), p.grounded) {
                assert!(
                    p.pos[2] >= t - 0.05,
                    "feet under the terrain at ({:.1},{:.1}): {:.2} < {:.2}",
                    p.pos[0],
                    p.pos[1],
                    p.pos[2],
                    t
                );
            }
        }
        println!("crystal lake: {swim_ticks} swim ticks, min z {min_z:.2}");
        assert!(swim_ticks > 60, "the crossing is a swim, not a bed walk");
        assert!(min_z > 55.0, "the body never sinks toward the void");
        assert!(p.pos[1] < -150.0, "the far shore is reached");
        assert!(p.grounded, "and the body walks out onto it");
    }
}

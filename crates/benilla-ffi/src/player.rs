//! Middle-layer player: benilla's locomotion, collision probe and camera.
//!
//! The reference client owns movement (`player/state.rs`, `player/mover.rs`)
//! and the third-person camera (`player/camera.rs`) inside the Bevy app, which
//! cannot be called from here (it pulls the whole engine). This module keeps
//! the same constants, units (WoW yards) and laws - run 7 yd/s, backpedal
//! 4.5/7, walk 2.5/7, gravity 19.29, jump 7.96, terminal 60.15, 50 deg ground
//! slope, 1 yd step-up, ±89 deg pitch, 0.003 rad per mouse unit - and answers
//! the ground with the same ADT/WMO geometry benilla loads, so the UE host is
//! a pure renderer that just applies the returned transform.
//!
//! Player position is raw WoW world yards - the same space the ADT chunks and
//! MODF placements use - so `(x, -y, z) * 100` is the UE transform.

use std::collections::HashMap;

use benilla_formats::{adt_to_tile_mesh, world_to_chunk};

use crate::{placement_quat, q_rot, BnWorld};

// ---- benilla-app/src/player/state.rs constants (read-only port) ----
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
// The capsule the reference traces with (`CAPSULE_RADIUS`/`CAPSULE_HEIGHT`);
// the probe below is a point at the feet plus the step window, which is the
// same corridor for a walker on the floors this data ships.
#[allow(dead_code)]
const CAPSULE_RADIUS: f32 = 1.0 / 3.0;
#[allow(dead_code)]
const CAPSULE_HEIGHT: f32 = 2.027_777_7;

// ---- benilla-app/src/player/camera.rs ----
const CAM_PITCH_LIMIT: f32 = 89.0 * std::f32::consts::PI / 180.0;
const LOOK_SENSITIVITY: f32 = 0.028;
const CAM_DIST_DEFAULT: f32 = 15.0; // camera_zoom.rs's deviation, for a wider view
/// The shortest boom the collision probe will pull to: any closer and the eye
/// sits inside the body (the reference's first-person cut is not modelled).
const CAM_BOOM_MIN: f32 = 1.0;
/// `cameraYawSmoothSpeed` default 180°/s: the smart follow's return rate.
const FOLLOW_RATE: f32 = std::f32::consts::PI;
/// `creature_anim::select::STRAFE_BLEND_RATE`: the body heading eases to the
/// strafe offset at this rate (/s).
const STRAFE_BLEND_RATE: f32 = 17.26;
/// `state.rs::TURN_RATE`: the keyboard turn rate, π rad/s (the server's own
/// value; the client ships no default).
const TURN_RATE: f32 = std::f32::consts::PI;
/// `state.rs::TURN_RATE_MOVING`: the scale while translating or falling.
const TURN_RATE_MOVING: f32 = 0.75;
/// Roam-mode flight speed (yd/s): a zone tour without waiting on terrain.
const FLY_SPEED: f32 = 30.0;

// ---- benilla-app/src/player/camera.rs pivot presets ----
/// A scale-1 Human Male's framing pivot: standing 1.9002692, swimming
/// 0.3882572 lower (`model_pivot_height`, `cam+0x124`).
const CAM_PIVOT_STAND: f32 = 1.900_269_2;
const CAM_PIVOT_SWIM: f32 = 1.512_012;
/// The pivot channel eases at 1.2 yd/s (the reference's `|Δh| / 1.2` s).
const CAM_PIVOT_SMOOTH: f32 = 1.2;
/// `model_pivot_height`'s `[5/6, 15]` band.
const CAM_PIVOT_FLOOR: f32 = 5.0 / 6.0;
const CAM_PIVOT_CEIL: f32 = 15.0;

// ---- benilla-app/src/player/camera_water.rs ----
/// The resting corridor floor, and the pivot's depth under the surface while
/// submerged (`[0x8089cc]`).
const WATER_REST: f32 = 5.0 / 6.0;
/// The surface band's width, and the pivot's lift above the waterline in it
/// (`[0x8089d0]`).
const WATER_SURFACE_BAND: f32 = 2.0 / 9.0;
/// The submerge band's outer edge (`[0x8089d4]`).
const WATER_SUBMERGE_EDGE: f32 = 5.0 / 9.0;
/// The least head-room the pivot keeps above the corridor floor (`[0x808a04]`).
const WATER_MIN_HEADROOM: f32 = 1.0 / 9.0;

/// Mount speed bonus in 1.12: +60% (the client's riding speed).
const MOUNT_SPEED_RATIO: f32 = 1.6;

// ---- benilla-app/src/player/swim.rs (read-only port) ----
/// Stock `MOVE_SWIM` (yd/s), vmangos `Unit.cpp:71`.
const SWIM_SPEED: f32 = 4.722_222;
/// Stock `MOVE_SWIM_BACK` (yd/s), vmangos `Unit.cpp:72`.
const SWIM_BACK_SPEED: f32 = 2.5;
/// The swim jump's seed (yd/s), about 14 % over the land jump.
const SWIM_JUMP_SPEED: f32 = 9.096_748;
/// The share of the collision height the water must cover to swim (`0x8012cc`).
const SWIM_DEPTH_FRAC: f32 = 0.75;
/// The enter/leave band (yd, `0x7ff9d0`): hold between `0.75·h - 1/36` and `0.75·h`.
const SWIM_HYSTERESIS: f32 = 1.0 / 36.0;

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
    /// AnimationData.dbc id to play (0 Stand, 4 Walk, 5 Run, 13 WalkBackwards,
    /// 16 AttackUnarmed, 37 JumpStart, 40 Fall, 91 Mount).
    pub anim: u32,
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
/// the eased channel value; the reference's head-room probe is not built, so the
/// reach is full. A surface swimmer's pivot rises to `surface + 2/9` and a diver's
/// sits `5/6` under the surface, which is what keeps the boom out of the water.
fn camera_pivot_height(target: f32, live: f32, surface: Option<f32>, feet: f32) -> f32 {
    let (floor, cap) = match surface {
        None => (WATER_REST, live),
        Some(s) => {
            let d = s - feet;
            let excess = d - target;
            // A tie at 2/9 lands in the submerge band and one at 5/9 in clear,
            // the reference's own tie-breaks.
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
    (floor + reach).min(cap).clamp(CAM_PIVOT_FLOOR, CAM_PIVOT_CEIL)
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

// ---- doodad footprints ----
/// The slice of a model that counts as its base (yd above the model origin):
/// knee height keeps trunks, crates, fences and sign posts, and leaves out
/// canopies and awnings the body walks under.
const DOODAD_SLICE: f32 = 1.0;
/// Below this base extent (yd) a doodad is scenery, not an obstacle.
const DOODAD_MIN_EXTENT: f32 = 0.15;

/// A doodad's standing obstacle: an oriented box over its base slice (a
/// cylinder would turn a fence into a blob). `ux`/`uy` are the model's local
/// axes projected to the world.
#[derive(Clone, Copy)]
struct DoodadCollider {
    /// Base centre (x, y) and the lowest point of the model, world yards.
    c: [f32; 3],
    ux: [f32; 2],
    uy: [f32; 2],
    hx: f32,
    hy: f32,
    /// Top of the model; a footprint floating above the body does not block.
    top: f32,
}

/// An M2's base footprint in model-local yards.
#[derive(Clone, Copy)]
struct Footprint {
    cx: f32,
    cy: f32,
    hx: f32,
    hy: f32,
    zmin: f32,
    zmax: f32,
}

/// The base footprint of an M2 doodad from its render mesh: the vertices
/// within knee height of the model origin decide the box, so a crate's sides
/// or a trunk are solid while its canopy is not.
fn m2_footprint(chain: &mut benilla_formats::Chain, raw_path: &str) -> Option<Footprint> {
    let subs = benilla_formats::load_m2_mesh(chain, raw_path).ok()?;
    let mut lo = [f32::INFINITY; 2];
    let mut hi = [f32::NEG_INFINITY; 2];
    let mut zmin = f32::INFINITY;
    let mut zmax = f32::NEG_INFINITY;
    let mut any = false;
    for s in &subs {
        for p in &s.positions {
            zmin = zmin.min(p[2]);
            zmax = zmax.max(p[2]);
            if p[2] >= -0.5 && p[2] <= DOODAD_SLICE {
                any = true;
                lo[0] = lo[0].min(p[0]);
                lo[1] = lo[1].min(p[1]);
                hi[0] = hi[0].max(p[0]);
                hi[1] = hi[1].max(p[1]);
            }
        }
    }
    if !any || zmax <= zmin {
        return None;
    }
    let hx = (hi[0] - lo[0]) * 0.5;
    let hy = (hi[1] - lo[1]) * 0.5;
    if hx.max(hy) < DOODAD_MIN_EXTENT {
        return None;
    }
    Some(Footprint {
        cx: (lo[0] + hi[0]) * 0.5,
        cy: (lo[1] + hi[1]) * 0.5,
        hx,
        hy,
        zmin,
        zmax,
    })
}

/// One ADT tile's collision: terrain triangles bucketed by the chunk they
/// came from (16x16 buckets = the chunks), plus the WMO floors once built.
struct TileCol {
    buckets: Vec<Vec<Tri>>,
    /// Terrain triangles by chunk bucket - `buckets.len() == 256`.
    /// WMO triangles bucketed on a [`WMO_GRID`] grid of the tile; a triangle
    /// lands in every cell its XY bounds overlap, so the huge city floors
    /// answer from any point instead of a full-scan every frame.
    wmo_cells: Vec<Vec<Tri>>,
    /// Doodad base footprints on the same grid (the 1.12 M2s ship no collision
    /// hull: a crate's sides or a trunk are their own base slice).
    doodad_cells: Vec<Vec<DoodadCollider>>,
    /// MODF placements, kept from the ADT so the WMO floors can be built once.
    wmos: Vec<benilla_formats::WmoInstance>,
    /// MDDF placements, whose base footprints become collision cylinders.
    doodads: Vec<benilla_formats::Doodad>,
    /// MCLQ surfaces, sampled for the swim latch and the floating resolver.
    liquids: Vec<benilla_formats::LiquidMesh>,
    /// WMO MLIQ surfaces (the canals and fountains) with their placement, in
    /// model space: a query is mirrored into model space instead of baking.
    wmo_liquids: Vec<(benilla_formats::WmoInstance, benilla_formats::LiquidMesh)>,
    wmo_ready: bool,
    /// Doodad cursor: footprints are built a few models at a time (the first
    /// cold parse of ~200 unique M2 meshes costs half a second), so the frame
    /// never stalls on a tile load.
    doodad_next: usize,
}

impl TileCol {
    fn new() -> Self {
        Self {
            buckets: (0..256).map(|_| Vec::new()).collect(),
            wmo_cells: (0..(WMO_GRID * WMO_GRID) as usize).map(|_| Vec::new()).collect(),
            doodad_cells: (0..(WMO_GRID * WMO_GRID) as usize).map(|_| Vec::new()).collect(),
            wmos: Vec::new(),
            doodads: Vec::new(),
            liquids: Vec::new(),
            wmo_liquids: Vec::new(),
            wmo_ready: false,
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

/// Bake one tile's WMO walking triangles + MLIQ surfaces into its grid.
/// Returns true when at least one model contributed geometry (the caller
/// then re-snaps the body so it does not start under a floor).
fn build_wmo_collision(
    t: &mut TileCol,
    tx: i32,
    ty: i32,
    chain: &mut benilla_formats::Chain,
    cache: &mut HashMap<String, benilla_formats::CollisionMesh>,
) -> bool {
    let mut built = false;
    for inst in &t.wmos {
        if !cache.contains_key(&inst.model) {
            let Ok(mesh) = benilla_formats::load_wmo_collision_tris(chain, &inst.model) else {
                continue;
            };
            cache.insert(inst.model.clone(), mesh);
        }
        let Some(mesh) = cache.get(&inst.model) else { continue };
        if mesh.is_empty() {
            continue;
        }
        built = true;
        let q = placement_quat(inst.rotation);
        let mut world = Vec::with_capacity(mesh.positions.len());
        for p in &mesh.positions {
            let r = q_rot(q, *p);
            world.push([
                r[0] + inst.position[0],
                r[1] + inst.position[1],
                r[2] + inst.position[2],
            ]);
        }
        for tri in mesh.indices.chunks_exact(3) {
            let (a, b, c) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
            let (Some(&va), Some(&vb), Some(&vc)) = (world.get(a), world.get(b), world.get(c)) else {
                continue;
            };
            let tri_c = Tri::new(va, vb, vc);
            let (x0, y0, x1, y1) = TileCol::wmo_span(tx, ty, &tri_c);
            for ly in y0..=y1 {
                for lx in x0..=x1 {
                    t.wmo_cells[(ly * WMO_GRID + lx) as usize].push(tri_c);
                }
            }
        }
    }
    // WMO MLIQ: the canals' and fountains' water, model space + placement
    // (a rotation-safe query, so an arbitrary yaw still samples).
    let mut liquid_cache: HashMap<String, Vec<benilla_formats::LiquidMesh>> = HashMap::new();
    for inst in &t.wmos {
        if !liquid_cache.contains_key(&inst.model) {
            let meshes = crate::read_wmo_group_liquids(Some(&mut *chain), &inst.model);
            liquid_cache.insert(inst.model.clone(), meshes);
        }
        let Some(meshes) = liquid_cache.get(&inst.model) else { continue };
        for m in meshes {
            t.wmo_liquids.push((inst.clone(), m.clone()));
        }
    }
    built
}

/// The liquid surface height under (x, y) in one liquid mesh, sampled
/// bilinearly over the containing cell as the reference does (`0x6b7500`).
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
    Some(tl * (1.0 - u) * (1.0 - v)
        + tr * (1.0 - u) * v
        + bl * u * (1.0 - v)
        + br * u * v)
}

/// Ray/triangle intersection (Möller-Trumbore, both faces): the ray length to
/// the hit, if any.
fn ray_tri(o: [f32; 3], d: [f32; 3], t: &Tri) -> Option<f32> {
    let [a, b, c] = t.v;
    let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let cross = |u: [f32; 3], v: [f32; 3]| {
        [u[1] * v[2] - u[2] * v[1],
         u[2] * v[0] - u[0] * v[2],
         u[0] * v[1] - u[1] * v[0]]
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
    /// strafing (`0x7c5360`).
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
    /// Latched by the water depth over the feet (`0x6030c0`); gravity is
    /// bypassed while set and the feet float at `0.75·h` under the surface.
    swimming: bool,
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
    mounted: bool,
    attack_left: f32,
    tiles: HashMap<(i32, i32), TileCol>,
    /// Doodad model footprints, parsed once per model path (`None` = nothing
    /// at body height).
    footprints: HashMap<String, Option<Footprint>>,
    /// Tiles already asked for and found missing in the archives.
    missing: Vec<(i32, i32)>,
    /// Set when a tile's WMO floors are built: the spawn sits on the terrain
    /// under a city, so climb to the lowest walkable WMO floor over it.
    snap_pending: bool,
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
            mounted: false,
            attack_left: 0.0,
            tiles: HashMap::new(),
            footprints: HashMap::new(),
            missing: Vec::new(),
            snap_pending: false,
            last_ground: [x, y, 0.0],
            void_time: 0.0,
            fall_time: 0.0,
            fly: false,
            look_active: false,
        }
    }

    fn tile_of(x: f32, y: f32) -> (i32, i32) {
        let (cx, cy) = world_to_chunk(x, y);
        ((cx / 16) as i32, (cy / 16) as i32)
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
            match load_tile(archives, &self.map, k.0, k.1) {
                Some(t) => {
                    self.tiles.insert(k, t);
                }
                None => self.missing.push(k),
            }
        }
        self.tiles.retain(|k, _| {
            (k.0 - tx).abs() <= 1 && (k.1 - ty).abs() <= 1
        });
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
            t.wmo_ready = true;
            if build_wmo_collision(t, tx, ty, chain, cache) {
                self.snap_pending = true;
            }
        }
        // Doodad footprints: the 1.12 M2s ship no collision hull (all 5,115
        // doodads around Stormwind have an empty one), so the base slice of
        // the render mesh stands in - a crate's sides, a trunk, a fence post -
        // but not the canopy, which would wall off a forest. The first tile
        // cold-parses ~200 unique models, so this runs on a 2ms budget per
        // tick instead of all at once.
        let start = std::time::Instant::now();
        while t.doodad_next < t.doodads.len() {
            let i = t.doodad_next;
            t.doodad_next += 1;
            let d = &t.doodads[i];
            if !self.footprints.contains_key(&d.model) {
                let fp = m2_footprint(chain, &d.model);
                self.footprints.insert(d.model.clone(), fp);
            }
            let Some(Some(fp)) = self.footprints.get(&d.model) else { continue };
            let q = placement_quat(d.rotation);
            let off = q_rot(q, [fp.cx * d.scale, fp.cy * d.scale, 0.0]);
            let ax = q_rot(q, [1.0, 0.0, 0.0]);
            let ay = q_rot(q, [0.0, 1.0, 0.0]);
            let (ax, ay) = ([ax[0], ax[1]], [ay[0], ay[1]]);
            let (lx, ly) = (
                (ax[0] * ax[0] + ax[1] * ax[1]).sqrt(),
                (ay[0] * ay[0] + ay[1] * ay[1]).sqrt(),
            );
            if lx < 1.0e-4 || ly < 1.0e-4 {
                continue;
            }
            let (hx, hy) = (fp.hx * d.scale, fp.hy * d.scale);
            let reach = (hx * hx + hy * hy).sqrt();
            let col = DoodadCollider {
                c: [
                    d.position[0] + off[0],
                    d.position[1] + off[1],
                    d.position[2] + fp.zmin * d.scale,
                ],
                ux: [ax[0] / lx, ax[1] / lx],
                uy: [ay[0] / ly, ay[1] / ly],
                hx,
                hy,
                top: d.position[2] + fp.zmax * d.scale,
            };
            let (ax0, ay0) = TileCol::wmo_local(tx, ty, col.c[0] - reach, col.c[1] - reach);
            let (bx0, by0) = TileCol::wmo_local(tx, ty, col.c[0] + reach, col.c[1] + reach);
            let (x0, x1) = (
                ax0.min(bx0).clamp(0, WMO_GRID - 1),
                ax0.max(bx0).clamp(0, WMO_GRID - 1),
            );
            let (y0, y1) = (
                ay0.min(by0).clamp(0, WMO_GRID - 1),
                ay0.max(by0).clamp(0, WMO_GRID - 1),
            );
            for ly in y0..=y1 {
                for lx in x0..=x1 {
                    t.doodad_cells[(ly * WMO_GRID + lx) as usize].push(col);
                }
            }
            if start.elapsed().as_millis() >= 2 {
                break;
            }
        }
    }

    /// The highest walkable surface under (x, y) at or below `ceil`.
    fn ground(&self, x: f32, y: f32, ceil: f32) -> Option<(f32, f32)> {
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
            }
        } else {
            // Crossing a border before the neighbour finished loading: any
            // loaded tile still answers in world coords.
            for (&(k0, k1), tile) in &self.tiles {
                if let Some(i) = TileCol::wmo_cell(k0, k1, x, y) {
                    for t in &tile.wmo_cells[i] {
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

    /// The liquid surface over (x, y), the lowest of any stacked blocks
    /// (`benilla_world`'s `liquid_at`, so a stream over the sea reads as the
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

    /// A steep WMO face within `radius` of `p` (a wall; floors are excluded).
    /// Faces entirely below `step_base + STEP_UP_HEIGHT` are steps, not walls:
    /// the reference's mover climbs them, so its wall test must not see them.
    /// Pass `f32::NEG_INFINITY` to keep every face (the camera's probe).
    fn hit_steep(&self, p: [f32; 3], radius: f32, step_base: f32) -> bool {
        let (tx, ty) = Self::tile_of(p[0], p[1]);
        let probe = |ktx: i32, kty: i32, tile: &TileCol| -> bool {
            let Some(i) = TileCol::wmo_cell(ktx, kty, p[0], p[1]) else {
                return false;
            };
            let (lx, ly) = ((i % WMO_GRID as usize) as i32,
                            (i / WMO_GRID as usize) as i32);
            let r2 = radius * radius;
            for gy in (ly - 1)..=(ly + 1) {
                for gx in (lx - 1)..=(lx + 1) {
                    if !(0..WMO_GRID).contains(&gx) || !(0..WMO_GRID).contains(&gy) {
                        continue;
                    }
                    for t in &tile.wmo_cells[(gy * WMO_GRID + gx) as usize] {
                        if t.n[2].abs() >= GROUND_COS {
                            continue;
                        }
                        let top = t.v[0][2].max(t.v[1][2]).max(t.v[2][2]);
                        if top <= step_base + STEP_UP_HEIGHT {
                            continue; // a curb the mover steps over
                        }
                        let q = closest_on_tri(p, t);
                        let d = [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
                        if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] < r2 {
                            return true;
                        }
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

    /// Horizontal blocking by a steep WMO face: the reference's capsule (a
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
        }) || self.doodad_hit(x, y, feet)
    }

    /// True when a doodad's base cylinder overlaps the body at (`x`, `y`).
    fn doodad_hit(&self, x: f32, y: f32, feet: f32) -> bool {
        let (tx, ty) = Self::tile_of(x, y);
        let Some(tile) = self.tiles.get(&(tx, ty)) else { return false };
        let Some(ci) = TileCol::wmo_cell(tx, ty, x, y) else { return false };
        let (gx, gy) = ((ci % WMO_GRID as usize) as i32, (ci / WMO_GRID as usize) as i32);
        let lo = feet + 0.1;
        let hi = feet + CAPSULE_HEIGHT;
        for ly in (gy - 1)..=(gy + 1) {
            for lx in (gx - 1)..=(gx + 1) {
                if !(0..WMO_GRID).contains(&lx) || !(0..WMO_GRID).contains(&ly) {
                    continue;
                }
                for d in &tile.doodad_cells[(ly * WMO_GRID + lx) as usize] {
                    if d.top <= lo || d.c[2] >= hi {
                        continue;
                    }
                    let dx = x - d.c[0];
                    let dy = y - d.c[1];
                    // Closest point on the oriented base box to the body axis.
                    let lx = dx * d.ux[0] + dy * d.ux[1];
                    let ly = dx * d.uy[0] + dy * d.uy[1];
                    let ex = lx - lx.clamp(-d.hx, d.hx);
                    let ey = ly - ly.clamp(-d.hy, d.hy);
                    if ex * ex + ey * ey < CAPSULE_RADIUS * CAPSULE_RADIUS {
                        return true;
                    }
                }
            }
        }
        false
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
    fn camera_fwd(&self) -> [f32; 3] {
        let (sp, cp) = self.cam_pitch.sin_cos();
        let (sy, cy) = self.cam_yaw.sin_cos();
        [cp * cy, -cp * sy, sp]
    }

    /// True when the boom may reach `p`: 0.3 yd clear of the floor.
    fn boom_clear(&self, p: [f32; 3]) -> bool {
        match self.ground(p[0], p[1], p[2]) {
            Some((g, _)) => p[2] >= g + 0.3,
            None => true,
        }
    }

    /// Where the boom first enters a steep WMO face, if it does. Point probes
    /// miss walls seen edge-on (a wall joining two samples falls between
    /// them), so the segment is tested against the triangles itself.
    fn boom_wall_hit(&self, pivot: [f32; 3], dir: [f32; 3], max: f32) -> Option<f32> {
        let end = [pivot[0] + dir[0] * max, pivot[1] + dir[1] * max,
                   pivot[2] + dir[2] * max];
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
                        if t.n[2].abs() >= GROUND_COS {
                            continue; // a floor, left to the ground probe
                        }
                        if let Some(d) = ray_tri(pivot, dir, t) {
                            if d <= max && best.map_or(true, |b| d < b) {
                                best = Some(d);
                            }
                        }
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
                for (&(k0, k1), tile) in &self.tiles {
                    scan(k0, k1, tile);
                }
            }
        }
        best
    }

    /// The longest clear boom up to `max`: the wall the segment first enters
    /// (less a 0.3 yd stand-off), then eight floor samples and a bisection for
    /// the exact stand-off, floored at [`CAM_BOOM_MIN`] so the eye never ends
    /// up inside the body.
    fn solid_boom(&self, pivot: [f32; 3], fwd: [f32; 3], max: f32) -> f32 {
        if max <= 0.0 {
            return 0.0;
        }
        let dir = [-fwd[0], -fwd[1], -fwd[2]];
        let mut limit = max;
        if let Some(t) = self.boom_wall_hit(pivot, dir, max) {
            limit = (t - 0.3).max(0.0);
        }
        let at = |f: f32| {
            [pivot[0] + dir[0] * f, pivot[1] + dir[1] * f, pivot[2] + dir[2] * f]
        };
        let mut prev = 0.0;
        for i in 1..=8 {
            let f = limit * i as f32 / 8.0;
            if self.boom_clear(at(f)) {
                prev = f;
                continue;
            }
            let (mut lo, mut hi) = (prev, f);
            for _ in 0..6 {
                let mid = 0.5 * (lo + hi);
                if self.boom_clear(at(mid)) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            return lo.max(CAM_BOOM_MIN.min(max));
        }
        limit.max(CAM_BOOM_MIN.min(max))
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
            self.model_yaw = wrap_pi(self.model_yaw + d);
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
        // Body: the aim plus the same strafe counter-twist as walking.
        let offset = strafe_body_offset(forward, strafe);
        let cur = wrap_pi(self.model_yaw - self.yaw);
        let eased = cur + (offset - cur) * (1.0 - (-STRAFE_BLEND_RATE * dt).exp());
        self.model_yaw = wrap_pi(self.yaw + eased);
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
        self.cam_distance +=
            (self.cam_target_distance - self.cam_distance).clamp(-step_d, step_d);
        self.cam_live = self.cam_distance;
    }

    /// One frame: integrate benilla's locomotion, resolve the floor, then place
    /// the camera. `dt` is clamped like the client's frame slice.
    fn tick(&mut self, dt: f32) {
        // Consume the drag latch first: the host reports the held look button
        // on every frame, including frames with no pointer delta.
        let looking = self.look_active;
        self.look_active = false;
        let dt = dt.clamp(0.0, 0.1);
        if dt <= 0.0 {
            return;
        }
        if self.fly {
            self.tick_fly(dt);
            return;
        }
        if self.snap_pending {
            self.snap_pending = false;
            if let Some(z) = self.lowest_floor_above(self.pos[0], self.pos[1], self.pos[2], 60.0)
            {
                self.pos[2] = z;
                self.vel[2] = 0.0;
                self.grounded = true;
                self.last_ground = self.pos;
            }
        }
        let feet = self.pos;
        let forward = self.input.forward.clamp(-1.0, 1.0);
        let strafe = self.input.strafe.clamp(-1.0, 1.0);

        // The swim latch (`0x6030c0`): the depth over the feet against
        // `0.75·h`, held in a 1/36-yd band. A rising launch is not caught, so
        // a swim jump can leave the water.
        let surface = self.water(self.pos[0], self.pos[1]);
        let enter = SWIM_DEPTH_FRAC * CAPSULE_HEIGHT;
        self.swimming = match surface {
            None => false,
            Some(s) => {
                let depth = s - self.pos[2];
                if self.swimming {
                    depth >= enter - SWIM_HYSTERESIS
                } else {
                    depth > enter && self.vel[2] <= 0.0
                }
            }
        };
        if self.swimming {
            // The client dismounts on entering deep water.
            self.mounted = false;
        }
        // The pivot channel (`0x50f160`): the standing preset, the swim drop in
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

        // The 1.12 control model (`0x7c5360`): A/D turn the character unless the
        // right mouse button is held, when the host reports them as strafe. A
        // keyboard turn is left-positive and carries the body and the camera
        // rigidly (`camera.rs`); translating or falling scales it by 0.75.
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
            self.model_yaw = wrap_pi(self.model_yaw + d);
            self.cam_yaw = wrap_pi(self.cam_yaw - d);
        }

        // WASD moves in the aim's basis, as the reference does (`state.rs`:
        // "the facing ... is the basis WASD move in"); orbiting the camera
        // does not steer the body. WoW axes: +X north, +Y west, so facing
        // yaw gives forward (cos, sin) and the player's right (sin, -cos).
        let (sy, cy) = self.yaw.sin_cos();
        let fwd = [cy, sy];
        let right = [sy, -cy];
        let mag;
        if self.swimming {
            // The travel basis (`0x7c5880`): forward is the facing pitched by
            // the aim, strafe stays level, and there is no vertical input.
            let (sp, cp) = self.cam_pitch.sin_cos();
            let mut v = [fwd[0] * cp * forward + right[0] * strafe,
                         fwd[1] * cp * forward + right[1] * strafe,
                         sp * forward];
            let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if len > 1.0 {
                for c in &mut v {
                    *c /= len;
                }
            }
            mag = len.min(1.0);
            let speed = if forward < -0.01 { SWIM_BACK_SPEED } else { SWIM_SPEED };
            self.vel = [v[0] * speed, v[1] * speed, v[2] * speed];
            // The rise cap (`0x632ba0`): the stroke's upward speed is clamped
            // to what reaches the rest line this frame; the surplus turns into
            // level travel at the same speed.
            if let Some(s) = surface {
                let cap = ((s - enter - self.pos[2]) / dt).max(0.0);
                if self.vel[2] > cap {
                    let speed = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]
                        + self.vel[2] * self.vel[2]).sqrt();
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
            let mut dir = [fwd[0] * forward + right[0] * strafe,
                           fwd[1] * forward + right[1] * strafe];
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

        // The rendered body eases to the aim plus the strafe counter-twist
        // (`creature_anim::select::strafe_body_offset`): ±π/2 for a pure
        // strafe, ±π/4 with forward or back, mirrored while backpedaling.
        let offset = strafe_body_offset(forward, strafe);
        let cur = wrap_pi(self.model_yaw - self.yaw);
        let eased = cur + (offset - cur) * (1.0 - (-STRAFE_BLEND_RATE * dt).exp());
        self.model_yaw = wrap_pi(self.yaw + eased);

        // Smart follow (`cameraSmoothStyle` default): while the character is
        // driven, the camera returns behind the aim at `cameraYawSmoothSpeed`.
        if mag > 0.01 && !looking {
            let target = wrap_pi(-self.yaw); // UE yaw is the mirror of WoW yaw
            let d = wrap_pi(target - self.cam_yaw);
            let max = FOLLOW_RATE * dt;
            self.cam_yaw = wrap_pi(self.cam_yaw + d.clamp(-max, max));
        }

        // Jump: an edge. In water Space is the swim jump (`0x7c6230`), which
        // clears the latch with a stronger seed at any depth.
        let jump = self.input.jump && !self.jump_was_down;
        self.jump_was_down = self.input.jump;
        if jump {
            if self.swimming {
                self.swimming = false;
                self.vel[2] = SWIM_JUMP_SPEED;
                self.grounded = false;
                self.pos[2] += 0.05;
            } else if self.grounded {
                self.vel[2] = JUMP_SPEED;
                self.grounded = false;
                self.pos[2] += 0.05;
            }
        }

        // Horizontal step with the reference's wall response: a floor more
        // than a step above the feet blocks the axis; the other one slides.
        let step = [self.vel[0] * dt, self.vel[1] * dt];
        if step[0] != 0.0 || step[1] != 0.0 {
            let blocked = |dx: f32, dy: f32| -> bool {
                let (nx, ny) = (self.pos[0] + dx, self.pos[1] + dy);
                if self.wall_hit(nx, ny, self.pos[2]) {
                    return true;
                }
                match self.ground(nx, ny, self.pos[2] + STEP_UP_HEIGHT) {
                    Some((z, nz)) => z > self.pos[2] + STEP_UP_HEIGHT - 1.0e-3 || nz < GROUND_COS,
                    None => false,
                }
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
            // The floating resolver (`0x634640`): no gravity, the vertical only
            // from the stroke. The feet sink to the rest line
            // `surface - 0.75·h` when the surface falls, and a shallow bottom
            // still holds them higher.
            let mut z = self.pos[2] + self.vel[2] * dt;
            if let Some(s) = surface {
                let rest = s - enter;
                let bed = self
                    .ground(self.pos[0], self.pos[1], z + STEP_UP_HEIGHT)
                    .map(|(b, _)| b)
                    .filter(|b| *b > z - 0.05);
                z = match bed {
                    Some(b) if b > rest => b,
                    _ => z.min(rest),
                };
            }
            self.pos[2] = z;
            self.grounded = false;
            self.vel[2] = 0.0;
        } else {
            // Vertical: gravity then the floor probe (benilla's order).
            //
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
            let ceil = self.pos[2].max(prev_z) + STEP_UP_HEIGHT;
            let found = self.ground(self.pos[0], self.pos[1], ceil);
            match found {
                Some((z, _)) => {
                    self.void_time = 0.0;
                    if self.pos[2] <= z || (self.grounded && self.pos[2] - z <= STEP_UP_HEIGHT) {
                        self.pos[2] = z;
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
                            self.pos[0], self.pos[1], self.pos[2], self.tiles.len()
                        );
                    }
                    if self.pos[2] < self.last_ground[2] - VOID_GUARD_DEPTH {
                        eprintln!(
                            "bn_player: void guard restored ({:.1},{:.1},{:.1}) -> ({:.1},{:.1},{:.1})",
                            self.pos[0], self.pos[1], self.pos[2],
                            self.last_ground[0], self.last_ground[1], self.last_ground[2]
                        );
                        self.pos = self.last_ground;
                        self.vel = [0.0; 3];
                        self.grounded = true;
                        self.void_time = 0.0;
                    }
                }
            }
            if self.grounded {
                self.pos[2] = self.pos[2].max(feet[2] - STEP_UP_HEIGHT);
            }
            // Falling-trace: a body that drops through a city floor lands on the
            // terrain, where `ground` still answers - so the no-ground warning
            // never fires. Log the trajectory (with the tile's WMO cell count)
            // every half second instead, so a "fell through here" report has
            // coordinates and tells us whether the cell held any geometry.
            self.fall_time = if self.grounded { 0.0 } else { self.fall_time + dt };
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

        self.attack_left = (self.attack_left - dt).max(0.0);

        // Zoom glide (`camera_zoom.rs`: constant 8.33 yd/s toward the target).
        let step_d = 8.33 * dt;
        self.cam_distance += (self.cam_target_distance - self.cam_distance)
            .clamp(-step_d, step_d);

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
    fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let pivot = self.camera_pivot();
        let fwd = self.camera_fwd();
        let mut dist = self.cam_live.min(self.cam_distance);
        let mut eye = [pivot[0] - fwd[0] * dist, pivot[1] - fwd[1] * dist,
                       pivot[2] - fwd[2] * dist];
        // `cameraWaterCollision`'s ray: the waterline is solid, so a boom that
        // would put the eye under the surface stops at it instead (`0x50e5ec`).
        if let Some(s) = self.surface {
            if eye[2] < s && pivot[2] > s {
                let t = (pivot[2] - s) / (pivot[2] - eye[2]);
                dist *= t.max(0.0);
                eye = [pivot[0] - fwd[0] * dist, pivot[1] - fwd[1] * dist,
                       pivot[2] - fwd[2] * dist];
            }
        }
        (eye, pivot)
    }

    /// Mouse look: `dx`/`dy` are raw pointer counts (`LOOK_SENSITIVITY` rad
    /// each). Called every frame a button is held, so a still drag latches
    /// `look_active`. With
    /// `turn_body` the aim and the rendered body turn with the camera, as
    /// `camera.rs` does on the right button; otherwise only the orbit moves.
    fn look(&mut self, dx: f32, dy: f32, turn_body: bool) {
        // The button is held this frame (the host calls even for a zero
        // delta), so the smart follow yields to the drag.
        self.look_active = true;
        let turn = dx * LOOK_SENSITIVITY;
        if turn_body {
            // `cam_yaw` is UE yaw, the mirror of the WoW aim: the same turn
            // has the opposite sign, so the camera stays locked to the body.
            self.yaw = wrap_pi(self.yaw - turn);
            self.model_yaw = wrap_pi(self.model_yaw - turn);
        }
        self.cam_yaw = wrap_pi(self.cam_yaw + turn);
        self.cam_pitch =
            (self.cam_pitch + dy * LOOK_SENSITIVITY).clamp(-CAM_PITCH_LIMIT, CAM_PITCH_LIMIT);
    }

    fn anim(&self) -> u32 {
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
        if self.attack_left > 0.0 {
            return 16; // AttackUnarmed
        }
        if !self.grounded {
            return if self.vel[2] > 0.0 { 37 } else { 40 }; // JumpStart / Fall
        }
        let hs = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
        if hs < 0.1 {
            // Turning in place plays the shuffle (11 left, 12 right).
            if turning > 0.0 {
                return 11;
            }
            if turning < 0.0 {
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
            anim: self.anim(),
        }
    }
}

/// Parse one ADT into collision triangles (world coords), bucketed by chunk.
fn load_tile(archives: &[benilla_mpq::Archive], map: &str, tx: i32, ty: i32)
    -> Option<TileCol>
{
    let path = format!("World\\Maps\\{map}\\{map}_{tx}_{ty}.adt");
    let bytes = archives.iter().rev().find_map(|a| {
        if a.contains(&path) {
            a.read_file(&path).ok()
        } else {
            None
        }
    })?;
    let mesh = adt_to_tile_mesh(&bytes).ok()?;
    let mut col = TileCol::new();
    col.wmos = mesh.wmos.clone();
    col.doodads = mesh.doodads.clone();
    for c in &mesh.chunks {
        col.liquids.extend(c.liquids.iter().cloned());
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
    Some(col)
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

/// Mouse look: raw pointer delta in pixels. The reference's law is 0.003 rad
/// per unit on both axes (`LOOK_SENSITIVITY`), pitch clamped to ±89°.
/// `turn_body` != 0 is the right button: `camera.rs` turns the character with
/// the camera, so the aim and the rendered body turn too. The left button
/// only orbits, leaving the aim (and WASD's basis) where it was.
#[no_mangle]
pub extern "C" fn bn_player_mouse(
    world: *mut BnWorld,
    dx: f32,
    dy: f32,
    turn_body: u32,
) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.look(dx, dy, turn_body != 0);
    1
}

/// Wheel zoom: `notches` positive = closer (CameraZoomIn 1.0 per notch).
#[no_mangle]
pub extern "C" fn bn_player_zoom(world: *mut BnWorld, notches: f32) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.cam_target_distance = (p.cam_target_distance - notches).clamp(0.0, CAM_DIST_DEFAULT);
    1
}

/// Unarmed attack edge. Ignored in the air or mounted, like the reference.
#[no_mangle]
pub extern "C" fn bn_player_attack(world: *mut BnWorld) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    if !p.grounded || p.mounted {
        return 0;
    }
    p.attack_left = 0.7;
    1
}

/// Mount / dismount (the same flag drives speed and the Mount idle).
#[no_mangle]
pub extern "C" fn bn_player_toggle_mount(world: *mut BnWorld) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.mounted = !p.mounted;
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
    let Some(p) = w.player.as_mut() else { return 0 };
    // Split borrows: the tile load reads the archives, the WMO build the chain
    // and its shared model cache.
    p.ensure_tiles(&w.archives);
    p.ensure_wmo(&mut w.chain, &mut w.wmo_collision);
    p.tick(dt);
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
        assert!(p.wall_hit(-8799.7, -800.0, 0.0), "within the capsule radius");
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
    fn a_hole_in_the_collision_data_never_drops_the_body_out_of_the_world() {
        // No geometry at all under the feet: the body falls, and the void guard
        // restores the last real floor once it is clearly through the map.
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.pos[2] = 10.0;
        p.last_ground = [p.pos[0], p.pos[1], 10.0];
        p.grounded = true;
        let mut restored = false;
        for _ in 0..400 {
            p.tick(0.1);
            assert!(p.pos[2] > -100.0, "never falls out of the world: {}", p.pos[2]);
            if p.grounded && (p.pos[2] - 10.0).abs() < 1.0e-3 {
                restored = true;
            }
        }
        assert!(restored, "the void guard put the body back on the floor");
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

    #[test]
    fn a_doodad_footprint_blocks_the_body() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        let (tx, ty) = Player::tile_of(p.pos[0], p.pos[1]);
        let (cx, cy) = (p.pos[0] + 2.0, p.pos[1]);
        let mut tile = TileCol::new();
        let ci = TileCol::wmo_cell(tx, ty, cx, cy).unwrap();
        let boxed = DoodadCollider {
            c: [cx, cy, 0.0],
            ux: [1.0, 0.0],
            uy: [0.0, 1.0],
            hx: 0.5,
            hy: 0.5,
            top: 3.0,
        };
        tile.doodad_cells[ci].push(boxed);
        // A footprint floating over the head does not block the feet.
        tile.doodad_cells[ci].push(DoodadCollider {
            c: [cx, cy + 4.0, 4.0],
            top: 6.0,
            ..boxed
        });
        p.tiles.insert((tx, ty), tile);
        assert!(p.wall_hit(cx, cy, 0.0), "inside the base cylinder");
        assert!(!p.wall_hit(cx + 2.0, cy, 0.0), "clear of it");
        assert!(!p.wall_hit(cx, cy + 4.0, 0.0), "canopy is above the body");
    }

    #[test]
    fn right_drag_turns_the_aim_with_the_camera() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.look(100.0, 0.0, true);
        let turn = 100.0 * LOOK_SENSITIVITY;
        assert!((p.cam_yaw - turn).abs() < 1.0e-5, "cam_yaw follows the drag");
        assert!((p.yaw + turn).abs() < 1.0e-5, "the aim is the mirror of cam_yaw");
        assert!((p.model_yaw + turn).abs() < 1.0e-5, "the body turns at once");
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
    fn keyboard_turn_carries_the_aim_the_body_and_the_camera() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.grounded = true;
        p.input.turn = 1.0;
        p.tick(0.1);
        let d = TURN_RATE * 0.1;
        assert!((p.yaw - d).abs() < 1.0e-5, "turning left grows the WoW yaw");
        assert!((p.model_yaw - d).abs() < 1.0e-5, "the body turns with it");
        assert!((p.cam_yaw + d).abs() < 1.0e-5, "the camera mirrors the yaw");
        // The smart follow sees the camera already behind.
        assert!(wrap_pi(wrap_pi(-p.yaw) - p.cam_yaw).abs() < 1.0e-5);
    }

    #[test]
    fn turning_in_place_shuffles_and_a_turn_in_water_is_the_swim_idle() {
        let mut p = Player::new("Azeroth", -8800.0, -800.0);
        p.grounded = true;
        p.input.turn = 1.0;
        assert_eq!(p.anim(), 11, "ShuffleLeft");
        p.input.turn = -1.0;
        assert_eq!(p.anim(), 12, "ShuffleRight");
        p.swimming = true;
        assert_eq!(p.anim(), 41, "SwimIdle");
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
        assert!((offset + std::f32::consts::FRAC_PI_2).abs() < 0.05,
                "a pure right strafe turns the body a quarter left in WoW yaw");
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
        assert_eq!(liquid_height(&m, 17066.666 + LIQUID_UNIT, y), None, "dry corner");
    }

    #[test]
    fn deep_water_latches_the_swim_and_freezes_gravity() {
        // 1.6 yd over the feet is past the 0.75·h = 1.521 entry depth.
        let mut p = swimmer(0.0, -1.6);
        p.tick(0.1);
        assert!(p.swimming);
        assert!(!p.grounded);
        assert!((p.pos[2] + 1.6).abs() < 1.0e-4, "an idle swimmer keeps its depth");
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
        assert!((p.pos[2] - rest).abs() < 0.02, "held just under the surface: {}", p.pos[2]);
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
    #[ignore = "needs the installed 1.12.1 data"]
    fn probe_city_fall_spot() {
        use crate::bn_init;
        let dir = std::ffi::CString::new(
            r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data",
        )
        .unwrap();
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
            let built = build_wmo_collision(t, k0, k1, chain, &mut cache);
            println!("baked tile ({k0},{k1}) built={built}");
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
        for gy in (-40..=40).step_by(4) {
            let mut row = String::new();
            for gx in (-40..=40).step_by(4) {
                let z = p.ground(sx + gx as f32, sy + gy as f32, 1000.0);
                row.push_str(&match z {
                    Some((z, _)) => format!("{z:7.1}"),
                    None => "   none".to_string(),
                });
            }
            println!("y{gy:+04}: {row}");
        }
    }

    #[test]
    #[ignore = "needs the installed 1.12.1 data"]
    fn doodad_footprints_on_city_tiles() {
        use crate::{bn_doodad_count, bn_doodad_model_path, bn_init, bn_load_tile};
        let dir = std::ffi::CString::new(
            r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data",
        )
        .unwrap();
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
                    let path = unsafe {
                        std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap()
                    };
                    if seen.insert(path.to_string()) {
                        paths.push(path.to_string());
                    }
                }
            }
        }
        let start = std::time::Instant::now();
        let (mut solid, mut skipped) = (0, 0);
        let (mut rsum, mut rmax) = (0.0f32, 0.0f32);
        let mut buckets = [0usize; 6];
        let mut biggest: Vec<(f32, String)> = Vec::new();
        for path in &paths {
            let chain = unsafe { (*w).chain.as_mut().unwrap() };
            match m2_footprint(chain, path) {
                Some(f) => {
                    let r = f.hx.max(f.hy);
                    solid += 1;
                    rsum += r;
                    rmax = rmax.max(r);
                    let b = if r < 0.3 { 1 } else if r < 0.6 { 2 } else if r < 1.0 { 3 } else if r < 2.0 { 4 } else { 5 };
                    buckets[b] += 1;
                    biggest.push((r, path.clone()));
                }
                _ => skipped += 1,
            }
        }
        biggest.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!(
            "placed {placed}, unique models {}, solid {solid}, skipped {skipped}, \
             avg r {:.2} max r {:.2}, buckets {buckets:?}, parsed in {:?}",
            paths.len(),
            if solid > 0 { rsum / solid as f32 } else { 0.0 },
            rmax,
            start.elapsed()
        );
        for (r, p) in biggest.iter().take(5) {
            println!("  r {r:.2} {p}");
        }
        crate::bn_shutdown(w);
    }
}


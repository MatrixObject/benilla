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
const TURN_RATE: f32 = std::f32::consts::PI;
const GRAVITY: f32 = 19.291_105;
const JUMP_SPEED: f32 = 7.955_547;
const TERMINAL_VELOCITY: f32 = 60.148_003;
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
const CAM_PIVOT_FALLBACK: f32 = 1.8;
const LOOK_SENSITIVITY: f32 = 0.003;
const CAM_DIST_DEFAULT: f32 = 15.0; // camera_zoom.rs's deviation, for a wider view

/// Mount speed bonus in 1.12: +60% (the client's riding speed).
const MOUNT_SPEED_RATIO: f32 = 1.6;

/// The state the host applies (plain C; yaw/pitch radians, positions yards).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BnPlayerState {
    /// Feet position in WoW world yards.
    pub pos: [f32; 3],
    /// Body facing: 0 looks along +X, π/2 along +Y.
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

/// One ADT tile's collision: terrain triangles bucketed by the chunk they
/// came from (16x16 buckets = the chunks), plus the WMO floors once built.
struct TileCol {
    buckets: Vec<Vec<Tri>>,
    /// Terrain triangles by chunk bucket - `buckets.len() == 256`.
    /// WMO triangles bucketed on a [`WMO_GRID`] grid of the tile; a triangle
    /// lands in every cell its XY bounds overlap, so the huge city floors
    /// answer from any point instead of a full-scan every frame.
    wmo_cells: Vec<Vec<Tri>>,
    /// MODF placements, kept from the ADT so the WMO floors can be built once.
    wmos: Vec<benilla_formats::WmoInstance>,
    wmo_ready: bool,
}

impl TileCol {
    fn new() -> Self {
        Self {
            buckets: (0..256).map(|_| Vec::new()).collect(),
            wmo_cells: (0..(WMO_GRID * WMO_GRID) as usize).map(|_| Vec::new()).collect(),
            wmos: Vec::new(),
            wmo_ready: false,
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
    jump: bool,
    walk: bool,
}

pub struct Player {
    map: String,
    /// Feet, WoW world yards.
    pos: [f32; 3],
    /// Facing angle: 0 = +X (north), π/2 = +Y (west).
    yaw: f32,
    vel: [f32; 3],
    grounded: bool,
    input: Input,
    jump_was_down: bool,
    /// Camera yaw in the UE convention (0 = +X, clockwise positive) and pitch
    /// up-positive, radians.
    cam_yaw: f32,
    cam_pitch: f32,
    cam_distance: f32,
    cam_target_distance: f32,
    mounted: bool,
    attack_left: f32,
    tiles: HashMap<(i32, i32), TileCol>,
    /// Tiles already asked for and found missing in the archives.
    missing: Vec<(i32, i32)>,
}

impl Player {
    pub fn new(map: &str, x: f32, y: f32) -> Self {
        Self {
            map: map.to_string(),
            pos: [x, y, 0.0],
            yaw: 0.0,
            vel: [0.0; 3],
            grounded: false,
            input: Input::default(),
            jump_was_down: false,
            // UE yaw of "facing +X" is 0; looking down the +X axis in WoW.
            cam_yaw: 0.0,
            cam_pitch: 0.0,
            cam_distance: CAM_DIST_DEFAULT,
            cam_target_distance: CAM_DIST_DEFAULT,
            mounted: false,
            attack_left: 0.0,
            tiles: HashMap::new(),
            missing: Vec::new(),
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
        if t.wmo_ready {
            return;
        }
        t.wmo_ready = true;
        let Some(chain) = chain.as_mut() else { return };
        for inst in &t.wmos {
            if !cache.contains_key(&inst.model) {
                let Ok(mesh) = benilla_formats::load_wmo_collision_tris(chain, &inst.model)
                else {
                    continue;
                };
                cache.insert(inst.model.clone(), mesh);
            }
            let Some(mesh) = cache.get(&inst.model) else { continue };
            if mesh.is_empty() {
                continue;
            }
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
                let (Some(&va), Some(&vb), Some(&vc)) =
                    (world.get(a), world.get(b), world.get(c))
                else {
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

    /// A steep WMO face within `radius` of `p` (a wall; floors are excluded).
    fn hit_steep(&self, p: [f32; 3], radius: f32) -> bool {
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
            self.hit_steep([x, y, lo + (hi - lo) * (k as f32 / 2.0)], CAPSULE_RADIUS)
        })
    }

    /// One frame: integrate benilla's locomotion, resolve the floor, then place
    /// the camera. `dt` is clamped like the client's frame slice.
    fn tick(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if dt <= 0.0 {
            return;
        }
        let feet = self.pos;
        let forward = self.input.forward.clamp(-1.0, 1.0);
        let strafe = self.input.strafe.clamp(-1.0, 1.0);
        let backpedal = forward < 0.0 && strafe.abs() < 0.01;

        // Camera-relative axis: UE yaw convention's forward is
        // (cos yaw, -sin yaw) in WoW XY (the mirror flips Y).
        let (sy, cy) = self.cam_yaw.sin_cos();
        let fwd = [cy, -sy];
        let right = [sy, cy];
        let mut dir = [fwd[0] * forward + right[0] * strafe,
                       fwd[1] * forward + right[1] * strafe];
        let mag = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
        if mag > 1.0 {
            dir[0] /= mag;
            dir[1] /= mag;
        }
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

        // Facing: the body turns toward the direction it walks (a backpedal
        // keeps the facing and plays WalkBackwards, as the reference does).
        if mag > 0.01 && !backpedal {
            let want = dir[1].atan2(dir[0]);
            let mut d = want - self.yaw;
            while d > std::f32::consts::PI {
                d -= std::f32::consts::TAU;
            }
            while d < -std::f32::consts::PI {
                d += std::f32::consts::TAU;
            }
            let step = TURN_RATE * dt;
            self.yaw += d.clamp(-step, step);
        }

        // Jump: an edge, only from the ground.
        let jump = self.input.jump && !self.jump_was_down;
        self.jump_was_down = self.input.jump;
        if jump && self.grounded {
            self.vel[2] = JUMP_SPEED;
            self.grounded = false;
            self.pos[2] += 0.05;
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

        // Vertical: gravity then the floor probe (benilla's order).
        self.vel[2] = (self.vel[2] - GRAVITY * dt).max(-TERMINAL_VELOCITY);
        self.pos[2] += self.vel[2] * dt;
        let ceil = self.pos[2] + STEP_UP_HEIGHT;
        match self.ground(self.pos[0], self.pos[1], ceil) {
            Some((z, _)) => {
                if self.pos[2] <= z || (self.grounded && self.pos[2] - z <= STEP_UP_HEIGHT) {
                    self.pos[2] = z;
                    self.vel[2] = 0.0;
                    self.grounded = true;
                } else {
                    self.grounded = false;
                }
            }
            None => self.grounded = false,
        }
        if self.grounded {
            self.pos[2] = self.pos[2].max(feet[2] - STEP_UP_HEIGHT);
        }

        self.attack_left = (self.attack_left - dt).max(0.0);

        // Zoom glide (`camera_zoom.rs`: constant 8.33 yd/s toward the target).
        let step_d = 8.33 * dt;
        self.cam_distance += (self.cam_target_distance - self.cam_distance)
            .clamp(-step_d, step_d);
    }

    /// The camera eye/target for this frame: pivot at the feet + head height,
    /// eye on the orbit at the current pitch/yaw, pulled in when the boom
    /// would enter the ground.
    fn camera(&self) -> ([f32; 3], [f32; 3]) {
        let pivot = [self.pos[0], self.pos[1], self.pos[2] + CAM_PIVOT_FALLBACK];
        let (sp, cp) = self.cam_pitch.sin_cos();
        let (sy, cy) = self.cam_yaw.sin_cos();
        // Forward (up-positive pitch) in WoW axes.
        let fwd = [cp * cy, -cp * sy, sp];
        let mut dist = self.cam_distance;
        // Shrink when a sample of the boom is under the floor.
        for i in 1..=8 {
            let f = (i as f32 / 8.0) * self.cam_distance;
            if f <= 0.0 {
                continue;
            }
            let p = [pivot[0] - fwd[0] * f, pivot[1] - fwd[1] * f,
                     pivot[2] - fwd[2] * f];
            if let Some((g, _)) = self.ground(p[0], p[1], p[2]) {
                if p[2] < g + 0.3 {
                    dist = dist.min((i as f32 - 1.0) / 8.0 * self.cam_distance);
                    break;
                }
            }
            // A wall between the pivot and the eye pulls the boom in too.
            if self.hit_steep(p, 0.3) {
                dist = dist.min((i as f32 - 1.0) / 8.0 * self.cam_distance);
                break;
            }
        }
        let eye = [pivot[0] - fwd[0] * dist, pivot[1] - fwd[1] * dist,
                   pivot[2] - fwd[2] * dist];
        (eye, pivot)
    }

    fn anim(&self) -> u32 {
        if self.mounted {
            return 91; // Mount
        }
        if self.attack_left > 0.0 {
            return 16; // AttackUnarmed
        }
        if !self.grounded {
            return if self.vel[2] > 0.0 { 37 } else { 40 }; // JumpStart / Fall
        }
        let hs = (self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1]).sqrt();
        if hs < 0.05 {
            return 0; // Stand
        }
        let back = self.input.forward < 0.0 && self.input.strafe.abs() < 0.01;
        if back {
            13 // WalkBackwards
        } else if hs <= RUN_SPEED * WALK_RATIO * 1.1 {
            4 // Walk
        } else {
            5 // Run
        }
    }

    fn state(&self) -> BnPlayerState {
        let (eye, target) = self.camera();
        BnPlayerState {
            pos: self.pos,
            yaw: self.yaw,
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
    for c in &mesh.chunks {
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
    }
    w.player = Some(p);
    1
}

/// Held movement input: forward/strafe -1..1 (camera-relative), jump and walk
/// as 0/1. The host forwards the raw key state each frame.
#[no_mangle]
pub extern "C" fn bn_player_input(
    world: *mut BnWorld,
    forward: f32,
    strafe: f32,
    jump: u32,
    walk: u32,
) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.input.forward = forward;
    p.input.strafe = strafe;
    p.input.jump = jump != 0;
    p.input.walk = walk != 0;
    1
}

/// Mouse look: raw pointer delta in pixels. The reference's law is 0.003 rad
/// per unit on both axes (`LOOK_SENSITIVITY`), pitch clamped to ±89°.
#[no_mangle]
pub extern "C" fn bn_player_mouse(world: *mut BnWorld, dx: f32, dy: f32) -> u32 {
    let Some(p) = player_mut(world) else { return 0 };
    p.cam_yaw += dx * LOOK_SENSITIVITY;
    p.cam_pitch = (p.cam_pitch + dy * LOOK_SENSITIVITY).clamp(-CAM_PITCH_LIMIT, CAM_PITCH_LIMIT);
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
}


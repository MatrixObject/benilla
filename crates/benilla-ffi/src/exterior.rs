
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use benilla_formats::WmoPortalInfo;
use benilla_formats::WmoPortalRef;

const EXTERIOR: u32 = 0x8;
const ON_PLANE_EPS: f32 = 0.01;
const W_CLAMP_BAND: f32 = 0.001;
const W_CLAMP_SUB: f32 = 1.0e-5;
const RECT_EPS: f32 = 0.001;
const PORTAL_NEAR_PARALLEL: f32 = 1.0e-4;
const PORTAL_PLANE_SNAP: f32 = 0.1;
const NEAREST_TIE_EPS: f32 = 1.0e-4;
const MAX_FLOOR_DROP: f32 = 1760.0;
const DEPTH_CAP: u32 = 10;
const MAX_ITERS: u32 = 1 << 16;
const MIN_WINDOW_NDC: f32 = 0.02;
const CALLBACK_PASS: u32 = 0x10000;
const CAM_FOVY: f32 = std::f32::consts::FRAC_PI_4;

type Rect = [f32; 4];
type Plane = [f32; 4];
type Frusta = [Plane; 4];

struct GroupNav {
    flags: u32,
    bbox_min: [f32; 3],
    bbox_max: [f32; 3],
    ref_start: u16,
    ref_count: u16,
}

pub struct NavModel {
    nav: Vec<GroupNav>,
    pverts: Vec<[f32; 3]>,
    pinfos: Vec<WmoPortalInfo>,
    prefs: Vec<WmoPortalRef>,
    tris: Vec<Vec<[[f32; 3]; 3]>>,
    dtris: Vec<Vec<[[f32; 3]; 3]>>,
    has_portals: bool,
}

pub struct Placement {
    key: String,
    pos: [f32; 3],
    q: [f32; 4],
    aabb: ([f32; 3], [f32; 3]),
}

struct Gate {
    frusta: Vec<Frusta>,
    own_uid: u32,
}

struct ExtState {
    wmo: HashMap<u32, Placement>,
    dd: HashMap<u32, Placement>,
    models: HashMap<String, Option<Arc<NavModel>>>,
    gate: Option<Gate>,
    /// This frame's portal flood per placement: one bit per group, the groups
    /// the walk from the camera's own room reaches. No entry is a model with no
    /// portal graph, which the client never culls.
    pvs: HashMap<u32, Arc<Vec<bool>>>,
}

fn state() -> &'static Mutex<ExtState> {
    static S: OnceLock<Mutex<ExtState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(ExtState {
            wmo: HashMap::new(),
            dd: HashMap::new(),
            models: HashMap::new(),
            gate: None,
            pvs: HashMap::new(),
        })
    })
}

fn q_conj(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

pub fn nav_known(key: &str) -> bool {
    state().lock().is_ok_and(|s| s.models.contains_key(key))
}

pub fn load_nav(chain: &mut benilla_formats::Chain, raw_path: &str) -> Option<Arc<NavModel>> {
    let root_path = raw_path.to_ascii_lowercase();
    let bytes = chain.read_file(&root_path).ok()?;
    let root = benilla_formats::parse_wmo_root(&bytes).ok()?;
    let portals = root.portals();
    let has_portals = !portals.refs.is_empty() && !portals.infos.is_empty();
    let mut m = NavModel {
        nav: root
            .group_infos()
            .iter()
            .map(|g| GroupNav {
                flags: 0,
                bbox_min: g.bbox_min,
                bbox_max: g.bbox_max,
                ref_start: 0,
                ref_count: 0,
            })
            .collect(),
        pverts: portals.vertices.clone(),
        pinfos: portals.infos.clone(),
        prefs: portals.refs.clone(),
        tris: Vec::new(),
        dtris: Vec::new(),
        has_portals,
    };
    if !has_portals {
        return Some(Arc::new(m));
    }
    let n = root.group_count() as usize;
    m.tris = vec![Vec::new(); n];
    m.dtris = vec![Vec::new(); n];
    let stem = root_path.strip_suffix(".wmo").unwrap_or(&root_path);
    for gi in 0..n {
        let Ok(gb) = chain.read_file(&format!("{stem}_{gi:03}.wmo")) else {
            continue;
        };
        if let (Some(h), Some(nav)) = (
            benilla_formats::wmo_group_header(&gb),
            m.nav.get_mut(gi)
        ) {
            nav.flags = h.flags;
            nav.ref_start = h.portal_ref_start;
            nav.ref_count = h.portal_ref_count;
        }
        let mut gp: Vec<[f32; 3]> = Vec::new();
        let mut gx: Vec<u32> = Vec::new();
        benilla_formats::accumulate_wmo_group_collision(&gb, &mut gp, &mut gx);
        if let Some(slot) = m.tris.get_mut(gi) {
            for t in gx.as_chunks::<3>().0 {
                if let (Some(&a), Some(&b), Some(&c)) =
                    (gp.get(t[0] as usize), gp.get(t[1] as usize), gp.get(t[2] as usize))
                {
                    slot.push([a, b, c]);
                }
            }
        }
        let mut dp: Vec<[f32; 3]> = Vec::new();
        let mut dx: Vec<u32> = Vec::new();
        benilla_formats::accumulate_wmo_group_camera_only_collision(&gb, &mut dp, &mut dx);
        if let Some(slot) = m.dtris.get_mut(gi) {
            for t in dx.as_chunks::<3>().0 {
                if let (Some(&a), Some(&b), Some(&c)) =
                    (dp.get(t[0] as usize), dp.get(t[1] as usize), dp.get(t[2] as usize))
                {
                    slot.push([a, b, c]);
                }
            }
        }
    }
    Some(Arc::new(m))
}

pub fn store_nav(key: &str, nav: Option<Arc<NavModel>>) {
    if let Ok(mut s) = state().lock() {
        s.models.insert(key.to_string(), nav);
    }
}

pub fn register_wmo(
    uid: u32,
    key: &str,
    position: [f32; 3],
    rotation_deg: [f32; 3],
    aabb: ([f32; 3], [f32; 3]),
) {
    let q = crate::placement_quat(rotation_deg);
    if let Ok(mut s) = state().lock() {
        s.wmo.insert(
            uid,
            Placement {
                key: key.to_string(),
                pos: position,
                q,
                aabb,
            },
        );
    }
}

pub fn register_doodad(uid: u32, pos: [f32; 3], q: [f32; 4], aabb: ([f32; 3], [f32; 3])) {
    if let Ok(mut s) = state().lock() {
        s.dd.insert(
            uid,
            Placement {
                key: String::new(),
                pos,
                q,
                aabb,
            },
        );
    }
}

struct Seeds {
    in_group: Option<usize>,
    across: Option<usize>,
    floor_z: f32,
}

fn point_in_poly_2d(pts: &[[f32; 2]], p: [f32; 2]) -> bool {
    let mut inside = false;
    let n = pts.len();
    for i in 0..n {
        let cur = pts[i];
        let pr = pts[(i + n - 1) % n];
        if (cur[1] > p[1]) != (pr[1] > p[1])
            && p[0] < (pr[0] - cur[0]) * (p[1] - cur[1]) / (pr[1] - cur[1]) + cur[0]
        {
            inside = !inside;
        }
    }
    inside
}

fn dominant_uv(plane: [f32; 4]) -> (usize, usize) {
    let (ax, ay, az) = (plane[0].abs(), plane[1].abs(), plane[2].abs());
    if ax >= ay && ax >= az {
        (1, 2)
    } else if ay >= az {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn portal_poly<'a>(m: &'a NavModel, info: &WmoPortalInfo) -> Option<&'a [[f32; 3]]> {
    let start = info.start_vertex as usize;
    let verts = m.pverts.get(start..start + info.count as usize)?;
    (verts.len() >= 3).then_some(verts)
}

fn eye_on_portal(m: &NavModel, info: &WmoPortalInfo, eye: [f32; 3]) -> bool {
    let [nx, ny, nz, d] = info.plane;
    if (nx * eye[0] + ny * eye[1] + nz * eye[2] + d).abs() > ON_PLANE_EPS {
        return false;
    }
    let Some(verts) = portal_poly(m, info) else {
        return false;
    };
    let (u, v) = dominant_uv(info.plane);
    point_in_poly_2d(
        &verts.iter().map(|p| [p[u], p[v]]).collect::<Vec<_>>(),
        [eye[u], eye[v]],
    )
}

fn down_ray(m: &NavModel, eye: [f32; 3]) -> Seeds {
    let in_column = |g: &GroupNav| {
        eye[0] >= g.bbox_min[0]
            && eye[0] <= g.bbox_max[0]
            && eye[1] >= g.bbox_min[1]
            && eye[1] <= g.bbox_max[1]
            && g.bbox_min[2] <= eye[2]
    };
    let mut best_z = f32::NEG_INFINITY;
    let mut best: Option<usize> = None;
    for (gi, g) in m.nav.iter().enumerate() {
        if !in_column(g) {
            continue;
        }
        let Some(group_tris) = m.tris.get(gi) else {
            continue;
        };
        for tri in group_tris {
            if let Some(z) = benilla_formats::triangle_z_at(tri, eye[0], eye[1]) {
                if z <= eye[2] && z > best_z {
                    best_z = z;
                    best = Some(gi);
                }
            }
        }
    }
    let mut across: Option<usize> = None;
    for (gi, g) in m.nav.iter().enumerate() {
        if !in_column(g) {
            continue;
        }
        let start = g.ref_start as usize;
        let end = (start + g.ref_count as usize).min(m.prefs.len());
        for r in &m.prefs[start..end] {
            let Some(info) = m.pinfos.get(r.portal as usize) else {
                continue;
            };
            let [nx, ny, nz, d] = info.plane;
            let z = if nz.abs() < PORTAL_NEAR_PARALLEL {
                if (nx * eye[0] + ny * eye[1] + nz * eye[2] + d).abs() > PORTAL_PLANE_SNAP {
                    continue;
                }
                eye[2]
            } else {
                let z = -(nx * eye[0] + ny * eye[1] + d) / nz;
                if z > eye[2] {
                    continue;
                }
                z
            };
            if z < best_z - NEAREST_TIE_EPS {
                continue;
            }
            let Some(verts) = portal_poly(m, info) else {
                continue;
            };
            let (u, v) = dominant_uv(info.plane);
            let pt = [eye[0], eye[1], z];
            if !point_in_poly_2d(
                &verts.iter().map(|p| [p[u], p[v]]).collect::<Vec<_>>(),
                [pt[u], pt[v]],
            ) {
                continue;
            }
            let d_signed = nx * eye[0] + ny * eye[1] + nz * eye[2] + d;
            let chosen = if (d_signed >= 0.0) == (r.side > 0) {
                gi
            } else {
                r.group as usize
            };
            if m.nav.get(chosen).is_none() {
                continue;
            }
            let other = if chosen == gi { r.group as usize } else { gi };
            best_z = z;
            best = Some(chosen);
            across = (m.nav.get(other).is_some() && other != chosen).then_some(other);
        }
    }
    let Some(in_group) = best else {
        let mut fb_z = f32::NEG_INFINITY;
        let mut fb: Option<usize> = None;
        for (gi, g) in m.nav.iter().enumerate() {
            if !in_column(g) {
                continue;
            }
            for tri in m.dtris.get(gi).into_iter().flatten() {
                if let Some(z) = benilla_formats::triangle_z_at(tri, eye[0], eye[1]) {
                    if z <= eye[2] && z > fb_z {
                        fb_z = z;
                        fb = Some(gi);
                    }
                }
            }
        }
        let named = fb
            .filter(|&g| eye[2] - fb_z <= MAX_FLOOR_DROP && m.nav[g].flags & EXTERIOR == 0);
        return Seeds {
            in_group: named,
            across: None,
            floor_z: fb_z,
        };
    };
    if eye[2] - best_z > MAX_FLOOR_DROP
        || m.nav.get(in_group).is_none_or(|g| g.flags & EXTERIOR != 0)
    {
        return Seeds {
            in_group: None,
            across: None,
            floor_z: best_z,
        };
    }
    Seeds {
        in_group: Some(in_group),
        across,
        floor_z: best_z,
    }
}

fn clip_rows(eye: [f32; 3], fwd: [f32; 3], aspect: f32) -> Option<[[f32; 4]; 4]> {
    let fl = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt();
    if fl < 1.0e-6 || aspect <= 0.0 {
        return None;
    }
    let f = [fwd[0] / fl, fwd[1] / fl, fwd[2] / fl];
    let up0 = if f[2].abs() > 0.99 { [0.0, 1.0, 0.0] } else { [0.0, 0.0, 1.0] };
    let mut r = [
        f[1] * up0[2] - f[2] * up0[1],
        f[2] * up0[0] - f[0] * up0[2],
        f[0] * up0[1] - f[1] * up0[0],
    ];
    let rl = (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt();
    if rl < 1.0e-6 {
        return None;
    }
    r = [r[0] / rl, r[1] / rl, r[2] / rl];
    let u = [
        r[1] * f[2] - r[2] * f[1],
        r[2] * f[0] - r[0] * f[2],
        r[0] * f[1] - r[1] * f[0],
    ];
    let half = (CAM_FOVY * 0.5).tan();
    let kx = 1.0 / (aspect.max(1.0e-3) * half);
    let ky = 1.0 / half;
    let row = |a: [f32; 3], k: f32| [k * a[0], k * a[1], k * a[2], -k * (a[0] * eye[0] + a[1] * eye[1] + a[2] * eye[2])];
    Some([
        row(r, kx),
        row(u, ky),
        row(f, 1.0),
        row(f, 1.0),
    ])
}

fn clip_point(rows: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 4] {
    let d = |r: &[f32; 4]| r[0] * p[0] + r[1] * p[1] + r[2] * p[2] + r[3];
    [d(&rows[0]), d(&rows[1]), d(&rows[2]), d(&rows[3])]
}

fn side_planes(rows: &[[f32; 4]; 4]) -> Frusta {
    // The reference's planes are `HalfSpace`s, normalized on construction, so the
    // stored `d` scales with the normal.
    let add = |a: &[f32; 4], b: &[f32; 4], s: f32| {
        let p = [
            a[0] + s * b[0],
            a[1] + s * b[1],
            a[2] + s * b[2],
            a[3] + s * b[3],
        ];
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        if len > 1.0e-12 {
            [p[0] / len, p[1] / len, p[2] / len, p[3] / len]
        } else {
            p
        }
    };
    [
        add(&rows[3], &rows[0], 1.0),
        add(&rows[3], &rows[0], -1.0),
        add(&rows[3], &rows[1], 1.0),
        add(&rows[3], &rows[1], -1.0),
    ]
}

fn window_rows(clip: &[[f32; 4]; 4], rect: Rect) -> [[f32; 4]; 4] {
    let [x0, y0, x1, y1] = rect;
    let (w, h) = (x1 - x0, y1 - y0);
    let mix = |a: &[f32; 4], s: f32, o: f32, b: &[f32; 4]| {
        [
            s * a[0] + o * b[0],
            s * a[1] + o * b[1],
            s * a[2] + o * b[2],
            s * a[3] + o * b[3],
        ]
    };
    [
        mix(&clip[0], 2.0 / w, -(x0 + x1) / w, &clip[3]),
        mix(&clip[1], 2.0 / h, -(y0 + y1) / h, &clip[3]),
        clip[2],
        clip[3],
    ]
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let rect = [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ];
    (rect[2] - rect[0] >= RECT_EPS && rect[3] - rect[1] >= RECT_EPS).then_some(rect)
}

fn portal_screen_rect(
    m: &NavModel,
    info: &WmoPortalInfo,
    clip: &[[f32; 4]; 4],
    pos: &[f32; 3],
    q: &[f32; 4],
) -> Option<Rect> {
    let verts = portal_poly(m, info)?;
    let mut poly: Vec<[f32; 4]> = verts
        .iter()
        .map(|v| {
            let r = crate::q_rot(*q, *v);
            clip_point(clip, [r[0] + pos[0], r[1] + pos[1], r[2] + pos[2]])
        })
        .collect();
    const PLANES: [usize; 4] = [0, 1, 2, 3];
    for side in PLANES {
        let f = |v: &[f32; 4]| match side {
            0 => v[3] + v[0],
            1 => v[3] - v[0],
            2 => v[3] + v[1],
            _ => v[3] - v[1],
        };
        let n = poly.len();
        let mut out: Vec<[f32; 4]> = Vec::with_capacity(n + 2);
        for i in 0..n {
            let a = poly[i];
            let b = poly[(i + 1) % n];
            let (fa, fb) = (f(&a), f(&b));
            if fa >= 0.0 {
                out.push(a);
            }
            if (fa >= 0.0) != (fb >= 0.0) {
                let t = fa / (fa - fb);
                out.push([
                    a[0] + (b[0] - a[0]) * t,
                    a[1] + (b[1] - a[1]) * t,
                    a[2] + (b[2] - a[2]) * t,
                    a[3] + (b[3] - a[3]) * t,
                ]);
            }
        }
        poly = out;
        if poly.len() < 3 {
            return None;
        }
    }
    let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
    let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for c in &poly {
        let w = if c[3].abs() < W_CLAMP_BAND {
            W_CLAMP_SUB
        } else {
            c[3]
        };
        let inv = 1.0 / w;
        min_x = min_x.min(c[0] * inv);
        max_x = max_x.max(c[0] * inv);
        min_y = min_y.min(c[1] * inv);
        max_y = max_y.max(c[1] * inv);
    }
    Some([min_x, min_y, max_x, max_y])
}

/// A group's MOGI box in its placement's rotated frame: the box the reference's
/// `Frustum::intersects_obb` tests, whose radius is `Aabb::relative_radius` over
/// the rotated axes and not a world-aligned bound (a rotated placement's world
/// AABB is larger, so it admits groups the client culls).
struct Obb {
    c: [f32; 3],
    h: [f32; 3],
    axes: [[f32; 3]; 3],
}

fn group_obb(m: &NavModel, pos: &[f32; 3], q: &[f32; 4], g: usize) -> Option<Obb> {
    let n = m.nav.get(g)?;
    let c = crate::q_rot(
        *q,
        [
            (n.bbox_min[0] + n.bbox_max[0]) * 0.5,
            (n.bbox_min[1] + n.bbox_max[1]) * 0.5,
            (n.bbox_min[2] + n.bbox_max[2]) * 0.5,
        ],
    );
    Some(Obb {
        c: [pos[0] + c[0], pos[1] + c[1], pos[2] + c[2]],
        h: [
            (n.bbox_max[0] - n.bbox_min[0]) * 0.5,
            (n.bbox_max[1] - n.bbox_min[1]) * 0.5,
            (n.bbox_max[2] - n.bbox_min[2]) * 0.5,
        ],
        axes: [
            crate::q_rot(*q, [1.0, 0.0, 0.0]),
            crate::q_rot(*q, [0.0, 1.0, 0.0]),
            crate::q_rot(*q, [0.0, 0.0, 1.0]),
        ],
    })
}

/// `Frustum::intersects_obb(aabb, xf, true, true)`: culled when a normalized
/// plane's `n·center + d + radius <= 0`, the box's radius the support term per
/// rotated axis.
fn obb_in_frustum(o: &Obb, frusta: &Frusta) -> bool {
    frusta.iter().all(|p| {
        let dot = |a: &[f32; 3]| p[0] * a[0] + p[1] * a[1] + p[2] * a[2];
        let r = o.h[0] * dot(&o.axes[0]).abs()
            + o.h[1] * dot(&o.axes[1]).abs()
            + o.h[2] * dot(&o.axes[2]).abs();
        p[0] * o.c[0] + p[1] * o.c[1] + p[2] * o.c[2] + p[3] + r > 0.0
    })
}

/// One frame's portal flood over a single placement: the client's `0x6b3b20`,
/// whose visible set is the per-group cull and whose deferred windows are the
/// exterior gate.
struct Flood<'a> {
    m: &'a NavModel,
    eye_local: [f32; 3],
    clip: &'a [[f32; 4]; 4],
    pos: &'a [f32; 3],
    q: &'a [f32; 4],
    visible: Vec<bool>,
    windows: Vec<Rect>,
    pushed: Vec<bool>,
    iters: u32,
}

/// A walk step: the group, the group it was entered from, the screen rect
/// carried down the chain, and the portal-hop depth.
type Step = (usize, usize, Rect, u32);

impl Flood<'_> {
    /// The recursion (`0x6b41c0`): pop a step, light its group, and enter every
    /// portal whose plane faces the eye and whose screen rect survives the
    /// intersection with the carried one. Only Pass 1 records windows.
    fn walk(&mut self, stack: &mut Vec<Step>, record_windows: bool) {
        let full: Rect = [-1.0, -1.0, 1.0, 1.0];
        while let Some((g, came, rect, depth)) = stack.pop() {
            self.iters += 1;
            if self.iters > MAX_ITERS || g >= self.m.nav.len() {
                continue;
            }
            self.visible[g] = true;
            if depth >= DEPTH_CAP {
                continue;
            }
            let nav_g = &self.m.nav[g];
            let start = nav_g.ref_start as usize;
            let end = (start + nav_g.ref_count as usize).min(self.m.prefs.len());
            for r in &self.m.prefs[start..end] {
                let neighbour = r.group as usize;
                if r.group == u16::MAX || neighbour == came {
                    continue;
                }
                let Some(info) = self.m.pinfos.get(r.portal as usize) else {
                    continue;
                };
                let [nx, ny, nz, d] = info.plane;
                let mut dd = nx * self.eye_local[0]
                    + ny * self.eye_local[1]
                    + nz * self.eye_local[2]
                    + d;
                if r.side < 0 {
                    dd = -dd;
                }
                if dd < 0.0 {
                    continue;
                }
                let on_plane = eye_on_portal(self.m, info, self.eye_local);
                let prect = if on_plane {
                    full
                } else {
                    match portal_screen_rect(self.m, info, self.clip, self.pos, self.q) {
                        Some(p) => p,
                        None => continue,
                    }
                };
                let Some(inter) = intersect_rect(rect, prect) else {
                    continue;
                };
                if record_windows
                    && self
                        .m
                        .nav
                        .get(neighbour)
                        .is_some_and(|n| n.flags & EXTERIOR != 0)
                {
                    if let Some(stamp) = self.pushed.get_mut(r.portal as usize) {
                        if !*stamp {
                            *stamp = true;
                            self.windows.push(inter);
                        }
                    }
                }
                stack.push((neighbour, g, inter, depth + 1));
            }
        }
    }
}

/// The flood, run to the client's three passes: the interior walk, the deferred
/// exterior-window replay, and the callback pass.
fn flood_windows(
    m: &NavModel,
    eye_local: [f32; 3],
    clip: &[[f32; 4]; 4],
    pos: &[f32; 3],
    q: &[f32; 4],
    seeds: &Seeds,
) -> (Vec<Rect>, Vec<bool>) {
    let full: Rect = [-1.0, -1.0, 1.0, 1.0];
    let mut flood = Flood {
        m,
        eye_local,
        clip,
        pos,
        q,
        visible: vec![false; m.nav.len()],
        windows: Vec::new(),
        pushed: vec![false; m.pinfos.len()],
        iters: 0,
    };
    // Pass 1 only records windows with a containing interior group (`[0xcde5ac]`);
    // an eye over the building's own `0x8` porch is the driver's outside leg.
    let inside_leg = seeds
        .in_group
        .and_then(|gi| m.nav.get(gi))
        .is_some_and(|n| n.flags & EXTERIOR == 0);
    let mut stack: Vec<Step> = Vec::new();
    match seeds.in_group {
        Some(c) => {
            stack.push((c, usize::MAX, full, 0));
            if let Some(a) = seeds.across {
                stack.push((a, usize::MAX, full, 0));
            }
        }
        None => {
            for (gi, g) in m.nav.iter().enumerate() {
                if g.flags & EXTERIOR != 0 {
                    stack.push((gi, usize::MAX, full, 0));
                }
            }
        }
    }
    flood.walk(&mut stack, inside_leg);

    // Pass 2: each `0x8` group inside a window's sub-frustum is flooded from,
    // carrying that window's rect. The narrowness reject gates the scene walk,
    // not the volume builder, so no `MIN_WINDOW_NDC` test here.
    let worklist = std::mem::take(&mut flood.windows);
    for rect in &worklist {
        let frusta = side_planes(&window_rows(clip, *rect));
        let mut roots: Vec<Step> = Vec::new();
        for (gi, g) in m.nav.iter().enumerate() {
            if g.flags & (EXTERIOR | CALLBACK_PASS) != EXTERIOR {
                continue;
            }
            let admitted = group_obb(m, pos, q, gi).is_some_and(|o| obb_in_frustum(&o, &frusta));
            if admitted {
                roots.push((gi, usize::MAX, *rect, 0));
            }
        }
        flood.walk(&mut roots, false);
    }
    flood.windows = worklist;

    // Pass 3: a callback group draws against the plain camera frustum even from a
    // sealed room.
    let camera = side_planes(clip);
    for (gi, g) in m.nav.iter().enumerate() {
        if flood.visible[gi] || g.flags & CALLBACK_PASS == 0 {
            continue;
        }
        flood.visible[gi] = group_obb(m, pos, q, gi).is_some_and(|o| obb_in_frustum(&o, &camera));
    }
    let Flood {
        visible, windows, ..
    } = flood;
    (windows, visible)
}


fn admits(gate: &Gate, o: &Obb) -> bool {
    gate.frusta.iter().any(|f| obb_in_frustum(o, f))
}

/// [`admits`] for a bound already in world space (the placement-level gate).
fn admits_aabb(gate: &Gate, aabb: &([f32; 3], [f32; 3])) -> bool {
    let c = [
        (aabb.0[0] + aabb.1[0]) * 0.5,
        (aabb.0[1] + aabb.1[1]) * 0.5,
        (aabb.0[2] + aabb.1[2]) * 0.5,
    ];
    let h = [
        (aabb.1[0] - aabb.0[0]) * 0.5,
        (aabb.1[1] - aabb.0[1]) * 0.5,
        (aabb.1[2] - aabb.0[2]) * 0.5,
    ];
    admits(
        gate,
        &Obb {
            c,
            h,
            axes: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        },
    )
}

/// A group the exterior drawer owns: `0x8` unlit exterior, `0x40` exterior-lit.
const GROUP_EXTERIOR: u32 = 0x148;

/// This frame's verdict for one batch of one placement, the client's per-part
/// chain: the portal flood's bit, the group's own far clip, and for an exterior
/// group the window gate (the camera's own building exempt).
fn group_ok(
    s: &ExtState,
    uid: u32,
    group: usize,
    eye: [f32; 3],
    fwd: [f32; 3],
    farclip: f32,
) -> bool {
    let Some(pl) = s.wmo.get(&uid) else {
        return true;
    };
    let Some(Some(m)) = s.models.get(&pl.key) else {
        return true;
    };
    let Some(o) = group_obb(m, &pl.pos, &pl.q, group) else {
        return true;
    };
    if s.pvs.get(&uid).is_some_and(|v| !v.get(group).copied().unwrap_or(true)) {
        return false;
    }
    let fl = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt();
    if fl > 1.0e-6 {
        let f = [fwd[0] / fl, fwd[1] / fl, fwd[2] / fl];
        let c = o.c;
        let h = o.h;
        let rad = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
        let depth =
            (c[0] - eye[0]) * f[0] + (c[1] - eye[1]) * f[1] + (c[2] - eye[2]) * f[2] - rad;
        if depth > farclip.clamp(177.0, 777.0) {
            return false;
        }
    }
    if let Some(g) = &s.gate {
        let exterior = m
            .nav
            .get(group)
            .is_some_and(|n| n.flags & GROUP_EXTERIOR != 0);
        if exterior && g.own_uid != uid && !admits(g, &o) {
            return false;
        }
    }
    true
}

#[no_mangle]
pub extern "C" fn bn_exterior_group_ok(
    uid: u32,
    group: u32,
    eye: *const f32,
    fwd: *const f32,
    farclip: f32,
    out: *mut u32,
) -> u32 {
    if eye.is_null() || fwd.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller passes two live float triples and one out word.
    let (e, f) = unsafe {
        (
            [*eye, *eye.add(1), *eye.add(2)],
            [*fwd, *fwd.add(1), *fwd.add(2)],
        )
    };
    if let Ok(s) = state().lock() {
        unsafe { *out = group_ok(&s, uid, group as usize, e, f, farclip) as u32 };
        return 1;
    }
    unsafe { *out = 1 };
    0
}

#[no_mangle]
pub extern "C" fn bn_exterior_update(
    eye: *const f32,
    fwd: *const f32,
    aspect: f32,
) -> u32 {
    if eye.is_null() || fwd.is_null() {
        return 0;
    }
    // SAFETY: caller passes two live float triples.
    let (e0, f0) = unsafe {
        (
            core::ptr::read_unaligned(eye as *const [f32; 3]),
            core::ptr::read_unaligned(fwd as *const [f32; 3]),
        )
    };
    let Some(clip) = clip_rows(e0, f0, aspect) else {
        return 0;
    };
    let mut ok = 0u32;
    if let Ok(mut s) = state().lock() {
        let mut best: Option<(f32, u32)> = None;
        let mut claim_windows: Option<Vec<Rect>> = None;
        let mut fresh: HashMap<u32, Arc<Vec<bool>>> = HashMap::new();
        for (uid, pl) in s.wmo.iter() {
            let Some(Some(m)) = s.models.get(&pl.key) else {
                continue;
            };
            if !m.has_portals {
                continue;
            }
            let rel = [e0[0] - pl.pos[0], e0[1] - pl.pos[1], e0[2] - pl.pos[2]];
            let eye_local = crate::q_rot(q_conj(pl.q), rel);
            let seeds = down_ray(m, eye_local);
            let (windows, visible) = flood_windows(m, eye_local, &clip, &pl.pos, &pl.q, &seeds);
            fresh.insert(*uid, Arc::new(visible));
            let claims = seeds
                .in_group
                .and_then(|g| m.nav.get(g))
                .is_some_and(|n| n.flags & EXTERIOR == 0);
            if claims && best.is_none_or(|(z, _)| seeds.floor_z > z) {
                best = Some((seeds.floor_z, *uid));
                claim_windows = Some(windows);
            }
        }
        s.pvs = fresh;
        let new_gate = best.zip(claim_windows).map(|((_, uid), windows)| {
            let frusta = windows
                .iter()
                .filter(|r| r[2] - r[0] >= MIN_WINDOW_NDC && r[3] - r[1] >= MIN_WINDOW_NDC)
                .map(|r| side_planes(&window_rows(&clip, *r)))
                .collect();
            Gate {
                frusta,
                own_uid: uid,
            }
        });
        s.gate = new_gate;
        ok = 1;
    }
    ok
}

fn verdict(placements: &HashMap<u32, Placement>, gate: &Option<Gate>, uid: u32) -> u32 {
    let Some(g) = gate else {
        return 1;
    };
    if g.own_uid == uid {
        return 1;
    }
    placements
        .get(&uid)
        .map_or(1, |p| admits_aabb(g, &p.aabb) as u32)
}

#[no_mangle]
pub extern "C" fn bn_exterior_wmo_ok(uid: u32, out: *mut u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    if let Ok(s) = state().lock() {
        unsafe { *out = verdict(&s.wmo, &s.gate, uid) };
        return 1;
    }
    unsafe { *out = 1 };
    0
}

#[no_mangle]
pub extern "C" fn bn_exterior_doodad_ok(uid: u32, out: *mut u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    if let Ok(s) = state().lock() {
        unsafe { *out = verdict(&s.dd, &s.gate, uid) };
        return 1;
    }
    unsafe { *out = 1 };
    0
}

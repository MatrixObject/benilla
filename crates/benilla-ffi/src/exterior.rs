//! The cull's UE-facing half: the same `benilla_cull` the renderer runs, behind the C ABI, where
//! the host asks once per placement (this frame's exterior window gate, and the placement verdicts)
//! and once per group (the portal bit, the group's far clip, the gate).
//!
//! Nothing here re-implements the reference: the seed down-ray, the portal flood, the deferred
//! windows and the frustum tests are `benilla-cull`'s. This module converts the camera the host
//! hands over (WoW yards) into the placement space the flood works in, and keeps the host's
//! per-uid tables.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use benilla_cull::coords::{bevy_to_wow, wow_to_bevy};
use benilla_cull::{
    collision_tri_grids, compute_pvs_traced, group_aabb, scene_window_frustum, Aabb, ColumnGrid,
    CullModel, DownRaySeeds, FloodTrace, Frustum, Rect, WmoGroupNav, EXTERIOR,
};
use benilla_formats::{WmoPortalInfo, WmoPortalRef};
use glam::{Affine3A, Mat3, Mat4, Quat, Vec3, Vec3A};

/// The cone the screen is drawn in: benilla-world's `view::CAM_FOVY` (45° vertical, the reference's
/// 44.1° at 16:9), which the host's UE camera is set from. The window rects are NDC extents in this
/// cone, so another projection would gate the wrong part of the screen.
const CAM_FOVY: f32 = std::f32::consts::FRAC_PI_4;
const CAM_NEAR: f32 = 0.1;

/// A group the exterior drawer owns: `0x8` unlit exterior, `0x40` exterior-lit.
const GROUP_EXTERIOR: u32 = 0x148;

/// A placement's rotation into render space: [`wow_to_bevy`] is the rotation `(−y, z, −x)`, so a
/// WoW-space rotation `R` becomes `M·R·Mᵀ` and its translation `M·pos`.
fn wow_to_bevy_mat3() -> Mat3 {
    Mat3::from_cols(
        Vec3::new(0.0, 0.0, -1.0),
        Vec3::new(-1.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
    )
}

/// A placement's `world_from_local`, from the WoW-space quaternion and origin the host registers.
fn world_from_local(pos: [f32; 3], q: [f32; 4]) -> Affine3A {
    let m = wow_to_bevy_mat3();
    let rot = Mat3::from_quat(Quat::from_xyzw(q[0], q[1], q[2], q[3]));
    Affine3A::from_mat3_translation(m * rot * m.transpose(), wow_to_bevy(pos))
}

/// The camera's clip matrix, built as `benilla-world` builds it from its camera: a right-handed view
/// down `fwd` and Bevy's reverse-infinite perspective. `eye` and `fwd` are in WoW yards.
fn clip_from_world(eye: [f32; 3], fwd: [f32; 3], aspect: f32) -> Option<Mat4> {
    let eye = wow_to_bevy(eye);
    let fwd = wow_to_bevy(fwd);
    if !aspect.is_finite() || aspect <= 0.0 || fwd.length_squared() < 1.0e-12 {
        return None;
    }
    let view = Mat4::look_at_rh(eye, eye + fwd.normalize(), Vec3::Y);
    Some(Mat4::perspective_infinite_reverse_rh(CAM_FOVY, aspect, CAM_NEAR) * view)
}

/// The terrain surface under the eye (raw WoW `z`) as a placement's model-space `z`: the frame the
/// down-ray races in. Exact for a yaw-only placement, where a vertical column stays vertical.
fn terrain_local(local_from_world: &Affine3A, eye_bwy: Vec3, terrain_wow_z: f32) -> f32 {
    let under = Vec3::new(eye_bwy.x, terrain_wow_z, eye_bwy.z);
    bevy_to_wow(local_from_world.transform_point3(under))[2]
}

/// One WMO's cull data, owned: [`NavModel::cull`] lends it to `benilla_cull` unchanged.
pub struct NavModel {
    nav: Vec<WmoGroupNav>,
    pverts: Vec<[f32; 3]>,
    pinfos: Vec<WmoPortalInfo>,
    prefs: Vec<WmoPortalRef>,
    tris: Vec<Vec<[[f32; 3]; 3]>>,
    grids: Vec<Option<ColumnGrid>>,
    dtris: Vec<Vec<[[f32; 3]; 3]>>,
    /// Per MODD index, the groups whose MODR names it: the client creates one doodad per (index,
    /// group) (`0x695aa0`) and draws a group's copy with the group.
    doodad_groups: Vec<Vec<u16>>,
    has_portals: bool,
}

impl NavModel {
    fn cull(&self) -> CullModel<'_> {
        CullModel {
            nav: &self.nav,
            portal_vertices: &self.pverts,
            portal_infos: &self.pinfos,
            portal_refs: &self.prefs,
            collision_tris: &self.tris,
            collision_grids: &self.grids,
            camera_only_tris: &self.dtris,
        }
    }
}

pub struct Placement {
    key: String,
    world_from_local: Affine3A,
    /// The placement's world box, WoW yards, min and max.
    aabb: ([f32; 3], [f32; 3]),
    /// A WMO-internal doodad's owning placement and MODD index; `None` is an ADT placement, which
    /// the exterior-window gate culls.
    owner: Option<(u32, u32)>,
}

impl Placement {
    /// The placement's world box in render space, the bound the frustum tests take.
    fn aabb(&self) -> Aabb {
        let (a, b) = (wow_to_bevy(self.aabb.0), wow_to_bevy(self.aabb.1));
        Aabb::from_min_max(Vec3A::from(a.min(b)), Vec3A::from(a.max(b)))
    }
}

struct Gate {
    frusta: Vec<Frustum>,
    own_uid: u32,
}

#[derive(Default)]
struct ExtState {
    wmo: BTreeMap<u32, Placement>,
    dd: BTreeMap<u32, Placement>,
    models: HashMap<String, Option<Arc<NavModel>>>,
    gate: Option<Gate>,
    /// This frame's portal flood per placement: one bit per group, the groups the walk from the
    /// camera's own room reaches. No entry is a model with no portal graph, which is never culled.
    pvs: HashMap<u32, Arc<Vec<bool>>>,
    /// The camera the last update ran for. Every tile actor asks the same question each frame, so
    /// the answer is computed once and reused until the camera, the aspect or the terrain moves.
    last_camera: Option<([f32; 3], [f32; 3], f32, f32)>,
}

fn state() -> &'static Mutex<ExtState> {
    static S: OnceLock<Mutex<ExtState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(ExtState::default()))
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
            .map(|g| WmoGroupNav {
                flags: 0,
                bbox_min: g.bbox_min,
                bbox_max: g.bbox_max,
                ref_start: 0,
                ref_count: 0,
                area_table_id: 0,
                fog_indices: [0; 4],
                group_liquid: benilla_formats::NO_GROUP_LIQUID,
            })
            .collect(),
        pverts: portals.vertices.clone(),
        pinfos: portals.infos.clone(),
        prefs: portals.refs.clone(),
        tris: Vec::new(),
        grids: Vec::new(),
        dtris: Vec::new(),
        doodad_groups: vec![Vec::new(); root.doodads().len()],
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
        if let (Some(h), Some(nav)) = (benilla_formats::wmo_group_header(&gb), m.nav.get_mut(gi)) {
            nav.flags = h.flags;
            nav.ref_start = h.portal_ref_start;
            nav.ref_count = h.portal_ref_count;
            nav.area_table_id = h.area_table_id;
            nav.fog_indices = h.fog_indices;
            nav.group_liquid = h.group_liquid;
        }
        for di in benilla_formats::wmo_group_doodad_refs(&gb) {
            if let Some(slot) = m.doodad_groups.get_mut(di as usize) {
                slot.push(gi as u16);
            }
        }
        let mut gp: Vec<[f32; 3]> = Vec::new();
        let mut gx: Vec<u32> = Vec::new();
        benilla_formats::accumulate_wmo_group_collision(&gb, &mut gp, &mut gx);
        if let Some(slot) = m.tris.get_mut(gi) {
            for t in gx.as_chunks::<3>().0 {
                if let (Some(&a), Some(&b), Some(&c)) = (
                    gp.get(t[0] as usize),
                    gp.get(t[1] as usize),
                    gp.get(t[2] as usize),
                ) {
                    slot.push([a, b, c]);
                }
            }
        }
        let mut dp: Vec<[f32; 3]> = Vec::new();
        let mut dx: Vec<u32> = Vec::new();
        benilla_formats::accumulate_wmo_group_camera_only_collision(&gb, &mut dp, &mut dx);
        if let Some(slot) = m.dtris.get_mut(gi) {
            for t in dx.as_chunks::<3>().0 {
                if let (Some(&a), Some(&b), Some(&c)) = (
                    dp.get(t[0] as usize),
                    dp.get(t[1] as usize),
                    dp.get(t[2] as usize),
                ) {
                    slot.push([a, b, c]);
                }
            }
        }
    }
    m.grids = collision_tri_grids(&m.tris);
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
                world_from_local: world_from_local(position, q),
                aabb,
                owner: None,
            },
        );
        s.last_camera = None;
    }
}

pub fn register_doodad(
    uid: u32,
    pos: [f32; 3],
    q: [f32; 4],
    aabb: ([f32; 3], [f32; 3]),
    owner: Option<(u32, u32)>,
) {
    if let Ok(mut s) = state().lock() {
        s.dd.insert(
            uid,
            Placement {
                key: String::new(),
                world_from_local: world_from_local(pos, q),
                aabb,
                owner,
            },
        );
        s.last_camera = None;
    }
}

/// The seed tap: the camera's own group, for the claim. The flood itself is `benilla_cull`'s.
#[derive(Default)]
struct SeedTap {
    in_group: Option<usize>,
}

impl FloodTrace for SeedTap {
    fn seed(&mut self, seeds: DownRaySeeds) {
        self.in_group = seeds.in_group;
    }
}

fn admits_group(gate: &Gate, pl: &Placement, nav: &WmoGroupNav) -> bool {
    let aabb = group_aabb(nav);
    gate.frusta
        .iter()
        .any(|f| f.intersects_obb(&aabb, &pl.world_from_local, true, true))
}

/// [`admits_group`] for a placement's own box, in world axes.
fn admits_aabb(gate: &Gate, aabb: &Aabb) -> bool {
    gate.frusta
        .iter()
        .any(|f| f.intersects_obb(aabb, &Affine3A::IDENTITY, true, true))
}

/// This frame's verdict for one batch of one placement, the client's per-part chain: the portal
/// flood's bit, the group's own far clip, and for an exterior group the window gate (the camera's
/// own building exempt).
fn group_ok(s: &ExtState, uid: u32, group: usize, eye: Vec3, fwd: Vec3, farclip: f32) -> bool {
    let Some(pl) = s.wmo.get(&uid) else {
        return true;
    };
    let Some(Some(m)) = s.models.get(&pl.key) else {
        return true;
    };
    let Some(nav) = m.nav.get(group) else {
        return true;
    };
    if s.pvs
        .get(&uid)
        .is_some_and(|v| !v.get(group).copied().unwrap_or(true))
    {
        return false;
    }
    if fwd.length_squared() > 1.0e-12 {
        let aabb = group_aabb(nav);
        let c = pl.world_from_local.transform_point3(aabb.center.into());
        let depth = (c - eye).dot(fwd.normalize()) - aabb.half_extents.length();
        if depth > farclip.clamp(177.0, 777.0) {
            return false;
        }
    }
    if let Some(g) = &s.gate {
        if nav.flags & GROUP_EXTERIOR != 0 && g.own_uid != uid && !admits_group(g, pl, nav) {
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
            core::ptr::read_unaligned(eye as *const [f32; 3]),
            core::ptr::read_unaligned(fwd as *const [f32; 3]),
        )
    };
    if let Ok(s) = state().lock() {
        // SAFETY: `out` is checked non-null above.
        unsafe {
            *out = group_ok(
                &s,
                uid,
                group as usize,
                wow_to_bevy(e),
                wow_to_bevy(f),
                farclip,
            ) as u32
        };
        return 1;
    }
    // SAFETY: `out` is checked non-null above.
    unsafe { *out = 1 };
    0
}

/// Recompute this frame's portal floods and the exterior window gate for the camera. `terrain_z`
/// is the terrain height under the eye in WoW yards (NaN where there is none), the seed's race leg.
#[no_mangle]
pub extern "C" fn bn_exterior_update(
    eye: *const f32,
    fwd: *const f32,
    aspect: f32,
    terrain_z: f32,
) -> u32 {
    if eye.is_null() || fwd.is_null() {
        return 0;
    }
    // SAFETY: caller passes two live float triples.
    let (e, f) = unsafe {
        (
            core::ptr::read_unaligned(eye as *const [f32; 3]),
            core::ptr::read_unaligned(fwd as *const [f32; 3]),
        )
    };
    let Some(clip) = clip_from_world(e, f, aspect) else {
        return 0;
    };
    let eye_bwy = wow_to_bevy(e);
    let Ok(mut s) = state().lock() else {
        return 0;
    };
    let camera = (e, f, aspect, terrain_z);
    if s.last_camera.is_some_and(|last| {
        last.0 == camera.0 && last.1 == camera.1 && last.2 == camera.2 && last.3 == camera.3
    }) {
        return 1;
    }
    s.last_camera = Some(camera);
    let mut fresh: HashMap<u32, Arc<Vec<bool>>> = HashMap::new();
    // The claim and its windows: the first placement whose seed lands in a non-exterior group, the
    // reference's `[0xc7b748]` writer. The worklist is that flood's own (`0x681070` copies it).
    let mut claim: Option<(u32, Vec<Rect>)> = None;
    for (&uid, pl) in &s.wmo {
        let Some(Some(m)) = s.models.get(&pl.key) else {
            continue;
        };
        if !m.has_portals {
            continue;
        }
        let local_from_world = pl.world_from_local.inverse();
        let eye_local = bevy_to_wow(local_from_world.transform_point3(eye_bwy));
        let terrain =
            (!terrain_z.is_nan()).then(|| terrain_local(&local_from_world, eye_bwy, terrain_z));
        let mut tap = SeedTap::default();
        let pvs = compute_pvs_traced(
            &m.cull(),
            eye_local,
            terrain,
            &clip,
            &pl.world_from_local,
            &mut tap,
        );
        let claims = tap
            .in_group
            .and_then(|g| m.nav.get(g))
            .is_some_and(|n| n.flags & EXTERIOR == 0);
        if claim.is_none() && claims {
            claim = Some((uid, pvs.windows.clone()));
        }
        fresh.insert(uid, Arc::new(pvs.visible));
    }
    s.pvs = fresh;
    s.gate = claim.map(|(uid, windows)| Gate {
        frusta: windows
            .iter()
            .filter_map(|r| scene_window_frustum(*r, &clip))
            .collect(),
        own_uid: uid,
    });
    1
}

fn wmo_verdict(s: &ExtState, uid: u32) -> u32 {
    let Some(g) = &s.gate else {
        return 1;
    };
    if g.own_uid == uid {
        return 1;
    }
    s.wmo
        .get(&uid)
        .map_or(1, |p| admits_aabb(g, &p.aabb()) as u32)
}

/// One naming group's chain for a WMO-internal prop: the portal flood's bit, and the window gate on
/// an exterior group of another building — the same terms `group_ok` applies to the group's own
/// geometry.
fn doodad_group_ok(s: &ExtState, owner_uid: u32, group: usize) -> bool {
    let Some(pl) = s.wmo.get(&owner_uid) else {
        return true;
    };
    let Some(Some(m)) = s.models.get(&pl.key) else {
        return true;
    };
    if s.pvs
        .get(&owner_uid)
        .is_some_and(|v| !v.get(group).copied().unwrap_or(true))
    {
        return false;
    }
    if let Some(g) = &s.gate {
        if let Some(nav) = m.nav.get(group) {
            if nav.flags & GROUP_EXTERIOR != 0
                && g.own_uid != owner_uid
                && !admits_group(g, pl, nav)
            {
                return false;
            }
        }
    }
    true
}

/// The client creates one doodad per (MODD index, MODR group) and draws it with that group
/// (`0x695aa0`), so a WMO-internal prop draws iff any naming group is in this frame's portal flood:
/// from outside only through a doorway the walk reaches, from inside the camera's own room
/// regardless of where it looks. An ADT placement has no room and keeps the exterior-window gate.
fn doodad_verdict(s: &ExtState, uid: u32) -> u32 {
    let Some(p) = s.dd.get(&uid) else {
        return 1;
    };
    let Some((owner_uid, di)) = p.owner else {
        return s
            .gate
            .as_ref()
            .map_or(1, |g| admits_aabb(g, &p.aabb()) as u32);
    };
    let Some(Some(m)) = s.wmo.get(&owner_uid).and_then(|w| s.models.get(&w.key)) else {
        return 1;
    };
    if !m.has_portals {
        return 1;
    }
    let Some(groups) = m.doodad_groups.get(di as usize) else {
        return 1;
    };
    u32::from(
        groups
            .iter()
            .any(|&g| doodad_group_ok(s, owner_uid, g as usize)),
    )
}

#[no_mangle]
pub extern "C" fn bn_exterior_wmo_ok(uid: u32, out: *mut u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    if let Ok(s) = state().lock() {
        // SAFETY: `out` is checked non-null above.
        unsafe { *out = wmo_verdict(&s, uid) };
        return 1;
    }
    // SAFETY: `out` is checked non-null above.
    unsafe { *out = 1 };
    0
}

#[no_mangle]
pub extern "C" fn bn_exterior_doodad_ok(uid: u32, out: *mut u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    if let Ok(s) = state().lock() {
        // SAFETY: `out` is checked non-null above.
        unsafe { *out = doodad_verdict(&s, uid) };
        return 1;
    }
    // SAFETY: `out` is checked non-null above.
    unsafe { *out = 1 };
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(flags: &[u32], doodad_groups: Vec<Vec<u16>>) -> Option<Arc<NavModel>> {
        Some(Arc::new(NavModel {
            nav: flags
                .iter()
                .map(|&flags| WmoGroupNav {
                    flags,
                    bbox_min: [0.0; 3],
                    bbox_max: [10.0; 3],
                    ref_start: 0,
                    ref_count: 0,
                    area_table_id: 0,
                    fog_indices: [0; 4],
                    group_liquid: 0xf,
                })
                .collect(),
            pverts: Vec::new(),
            pinfos: Vec::new(),
            prefs: Vec::new(),
            tris: vec![Vec::new(); flags.len()],
            grids: vec![None; flags.len()],
            dtris: vec![Vec::new(); flags.len()],
            doodad_groups,
            has_portals: true,
        }))
    }

    fn placement(owner: Option<(u32, u32)>, aabb: ([f32; 3], [f32; 3])) -> Placement {
        Placement {
            key: String::new(),
            world_from_local: world_from_local([0.0; 3], [0.0, 0.0, 0.0, 1.0]),
            aabb,
            owner,
        }
    }

    /// A camera far off, looking away: its cone holds nothing the tests place near the origin.
    fn sealed_gate(own_uid: u32) -> Gate {
        let clip = Mat4::perspective_infinite_reverse_rh(CAM_FOVY, 1.0, CAM_NEAR)
            * Mat4::look_at_rh(
                Vec3::new(1000.0, 0.0, 0.0),
                Vec3::new(2000.0, 0.0, 0.0),
                Vec3::Y,
            );
        Gate {
            frusta: vec![Frustum::from_clip_from_world(&clip)],
            own_uid,
        }
    }

    /// A prop draws with the MODR groups that name it and nothing else: the camera's own room lit
    /// keeps it up wherever the camera turns, a dark room takes it down, and an ADT placement still
    /// answers the window gate.
    #[test]
    fn a_wmo_prop_follows_its_rooms_portal_bit() {
        let mut s = ExtState::default();
        // Rooms 0 and 1, then the exterior porch group 2.
        s.models.insert(
            "inn".into(),
            nav(&[0, 0, EXTERIOR], vec![vec![0], vec![1], vec![2]]),
        );
        s.models
            .insert("twin".into(), nav(&[0, 0], vec![vec![0, 1]]));
        s.wmo.insert(
            7,
            Placement {
                key: "inn".into(),
                ..placement(None, ([0.0; 3], [10.0; 3]))
            },
        );
        let unit = ([0.0, 0.0, 1.0], [2.0, 2.0, 3.0]);
        s.dd.insert(100, placement(Some((7, 0)), unit)); // a room-0 prop
        s.dd.insert(101, placement(Some((7, 1)), unit)); // a room-1 prop
        s.dd.insert(102, placement(Some((9, 0)), unit)); // owner never registered
        s.dd.insert(103, placement(None, unit)); // an MDDF placement

        // Inside room 0: its prop and the lit porch stay; the dark room 1's
        // prop goes; an unknown owner or uid fails open.
        s.pvs.insert(7, Arc::new(vec![true, false, true]));
        assert_eq!(doodad_verdict(&s, 100), 1);
        assert_eq!(doodad_verdict(&s, 101), 0);
        assert_eq!(doodad_verdict(&s, 102), 1);
        assert_eq!(doodad_verdict(&s, 103), 1);
        assert_eq!(doodad_verdict(&s, 4242), 1);

        // A prop named by two groups draws while either is lit, and goes when
        // both are dark — the reference's any-naming-group rule.
        s.wmo.insert(
            8,
            Placement {
                key: "twin".into(),
                ..placement(None, ([0.0; 3], [10.0; 3]))
            },
        );
        s.dd.insert(104, placement(Some((8, 0)), unit));
        s.pvs.insert(8, Arc::new(vec![false, true]));
        assert_eq!(doodad_verdict(&s, 104), 1);
        s.pvs.insert(8, Arc::new(vec![false, false]));
        assert_eq!(doodad_verdict(&s, 104), 0);

        // Outside the building the flood lights only the exterior groups: the
        // room's props cull even with no window gate at all.
        s.pvs.insert(7, Arc::new(vec![false, false, true]));
        assert_eq!(doodad_verdict(&s, 100), 0);
        assert_eq!(doodad_verdict(&s, 101), 0);
        assert_eq!(doodad_verdict(&s, 102), 1, "unknown owner fails open");

        // A model with no portal graph is never culled by the client.
        s.models.insert("shed".into(), None);
        s.wmo.insert(
            9,
            Placement {
                key: "shed".into(),
                ..placement(None, ([0.0; 3], [10.0; 3]))
            },
        );
        s.dd.insert(105, placement(Some((9, 0)), unit));
        assert_eq!(doodad_verdict(&s, 105), 1);

        // Inside another building: an ADT prop outside every window culls, and
        // so does a prop of another building's exterior group; the camera's own
        // building and interior groups never consult the window gate.
        s.gate = Some(sealed_gate(7));
        assert_eq!(doodad_verdict(&s, 103), 0);
        assert_eq!(
            doodad_verdict(&s, 100),
            0,
            "pvs still decides its own rooms"
        );
        s.pvs.insert(7, Arc::new(vec![true, false, true]));
        assert_eq!(doodad_verdict(&s, 100), 1, "own room, gate ignores it");
        s.dd.insert(106, placement(Some((7, 2)), unit)); // the own porch
        assert_eq!(doodad_verdict(&s, 106), 1, "own building is exempt");
        s.dd.insert(107, placement(Some((8, 0)), unit)); // another building
        s.pvs.insert(8, Arc::new(vec![true, true]));
        assert_eq!(
            doodad_verdict(&s, 107),
            1,
            "an interior group of another building: no window test"
        );
        s.models
            .insert("hall".into(), nav(&[EXTERIOR], vec![vec![0]]));
        s.wmo.insert(
            10,
            Placement {
                key: "hall".into(),
                ..placement(None, ([0.0; 3], [10.0; 3]))
            },
        );
        s.pvs.insert(10, Arc::new(vec![true]));
        s.dd.insert(109, placement(Some((10, 0)), unit));
        assert_eq!(
            doodad_verdict(&s, 109),
            0,
            "exterior group follows the gate"
        );
    }

    /// The window gate is a cone of the claimed building's doorways: a group inside it stays, a
    /// group outside it goes.
    #[test]
    fn the_window_gate_admits_only_through_the_claiming_buildings_windows() {
        // A camera at the origin looking down -Z (render space) with a window around the centre.
        let clip = Mat4::perspective_infinite_reverse_rh(CAM_FOVY, 16.0 / 9.0, CAM_NEAR)
            * Mat4::look_at_rh(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let window = [-0.2, -0.2, 0.2, 0.2];
        let gate = Gate {
            frusta: vec![scene_window_frustum(window, &clip).expect("a 0.4 NDC window passes")],
            own_uid: 1,
        };
        let model = nav(&[EXTERIOR], Vec::new()).expect("nav");
        let nav_g = &model.nav[0];
        let ahead = Placement {
            key: String::new(),
            world_from_local: Affine3A::from_translation(Vec3::new(0.0, 0.0, -20.0)),
            aabb: ([0.0; 3], [0.0; 3]),
            owner: None,
        };
        let behind = Placement {
            world_from_local: Affine3A::from_translation(Vec3::new(0.0, 0.0, 20.0)),
            ..placement(None, ([0.0; 3], [0.0; 3]))
        };
        assert!(admits_group(&gate, &ahead, nav_g), "a group in the doorway");
        assert!(
            !admits_group(&gate, &behind, nav_g),
            "a group behind the camera"
        );
        // The gate drops a window sliver: `scene_window_frustum` is the open-world walk's width
        // reject, the one the reference applies to the exterior gate too.
        assert!(
            scene_window_frustum([-0.005, -0.2, 0.005, 0.2], &clip).is_none(),
            "a sliver window is not a gate"
        );
    }

    /// The MODD ownership comes from each group file's MODR, read through the
    /// `{stem}_{gi:03}.wmo` path `load_nav` builds: a wrong path or a missed
    /// RDOM would leave every prop unowned, which fails open to always-drawn.
    #[test]
    fn a_goldshire_building_names_its_props_from_its_group_files() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let mut checked = 0;
        // Goldshire's tiles: the ADT names are `(x from world y, y from world x)`.
        for (tx, ty) in [(31, 49), (32, 49), (31, 48)] {
            let Ok(bytes) =
                chain.read_file(&format!("world\\maps\\azeroth\\azeroth_{tx}_{ty}.adt"))
            else {
                continue;
            };
            let Ok(tile) = benilla_formats::adt_to_tile_mesh(&bytes) else {
                continue;
            };
            for w in tile
                .wmos
                .iter()
                .filter(|w| w.model.to_ascii_lowercase().contains("goldshire"))
            {
                let Some(m) = load_nav(&mut chain, &w.model) else {
                    continue;
                };
                if !m.has_portals {
                    continue;
                }
                let named: usize = m.doodad_groups.iter().filter(|g| !g.is_empty()).count();
                assert!(
                    named > 0,
                    "{}: no group names a MODD — the group path or the MODR read is wrong",
                    w.model
                );
                assert!(
                    m.nav.iter().any(|n| n.flags & EXTERIOR == 0),
                    "{}: a building with portals and props has a room",
                    w.model
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no Goldshire building with portals on the probed tiles"
        );
    }
}

//! WMO portal culling: each frame, which of a building's groups are reachable through portals from
//! the camera's current group.
//!
//! The 1.12 client's recursion (`0x6b41c0`) floods the portal graph from the camera's group with a
//! screen rect as its frustum: per portal it tests that the camera is on the front side, projects
//! the portal to a screen rect, intersects that with the carried rect and recurses with the result,
//! ending a branch when the rect collapses. That is why the Stormwind cathedral culls from the
//! Trade District but draws from the gates.
//!
//! From inside, exterior shells draw only through deferred windows, doorways the flood reached onto
//! an `0x8` group (none skips the pass, `0x6b3c87`): per window, every `0x8` group inside a frustum
//! clipped to that doorway becomes a flood root (`0x6b3d39 call 0x6b41c0`), carrying its rect on.
//!
//! This module computes each group's visible bit ([`WmoPortalInstance::visible`]); the one
//! `Visibility` authority, `crate::model_render`'s `apply_model_visibility`, ANDs it with the rest.
//! The seed is the down-ray in [`seed`]. The epsilons here (the side test at exactly `0.0`, rect
//! collapse `0.001`, the `w`-clamp pair, the `0.1` near-parallel snap) are the client's own: do
//! not loosen them into tolerances.

use std::sync::Arc;

mod fog;
mod interior;
mod probe;
mod room_vis;

pub use benilla_cull::{
    floor_z_at, point_in_poly_2d, portal_poly, DownRaySeeds, FloodTrace, GroupPvs, Rect, EXTERIOR,
    EXTERIOR_LIT, PORTAL_NEAR_PARALLEL, PORTAL_PLANE_SNAP,
};
use fog::select_wmo_fog;
pub use fog::{CameraWmoFog, WmoFogTarget};
pub use interior::{
    indoor_verdict_at, indoors_at, terrain_z_local, IndoorVerdict, LightAttach,
    INTERIOR_PROBE_HEIGHT,
};
pub(crate) use interior::{surface_terrain_sample, WmoAreas, POSITION_PROBE_LIFT};
use interior::{track_area_interior, track_current_interior, track_unit_interiors};
pub use interior::{
    CurrentAreaInterior, CurrentWmoInterior, PlayerWmoRoom, UnitWmoRoom, WmoInteriorKeys,
};
use probe::TraceLog;
pub use probe::WmoCullProbe;
pub use room_vis::room_pvs_visible;

use crate::terrain_stream::{terrain_height_under, TerrainStreamer};
use crate::view::WorldCamera;
use benilla_assets::coords::bevy_to_wow;
use benilla_assets::{AdtTile, WmoModel};
use bevy::camera::Projection;
use bevy::math::Affine3A;
use bevy::prelude::*;

/// The deferred exterior-window worklist: the doorways the outdoor world may draw through this
/// frame, as NDC rects. The client's `[0xcbe324]` list (count `[0xcbe320]`), which the scene
/// driver `0x681070` copies to `0xc7cb7c` and walks once per window, the frustum narrowed to it;
/// `0x682fa0` is the only exterior producer, so no window means no exterior content (`0x681199`).
#[derive(Resource, Default, Clone, PartialEq, Debug)]
pub enum ExteriorWindows {
    /// The driver's outside leg (`0x6811ca`): no containing WMO, the whole frustum is the window.
    #[default]
    Unrestricted,
    /// The camera is inside: exterior content draws only where it survives one of these, and an
    /// empty list, the sealed room, draws none.
    Windows(Vec<Rect>),
}

/// Tag on every portal-culled WMO piece (a group submesh, a group's MLIQ surface, a MODD prop): its
/// placement instance and the absolute groups that can draw it. Geometry and liquid belong to one
/// group; a prop can be named by many, as the reference admits a prop from any visited group whose
/// MODR names it. Built once per group or prop at spawn.
#[derive(Component, Clone)]
pub struct WmoGroupVis {
    pub(crate) instance: Entity,
    pub(crate) groups: Arc<[u16]>,
}

impl WmoGroupVis {
    /// Drawn iff any referencing group is in this frame's PVS; an index past the visible set reads
    /// visible, so a lookup miss never blanks a building.
    pub fn drawn_by(&self, inst: &WmoPortalInstance) -> bool {
        self.groups
            .iter()
            .any(|&g| inst.visible.get(g as usize).copied().unwrap_or(true))
    }

    /// Takes the building's MFOG triple iff any naming group does (the client's `[0xca7f00]`,
    /// [`WmoPortalInstance::interior_fog`]). A miss reads false, unlike `drawn_by`: a missing bit
    /// must not paint a room's fog onto the open world.
    pub fn interior_fogged_by(&self, inst: &WmoPortalInstance) -> bool {
        self.groups.iter().any(|&g| inst.fogs_group(g))
    }
}

/// Is a room-bound rider drawn this frame? [`WmoGroupVis::drawn_by`] for lanes that carry their
/// rooms by value: a prop's particles and ribbons
/// ([`crate::particles::EmitterFade::room_admitted`]) and a building's point lights
/// ([`crate::lighting::LightRooms`]). The reference admits a WMO prop only in frames the portal
/// walk visits its group (`0x6838f0`, from the visit callback `0x685d70`), so a prop in a culled
/// room is neither drawn, animated nor particle-ticked.
///
/// - No rooms: admitted (an ADT doodad, a creature, a held item).
/// - Rooms, placement resident: `drawn_by`, the predicate the owner's submeshes are culled by.
/// - Rooms, placement gone: refused, where `drawn_by` fails open. A rider outlives its building by
///   a frame, and a stray bright strip or torch is an artefact where a missing one is nothing.
pub fn room_admits(room: Option<&WmoGroupVis>, instance: Option<&WmoPortalInstance>) -> bool {
    match (room, instance) {
        (None, _) => true,
        (Some(r), Some(inst)) => r.drawn_by(inst),
        (Some(_), None) => false,
    }
}

/// One placed WMO's portal-cull state, spawned with its geometry and despawned with the placement.
#[derive(Component)]
pub struct WmoPortalInstance {
    /// The building asset (portal graph and per-group nav).
    pub handle: Handle<WmoModel>,
    /// Placement transform, Bevy model→world: a WoW-space portal vertex reaches world as
    /// `world_from_local * wow_to_bevy(v)`, the path the rendered geometry takes.
    pub world_from_local: Affine3A,
    /// The placement's `MODF.nameSet`, the `WMOAreaTable.NameSetID` variant (one abbey model
    /// serves Northshire and Tyr's Hand by name set).
    pub name_set: u16,
    /// Per-group PVS bit this frame, by absolute group index; seeded all-true, which a portal-less
    /// prop keeps.
    pub visible: Vec<bool>,
    /// Per-group interior-fog gate this frame (the client's `[0xca7f00]`, see [`GroupPvs`]): `true`
    /// draws the group and its doodads with the building's MFOG triple, `false` with the scene fog.
    /// Seeded all-false, which a portal-less prop keeps.
    pub interior_fog: Vec<bool>,
    /// Per-group ever-visited latch: once the flood visits a group, its MLIQ surface draws for the
    /// rest of the placement's residency, whatever this frame's PVS. Latched by
    /// [`compute_wmo_pvs`]; starts all-false.
    pub liquid_visited: Vec<bool>,
    /// Per-group whole-group submersion from MOGP `groupLiquid`: `Some(kind)` means anywhere in the
    /// group is under liquid, at every z. Baked at spawn; empty on a placement spawned before its
    /// model resolved.
    pub flooded: Vec<Option<benilla_formats::LiquidKind>>,
}

impl WmoPortalInstance {
    /// The `[0xca7f00]` bit for one group, the grain every consumer asks at: a WMO piece ORs it
    /// over its groups, a unit asks it of the room its light node attached to. A group past the set
    /// reads false.
    pub fn fogs_group(&self, group: u16) -> bool {
        self.interior_fog
            .get(group as usize)
            .copied()
            .unwrap_or(false)
    }
}

/// One WMO room: a placement's instance entity and an absolute group index in it, the client's
/// `[0xc7b748]` (the camera's containing placed map object) with the group its containing-group
/// set `0xc7cd88` carries. It is also the liquid query's scope key: a liquid footprint has no
/// floor, so an unowned pool would claim every position under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WmoRoom {
    /// The placement's [`WmoPortalInstance`] entity.
    pub instance: Entity,
    /// Absolute group index within the building.
    pub group: u16,
}

/// The camera's interior claim this frame: `None` over the open world or an EXTERIOR (`0x8`)
/// group's floor, while an EXTERIOR_LIT-only porch still claims (the `[0xc7b748]` writer rejects on
/// `0x8` alone, `0x6be451`). Written by [`compute_wmo_pvs`] from the flood's own seed; the
/// reference's environment probe `0x6809c0` picks the WMO group's MLIQ over the ADT liquid on it.
#[derive(Resource, Default, Clone, Copy, PartialEq)]
pub struct CameraInteriorClaim(pub Option<InteriorClaim>);

/// See [`CameraInteriorClaim`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct InteriorClaim {
    /// The room the camera eye is in: the placement and its current group.
    pub room: WmoRoom,
    /// A group with any of `0x148` is in the placement's PVS: the weather flag `[0xca80c4]`
    /// (`0x6b42d9`), so rain shows through a doorway the camera can see.
    pub(crate) exterior_visible: bool,
}

/// Ordering handle so the `Visibility` authority runs after this frame's PVS.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WmoPvsSet;

/// Registers the per-frame PVS compute; the apply side is `crate::model_render`'s.
pub struct WmoPortalPlugin;

impl Plugin for WmoPortalPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Startup,
            interior::load_wmo_areas.after(benilla_assets::AssetSet::Open),
        )
        .init_resource::<CurrentWmoInterior>()
        .init_resource::<PlayerWmoRoom>()
        .init_resource::<CurrentAreaInterior>()
        .init_resource::<CameraWmoFog>()
        .init_resource::<CameraInteriorClaim>()
        .init_resource::<ExteriorWindows>()
        .init_resource::<WmoCullProbe>()
        .add_systems(
            Update,
            (
                compute_wmo_pvs,
                track_current_interior,
                track_area_interior,
                track_unit_interiors,
            )
                .in_set(WmoPvsSet)
                // On this frame's camera pose: the flood, the claim and the windows key on the eye
                // and decide what the frame may draw.
                .after(crate::view::CameraPoseSet),
        );
    }
}

/// Max drop (yd) for the current-group down-ray, the client's ray length: a floor further below
/// is not the room you are in.
const MAX_FLOOR_DROP: f32 = 1760.0;

/// Recompute each resident WMO's per-group visible set from the camera; a portal-less prop stays
/// all-visible.
fn compute_wmo_pvs(
    wmos: Res<Assets<WmoModel>>,
    cam: Query<(&GlobalTransform, &Projection), With<WorldCamera>>,
    mut instances: Query<(Entity, &mut WmoPortalInstance)>,
    streamer: Res<TerrainStreamer>,
    adt_tiles: Res<Assets<AdtTile>>,
    mut probe: ResMut<WmoCullProbe>,
    mut camera_fog: ResMut<CameraWmoFog>,
    mut camera_claim: ResMut<CameraInteriorClaim>,
    mut camera_windows: ResMut<ExteriorWindows>,
) {
    // No world camera yet: keep last frame's sets, all visible.
    let Some((cam_t, proj)) = cam.iter().next() else {
        return;
    };
    let clip_from_world = proj.get_clip_from_view() * cam_t.to_matrix().inverse();
    let eye_world = cam_t.translation();
    probe.eye = eye_world;
    // The seed's terrain leg, sampled once for the camera's column (`0x6821f0`).
    let terrain = terrain_height_under(&streamer, &adt_tiles, eye_world);
    // `WOW_CULLDUMP=<path>` re-requests the dump every frame, last writer wins: a headless run has
    // no panel button, and a one-shot request at startup would catch an empty world.
    static ENV_DUMP: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let dump_to = probe.dump_to.take().or_else(|| {
        ENV_DUMP
            .get_or_init(|| std::env::var_os("WOW_CULLDUMP").map(std::path::PathBuf::from))
            .clone()
    });
    // The full camera pose: replaying a dump needs forward, fov and aspect, not just the eye.
    let mut dump_text = dump_to.is_some().then(|| {
        let fwd = cam_t.forward();
        let (fovy, aspect) = match proj {
            Projection::Perspective(p) => (p.fov, p.aspect_ratio),
            _ => (f32::NAN, f32::NAN),
        };
        format!("eye world: {eye_world:?}\nforward: {fwd:?} fovy {fovy:.4} aspect {aspect:.4}\n")
    });

    // The fog target and the claim come from the flood's own seed; the first placement whose seed
    // claims a non-exterior group wins both.
    let mut fog_target: Option<WmoFogTarget> = None;
    let mut claim: Option<InteriorClaim> = None;
    // `None` is the driver's outside leg: no containing map object, one full-screen walk.
    let mut windows: Option<Vec<Rect>> = None;
    for (entity, mut inst) in &mut instances {
        let Some(model) = wmos.get(&inst.handle) else {
            continue;
        };
        let groups = model.group_nav.len();
        // A WMO with no portal graph (single-group props, doors) is never culled, and never on the
        // interior-fog lane. Written through the change gate: marking every prop changed each frame
        // would defeat `Changed<WmoPortalInstance>`.
        if model.portal_refs.is_empty() || model.portal_infos.is_empty() {
            let held = inst.bypass_change_detection();
            let mut changed = false;
            if held.visible.len() != groups || held.visible.iter().any(|v| !*v) {
                held.visible = vec![true; groups];
                changed = true;
            }
            if held.interior_fog.len() != groups || held.interior_fog.iter().any(|v| *v) {
                held.interior_fog = vec![false; groups];
                changed = true;
            }
            if changed {
                inst.set_changed();
            }
            continue;
        }
        let world_from_local = inst.world_from_local;
        // The eye in model space (WoW axes): the down-ray origin and the front-side test point.
        let local_from_world = world_from_local.inverse();
        let eye_local = bevy_to_wow(local_from_world.transform_point3(eye_world));
        let terrain_local = terrain.map(|z| terrain_z_local(&local_from_world, eye_world, z));
        // One flood with every recorder attached: the dump must never change the answer it records.
        let mut tap = SeedTap::default();
        let mut log = dump_text
            .is_some()
            .then(|| TraceLog::new(model, eye_local, terrain_local));
        // Compare, then write: a parked camera's flood repeats its answer, and a blind write would
        // mark every instance changed.
        let mut fresh = compute_pvs_traced(
            model,
            eye_local,
            terrain_local,
            &clip_from_world,
            &world_from_local,
            &mut (&mut tap, &mut log),
        );
        if inst.bypass_change_detection().visible != fresh.visible {
            inst.bypass_change_detection().visible = fresh.visible;
            inst.set_changed();
        }
        if inst.bypass_change_detection().interior_fog != fresh.interior_fog {
            inst.bypass_change_detection().interior_fog = fresh.interior_fog;
            inst.set_changed();
        }
        if let (Some(text), Some(log)) = (&mut dump_text, &log) {
            // The origin tells apart two placements of one model in the dump.
            let o = world_from_local.translation;
            text.push_str(&format!(
                "placement @ world ({:.1}, {:.1}, {:.1}) — {} deferred window(s) {:?}, {} walk steps\n",
                o.x,
                o.y,
                o.z,
                fresh.windows.len(),
                fresh.windows,
                fresh.iters
            ));
            text.push_str(&log.text);
            text.push_str(&format!("visible: {:?}\n\n", inst.visible));
        }
        // The claim rejects on `0x8` alone (the `[0xc7b748]` writer, `0x6be451`/`0x6be477`): a
        // `0x40`-only porch still claims the camera.
        if claim.is_none() {
            if let Some(gi) = tap.in_group {
                if model
                    .group_nav
                    .get(gi)
                    .is_some_and(|n| n.flags & EXTERIOR == 0)
                {
                    // The MFOG target exists only when one of the camera's containing groups (the
                    // in-group and across-group, `0xc7cd88`) is a true interior, with no `0x48`
                    // (the byteOut loop `0x69de5f`–`0x69dea0`). Its candidates come from the
                    // first such group's fog indices.
                    fog_target = [Some(gi), tap.across]
                        .into_iter()
                        .flatten()
                        .find_map(|g| {
                            model
                                .group_nav
                                .get(g)
                                .filter(|n| n.flags & (EXTERIOR | EXTERIOR_LIT) == 0)
                        })
                        .and_then(|n| select_wmo_fog(&model.fogs, n.fog_indices, eye_local));
                    claim = Some(InteriorClaim {
                        room: WmoRoom {
                            instance: entity,
                            group: gi as u16,
                        },
                        exterior_visible: model
                            .group_nav
                            .iter()
                            .zip(&inst.visible)
                            .any(|(n, v)| *v && n.flags & 0x148 != 0),
                    });
                    // The exterior windows are the claiming placement's alone: the reference floods
                    // the containing map object (`[0xc7b748]`) and defers that flood's windows,
                    // the list Pass 2 already used. Its push test is `0x8` alone
                    // (`opens_a_window`), not the `0x148` of `exterior_visible` above.
                    windows = Some(std::mem::take(&mut fresh.windows));
                }
            }
        }
        // The `liquid_visited` latch, through the change gate: only a slot that flips marks the
        // instance changed.
        let held = inst.bypass_change_detection();
        let mut latched = false;
        if held.liquid_visited.len() != groups {
            held.liquid_visited = vec![false; groups];
            latched = true;
        }
        for g in 0..groups {
            if held.visible.get(g).copied().unwrap_or(false) && !held.liquid_visited[g] {
                held.liquid_visited[g] = true;
                latched = true;
            }
        }
        if latched {
            inst.set_changed();
        }
    }
    if camera_fog.0 != fog_target {
        camera_fog.0 = fog_target;
    }
    if camera_claim.0 != claim {
        camera_claim.0 = claim;
    }
    let want_windows = match windows {
        Some(rects) => ExteriorWindows::Windows(rects),
        None => ExteriorWindows::Unrestricted,
    };
    if *camera_windows != want_windows {
        *camera_windows = want_windows;
    }
    if let Some((mut text, path)) = dump_text.zip(dump_to) {
        // The frame's published verdict, after every placement: the claim and the windows.
        text.push_str(&format!("CAMERA CLAIM: {:?}\n", camera_claim.0));
        text.push_str(&format!("EXTERIOR WINDOWS: {:?}\n", *camera_windows));
        // The folder is made only when there is a trace to put in it.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, &text) {
            Ok(()) => info!("wmo cull trace written to {}", path.display()),
            Err(e) => warn!("wmo cull trace: cannot write {}: {e}", path.display()),
        }
    }
}

/// The per-frame [`FloodTrace`] tap: the down-ray's seeds, for the claim and the fog without a
/// second down-ray.
#[derive(Default)]
struct SeedTap {
    in_group: Option<usize>,
    /// The across-portal seed; with [`Self::in_group`], the client's `0xc7cd88`, the camera's
    /// containing groups (not the flood's visible set).
    across: Option<usize>,
}
impl FloodTrace for SeedTap {
    fn seed(&mut self, seeds: DownRaySeeds) {
        self.in_group = seeds.in_group;
        self.across = seeds.across;
    }
}

/// [`benilla_cull::compute_pvs`] over a loaded model: the tests' and the audit's shorthand.
#[cfg(test)]
fn compute_pvs(
    model: &WmoModel,
    eye_local: [f32; 3],
    terrain_z: Option<f32>,
    clip_from_world: &Mat4,
    world_from_local: &Affine3A,
) -> GroupPvs {
    compute_pvs_traced(
        model,
        eye_local,
        terrain_z,
        clip_from_world,
        world_from_local,
        &mut (),
    )
}

/// The cull's view of a loaded WMO: every field the portal walk reads, borrowed. The model outlives
/// each call and nothing is copied; the cull itself is `benilla_cull`, the same code the UE host
/// runs, so the two can never drift apart.
fn cull_view(model: &WmoModel) -> benilla_cull::CullModel<'_> {
    benilla_cull::CullModel {
        nav: &model.group_nav,
        portal_vertices: &model.portal_vertices,
        portal_infos: &model.portal_infos,
        portal_refs: &model.portal_refs,
        collision_tris: &model.group_collision_tris,
        collision_grids: &model.group_collision_grids,
        camera_only_tris: &model.group_camera_only_tris,
    }
}

/// The down-ray's verdict for a loaded model: the camera's group and the group across the nearest
/// portal; `terrain_z` is the terrain height under the eye in model space.
pub fn down_ray_seeds(model: &WmoModel, eye: [f32; 3], terrain_z: Option<f32>) -> DownRaySeeds {
    benilla_cull::down_ray_seeds(&cull_view(model), eye, terrain_z)
}

/// The portal walk over a loaded model, with a [`FloodTrace`] tap; [`benilla_cull::compute_pvs_traced`]
/// is the implementation.
fn compute_pvs_traced<T: FloodTrace>(
    model: &WmoModel,
    eye_local: [f32; 3],
    terrain_z: Option<f32>,
    clip_from_world: &Mat4,
    world_from_local: &Affine3A,
    trace: &mut T,
) -> GroupPvs {
    benilla_cull::compute_pvs_traced(
        &cull_view(model),
        eye_local,
        terrain_z,
        clip_from_world,
        world_from_local,
        trace,
    )
}

#[cfg(test)]
mod audit;

#[cfg(test)]
mod tests {
    use super::*;

    use benilla_assets::{WmoGroupNav, WmoPortalInfo, WmoPortalRef};
    use benilla_cull::CALLBACK_PASS;

    /// Any visible referrer draws a prop; an index past the visible set fails open.
    #[test]
    fn a_prop_is_drawn_while_any_of_its_rooms_is_visible() {
        let inst = WmoPortalInstance {
            handle: Handle::default(),
            world_from_local: Affine3A::IDENTITY,
            name_set: 0,
            visible: vec![false, true, false],
            interior_fog: vec![false; 3],
            liquid_visited: vec![false; 3],
            flooded: vec![None; 3],
        };
        let key = |groups: &[u16]| WmoGroupVis {
            instance: Entity::PLACEHOLDER,
            groups: Arc::from(groups),
        };
        assert!(key(&[1]).drawn_by(&inst), "its own room is visible");
        assert!(!key(&[0]).drawn_by(&inst), "its own room is culled");
        assert!(
            key(&[0, 2, 1]).drawn_by(&inst),
            "one visible referrer out of three draws the fall"
        );
        assert!(!key(&[0, 2]).drawn_by(&inst), "no referrer is visible");
        assert!(
            !key(&[]).drawn_by(&inst),
            "no referrers at all: nothing ORs"
        );
        assert!(key(&[9]).drawn_by(&inst), "index past the set fails open");
    }

    /// The interior-fog key ORs like `drawn_by`, but a lookup miss reads false.
    #[test]
    fn the_interior_fog_key_ors_over_rooms_and_fails_closed() {
        let inst = WmoPortalInstance {
            handle: Handle::default(),
            world_from_local: Affine3A::IDENTITY,
            name_set: 0,
            visible: vec![true; 3],
            interior_fog: vec![false, true, false],
            liquid_visited: vec![false; 3],
            flooded: vec![None; 3],
        };
        let key = |groups: &[u16]| WmoGroupVis {
            instance: Entity::PLACEHOLDER,
            groups: Arc::from(groups),
        };
        assert!(key(&[1]).interior_fogged_by(&inst), "its own room is on it");
        assert!(
            !key(&[0]).interior_fogged_by(&inst),
            "its own room is off it"
        );
        assert!(
            key(&[0, 2, 1]).interior_fogged_by(&inst),
            "one referrer on the lane is enough"
        );
        assert!(
            !key(&[0, 2]).interior_fogged_by(&inst),
            "no referrer is on it"
        );
        assert!(
            !key(&[]).interior_fogged_by(&inst),
            "no referrers: nothing ORs"
        );
        assert!(
            !key(&[9]).interior_fogged_by(&inst),
            "index past the set fails CLOSED — the opposite of drawn_by"
        );
    }

    /// Pass 2 is a per-window test (`0x6b3d13`/`0x6b3d22`): of two portal-disconnected shells, the
    /// one through the doorway draws and the one 30 yd aside does not.
    #[test]
    fn pass_two_admits_only_the_exterior_shells_a_doorway_shows() {
        let model = doorway_model();
        // Eye 3 yd back from the doorway, WoW (-3, 0, 1) = Bevy (0, 1, 3), looking along WoW +x
        // (Bevy −z), straight through the opening.
        let eye_local = [-3.0, 0.0, 1.0];
        let view = Mat4::look_at_rh(
            bevy::math::Vec3::new(0.0, 1.0, 3.0),
            bevy::math::Vec3::new(0.0, 1.0, -10.0),
            bevy::math::Vec3::Y,
        );
        let clip = Mat4::perspective_rh(0.9, 16.0 / 9.0, 0.1, 1000.0) * view;
        let pvs = compute_pvs_traced(&model, eye_local, None, &clip, &Affine3A::IDENTITY, &mut ());

        assert_eq!(
            pvs.windows.len(),
            1,
            "the one portal onto an exterior group is the one deferred window"
        );
        assert!(pvs.visible[0], "the camera's own room");
        assert!(pvs.visible[1], "the exterior group the portal opens onto");
        assert!(
            pvs.visible[2],
            "a portal-disconnected shell straight through the doorway is admitted by its window \
             — this is the half the flood alone can never reach"
        );
        assert!(
            !pvs.visible[3],
            "a portal-disconnected shell 30 yd off the doorway's axis is culled: no window shows \
             it, and drawing it would be pure over-draw"
        );
        assert!(
            pvs.visible[4],
            "an admitted shell is FLOODED FROM, not merely marked — g4 is an interior behind \
             g2 with no edge to the seed's half of the graph, so the walk through g2's own back \
             door is its only route into the PVS (`0x6b3d39 call 0x6b41c0`). A marking Pass 2 \
             leaves it culled, which from a Darnassus shop is most of the city"
        );

        // The control: standing in the doorway takes the full-screen branch, and the side shell
        // comes back, so the cull above is the window's.
        let in_door = compute_pvs_traced(
            &model,
            [0.0, 0.0, 1.0],
            None,
            &clip,
            &Affine3A::IDENTITY,
            &mut (),
        );
        assert!(
            in_door.visible[3],
            "with the doorway full-screen, every shell in front of the camera draws"
        );
    }

    /// Pass 3 draws a `0x10000` group from a sealed room (the `jbe` fall-through at `0x6b3d6f`):
    /// with an empty worklist every `0x8` group is dark, yet the callback group draws, as
    /// Razorfen's enclosing shells (`0x8` and `0x10000`) and Stormwind's canals do.
    #[test]
    fn pass_three_draws_a_callback_group_from_a_sealed_room() {
        // The doorway model, sealed: no portals, no windows. g1..g3 keep `0x8`; g3 gains `0x10000`.
        let mut model = doorway_model();
        model.portal_refs.clear();
        model.group_nav[0].ref_count = 0;
        model.group_nav[1].ref_count = 0;
        model.group_nav[3].flags |= CALLBACK_PASS;
        // Aim at g3, 30 yd off-axis, so the full frustum holds it: WoW (2..6, 30..34) is Bevy
        // x ∈ [-34,-30], z ∈ [-6,-2].
        let view = Mat4::look_at_rh(
            bevy::math::Vec3::new(0.0, 1.0, 3.0),
            bevy::math::Vec3::new(-32.0, 1.0, -4.0),
            bevy::math::Vec3::Y,
        );
        let clip = Mat4::perspective_rh(0.9, 16.0 / 9.0, 0.1, 1000.0) * view;
        let pvs = compute_pvs_traced(
            &model,
            [-3.0, 0.0, 1.0],
            None,
            &clip,
            &Affine3A::IDENTITY,
            &mut (),
        );

        assert!(
            pvs.windows.is_empty(),
            "a sealed room opens no doorway onto the outdoors"
        );
        assert!(pvs.visible[0], "the camera's own room");
        assert!(
            !pvs.visible[1] && !pvs.visible[2],
            "plain 0x8 shells stay dark with an empty worklist — Pass 2 never runs"
        );
        assert!(
            pvs.visible[3],
            "the 0x10000 group draws anyway: Pass 3 is the jbe fall-through, full frustum, ungated"
        );

        // The control: Pass 3 is still a frustum test, so facing away darkens g3.
        let away = Mat4::perspective_rh(0.9, 16.0 / 9.0, 0.1, 1000.0)
            * Mat4::look_at_rh(
                bevy::math::Vec3::new(0.0, 1.0, 3.0),
                bevy::math::Vec3::new(32.0, 1.0, 10.0),
                bevy::math::Vec3::Y,
            );
        let behind = compute_pvs_traced(
            &model,
            [-3.0, 0.0, 1.0],
            None,
            &away,
            &Affine3A::IDENTITY,
            &mut (),
        );
        assert!(
            !behind.visible[3],
            "a callback group behind the camera is still frustum-culled"
        );

        // The outside leg draws it too (`0x6b3dd0` forks on the same bit): an eye with no floor
        // under it seeds only `0x8` groups, and g3, with no portal refs, is Pass 3's alone.
        let outside = compute_pvs_traced(
            &model,
            [-3.0, 0.0, 500.0],
            None,
            &clip,
            &Affine3A::IDENTITY,
            &mut (),
        );
        assert!(
            outside.visible[3],
            "the callback arm belongs to BOTH legs — gating it on the inside leg left 20 of the 24 \
             real groups undrawable from anywhere"
        );
    }

    /// An interior room `g0` (floor at z=0, the seed) whose doorway `p0` (WoW x=0, y ∈ [-0.5,0.5],
    /// z ∈ [0,2]) opens onto exterior `g1`, two portal-disconnected exterior shells, `g2` ahead
    /// through the opening and `g3` 30 yd aside, and `g4`, reached only through `g2`.
    fn doorway_model() -> WmoModel {
        let nav = |flags: u32, min: [f32; 3], max: [f32; 3], ref_start: u16, ref_count: u16| {
            WmoGroupNav {
                flags,
                bbox_min: min,
                bbox_max: max,
                ref_start,
                ref_count,
                area_table_id: 0,
                fog_indices: [0; 4],
                group_liquid: benilla_formats::NO_GROUP_LIQUID,
            }
        };
        let floor = |z: f32| {
            vec![
                [[-10.0, -10.0, z], [0.0, -10.0, z], [0.0, 10.0, z]],
                [[-10.0, -10.0, z], [0.0, 10.0, z], [-10.0, 10.0, z]],
            ]
        };
        WmoModel {
            portal_vertices: vec![
                [0.0, -0.5, 0.0],
                [0.0, 0.5, 0.0],
                [0.0, 0.5, 2.0],
                [0.0, -0.5, 2.0],
                // p1, the back door of g2 and g4's only way in.
                [6.0, -0.5, 0.0],
                [6.0, 0.5, 0.0],
                [6.0, 0.5, 2.0],
                [6.0, -0.5, 2.0],
            ],
            portal_infos: vec![
                WmoPortalInfo {
                    start_vertex: 0,
                    count: 4,
                    plane: [1.0, 0.0, 0.0, 0.0],
                },
                WmoPortalInfo {
                    start_vertex: 4,
                    count: 4,
                    plane: [1.0, 0.0, 0.0, -6.0],
                },
            ],
            // g0 ↔ g1, `side = -1` putting the eye at x < 0 on the entering half-space; then
            // g2 ↔ g4 through p1, reachable only by Pass 2 admitting g2 and walking on.
            portal_refs: vec![
                WmoPortalRef {
                    portal: 0,
                    group: 1,
                    side: -1,
                },
                WmoPortalRef {
                    portal: 0,
                    group: 0,
                    side: 1,
                },
                WmoPortalRef {
                    portal: 1,
                    group: 4,
                    side: -1,
                },
                WmoPortalRef {
                    portal: 1,
                    group: 2,
                    side: 1,
                },
            ],
            group_nav: vec![
                nav(0x2000, [-10.0, -10.0, 0.0], [0.0, 10.0, 3.0], 0, 1),
                nav(EXTERIOR, [1.0, -0.5, 0.0], [5.0, 0.5, 2.0], 1, 1),
                nav(EXTERIOR, [2.0, -0.5, 0.0], [6.0, 0.5, 2.0], 2, 1),
                nav(EXTERIOR, [2.0, 30.0, 0.0], [6.0, 34.0, 2.0], 0, 0),
                nav(0x2000, [7.0, -0.5, 0.0], [11.0, 0.5, 2.0], 3, 1),
            ],
            group_collision_tris: vec![floor(0.0), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            ..portal_model()
        }
    }

    /// A bare model with one vertical doorway portal in the WoW x=0 plane, y and z in [-1,1].
    fn portal_model() -> WmoModel {
        WmoModel {
            wmo_id: 0,
            submeshes: Vec::new(),
            submesh_group: Vec::new(),
            portal_vertices: vec![
                [0.0, -1.0, -1.0],
                [0.0, 1.0, -1.0],
                [0.0, 1.0, 1.0],
                [0.0, -1.0, 1.0],
            ],
            portal_infos: vec![WmoPortalInfo {
                start_vertex: 0,
                count: 4,
                plane: [1.0, 0.0, 0.0, 0.0],
            }],
            portal_refs: Vec::new(),
            group_nav: Vec::new(),
            fogs: Vec::new(),
            skybox: None,
            group_collision_tris: Vec::new(),
            group_camera_only_tris: Vec::new(),
            group_collision_bounds: Vec::new(),
            group_collision_grids: Vec::new(),
            collision_bounds: None,
            collision: None,
            collision_camera: None,
            doodads: Vec::new(),
            doodad_sets: Vec::new(),
            lights: Vec::new(),
            group_bounds: Vec::new(),
            group_footprints: Vec::new(),
            material_ground_type: Vec::new(),
            material_diff_color: Vec::new(),
            group_footprint_bounds: Vec::new(),
            group_footprint_grids: Vec::new(),
            group_light_refs: Vec::new(),
            group_liquids: Vec::new(),
            doodad_base: Default::default(),
            doodad_owner: Default::default(),
            doodad_groups: Default::default(),
        }
    }
}

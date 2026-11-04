//! The selection ring: the ground marker the client draws under its selected unit.
//!
//! A port of the app's `crates/benilla-app/src/target/ring.rs` across the FFI boundary: the same
//! footprint (`0x60aee0`), the same selector palette (`0x605960`) and the same camera-fed fade
//! (`0x608e00`). The app projects that box through its own decal projector and pushes the result as
//! an additive world-effect batch; the dll answers the batch and the host draws it, the way
//! [`crate::quest_markers`] answers a marker model.
//!
//! - [`bn_selection_ring`] resolves the selection: the ring's art, its vertex count and its radius.
//! - [`bn_selection_ring_verts`] hands over the vertices, world-space and fan-unrolled, exactly as
//!   the app's `RingState.verts` holds them.
//!
//! Two gaps, both from machinery the dll does not carry: the app rasterises the box against its
//! collider scene (`0x6d7330`), clipping the square on the exact faces that cross it, while the dll
//! samples the same surface set on a lattice over the square — so an edge or a wall inside the box
//! leaves the mesh between the samples flat instead of cut. And [`ring_rank`] is the template
//! comparator alone (`0x606640`): the dll holds no reputation standings, so a rank-2 faction reads
//! by its template reaction and the duel, contested-guard and party rungs of `0x6061e0` are not
//! applied.

use std::ffi::c_char;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use benilla_formats::{FactionCatalog, FactionTemplate, Reaction};
use benilla_protocol::messages::ObjectFields;

use crate::creature::{catalog, m2_path};
use crate::net;
use crate::BnWorld;

/// `0x60aee0`'s answer for a model with no sequences: the degenerate-box footprint.
const RING_FALLBACK_RADIUS: f32 = 1.2;
/// The ring's own art, the reference's `Load 0x6146d0` path.
const RING_TEXTURE: &str = "textures\\unitselecttexture.blp";
/// `UNIT_FIELD_PVP`, which CanAttack's player-vs-player arm accepts on its target (`0x606b5c`).
const UNIT_FLAG_PVP: u32 = 0x1000;

/// One world-effect vertex, the app's `EffectVertex`: a position in world WoW yards (z up), its
/// texture coordinates and its colour with the fade's alpha, as linear floats.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct BnEffectVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// The ring's answer for the currently selected unit.
#[repr(C)]
pub struct BnSelectionRing {
    /// The selected unit; 0 when the client has no selection.
    pub guid: u64,
    /// The box's half-extent in yards, carrying the model and object scales.
    pub radius: f32,
    /// How many vertices [`bn_selection_ring_verts`] writes.
    pub vert_count: u32,
    /// The ring's BLP, NUL-terminated.
    pub tex: [c_char; 96],
}

/// The ring's vertices, kept between the two calls the host makes (count, then fill).
fn held_verts() -> &'static Mutex<Vec<BnEffectVertex>> {
    static CACHE: OnceLock<Mutex<Vec<BnEffectVertex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// The colour class `GetSelectionCircleColor 0x605960` resolves, with the app's palette.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RingVariant {
    Hostile,
    Unfriendly,
    Neutral,
    Friendly,
    Player,
    Dead,
    Party,
    PartyPvp,
}

impl RingVariant {
    fn color(self) -> [f32; 3] {
        match self {
            Self::Hostile => [1.0, 0.0, 0.0],
            Self::Unfriendly => [1.0, 0.502, 0.0],
            Self::Neutral => [1.0, 1.0, 0.0],
            Self::Friendly => [0.0, 1.0, 0.0],
            Self::Player => [0.376, 0.376, 1.0],
            Self::Dead => [0.498, 0.498, 0.498],
            Self::Party => [0.667, 0.667, 1.0],
            Self::PartyPvp => [0.667, 1.0, 0.667],
        }
    }
}

/// The selector's player-path branches (`0x605bb6`-`0x605c2c`); any other unit takes the rank
/// palette, gray for a corpse.
#[allow(clippy::too_many_arguments)]
fn variant(
    is_player: bool,
    attacks_us: bool,
    attackable: bool,
    pvp: bool,
    in_party: bool,
    rank: u8,
    dead: bool,
) -> RingVariant {
    if is_player {
        return match (attacks_us, attackable, pvp, in_party) {
            (true, true, ..) => RingVariant::Hostile,
            (true, false, ..) => RingVariant::Player,
            (false, true, ..) => RingVariant::Neutral,
            (false, false, true, true) => RingVariant::PartyPvp,
            (false, false, true, false) => RingVariant::Friendly,
            (false, false, false, true) => RingVariant::Party,
            (false, false, false, false) => RingVariant::Player,
        };
    }
    if dead {
        return RingVariant::Dead;
    }
    match rank {
        0..=1 => RingVariant::Hostile,
        2 => RingVariant::Unfriendly,
        3 => RingVariant::Neutral,
        _ => RingVariant::Friendly,
    }
}

/// One unit's `FactionTemplate.dbc` row, the way `ring_reaction` (`0x606530`) takes it: the
/// descriptor names the template, and the comparator reads the two rows directly.
pub(crate) fn template_of(
    factions: Option<&FactionCatalog>,
    f: &ObjectFields,
) -> Option<FactionTemplate> {
    factions?.template(f.unit_faction_template()?).copied()
}

/// `0x606640` alone: the target's reaction toward us as its rank, Neutral when a template or the
/// catalog is missing.
fn ring_rank(factions: Option<&FactionCatalog>, target: &ObjectFields, own: &ObjectFields) -> u8 {
    reaction(factions, target, own).unwrap_or(Reaction::Neutral) as u8
}

/// `a`'s template reaction toward `b`, `None` when either side has no row. Hostility is only ever
/// the first argument's, so which way round the two are passed is the whole question.
fn reaction(
    factions: Option<&FactionCatalog>,
    a: &ObjectFields,
    b: &ObjectFields,
) -> Option<Reaction> {
    let t = template_of(factions, a)?;
    let o = template_of(factions, b)?;
    Some(t.reaction_toward(&o))
}

/// The camera's ground-projected fade, the app's `ring_fade_angle` read in WoW's z-up frame:
/// `-π/2 − atan2(f.y, f.x)` with the vertical component dropped. `None` looking straight down, where
/// the caller keeps the last angle.
fn fade_angle(fwd: [f32; 3]) -> Option<f32> {
    let (fx, fy) = (fwd[0], fwd[1]);
    if fx * fx + fy * fy < 1e-6 {
        return None;
    }
    Some(-std::f32::consts::FRAC_PI_2 - fy.atan2(fx))
}

/// The fade angle held between calls, so a straight-down camera keeps the last one as the app's
/// `Local<f32>` does.
fn held_fade(fresh: Option<f32>) -> f32 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    match fresh {
        Some(y) => {
            LAST.store(u64::from(y.to_bits()), Ordering::Relaxed);
            y
        }
        None => match LAST.load(Ordering::Relaxed) {
            0 => 0.0,
            bits => f32::from_bits(bits as u32),
        },
    }
}

/// One display's model-local Stand box: the ring's `ring_footprint`, the display row's own scale and
/// the box's Z extent (the blob shadow's vertical reach). Cached per display id, since the answer
/// needs an M2 read. A display with no row takes the degenerate-box footprint at scale 1 and no
/// height, so a unit always gets a ring.
pub(crate) fn display_bounds(world: &mut BnWorld, display_id: u32) -> (f32, f32, f32) {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<u32, (f32, f32, f32)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(r) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&display_id)
    {
        return *r;
    };
    let cat = catalog(world);
    let answer = match cat.as_ref().and_then(|c| c.model(display_id)) {
        Some(model) => {
            let path = m2_path(&model.model_path);
            let bounds = crate::doodad_rig::model_bytes(world, &path)
                .as_deref()
                .and_then(|bytes| benilla_formats::parse_m2_bounds(bytes).ok());
            let footprint = bounds.map_or(RING_FALLBACK_RADIUS, |b| b.ring_footprint);
            let height = bounds.map_or(0.0, |b| b.stand_box_z);
            (footprint, model.scale, height)
        }
        None => (RING_FALLBACK_RADIUS, 1.0, 0.0),
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(display_id, answer);
    answer
}

fn ring_local(world: &mut BnWorld, display_id: u32) -> (f32, f32) {
    let (footprint, scale, _) = display_bounds(world, display_id);
    (footprint, scale)
}

/// The ring's box (`0x608e00`): the texture square, yawed by the fade, laid over the ground.
///
/// The app sends the square through its decal projector (`0x6d7330`), which clips it onto whatever
/// surface crosses the box — two radii above and below the feet — and takes a vertex's alpha from
/// how deep in the box it landed. The dll samples the same surfaces the mover's ground cast reads
/// (`Player::ground`) on a 3×3-cell lattice over the square and keeps that alpha law: 16 lattice
/// points, 18 quads' worth of fan-unrolled triangles. A point with no surface inside the box stays
/// at the feet.
fn ring_box(
    feet: [f32; 3],
    radius: f32,
    fade: f32,
    tint: [f32; 3],
    ground: impl Fn(f32, f32) -> Option<f32>,
) -> Vec<BnEffectVertex> {
    const CELLS: usize = 3;
    let (sin, cos) = fade.sin_cos();
    // The box's own half-height, and the app's fade: full at the feet's level, gone two radii off it.
    let vert = 2.0 * radius;
    let mut lattice = [[BnEffectVertex::default(); CELLS + 1]; CELLS + 1];
    for (i, row) in lattice.iter_mut().enumerate() {
        let sx = -1.0 + 2.0 * (i as f32 / CELLS as f32);
        for (j, cell) in row.iter_mut().enumerate() {
            // The frame's axes in world WoW, the inverse of the projector's `in_frame` rotation:
            // `x' → (cos, −sin)`, `z' → (sin, cos)`.
            let sz = -1.0 + 2.0 * (j as f32 / CELLS as f32);
            let x = feet[0] + radius * (sx * cos + sz * sin);
            let y = feet[1] + radius * (sz * cos - sx * sin);
            let z = ground(x, y)
                .filter(|z| (feet[2] - vert..=feet[2] + vert).contains(z))
                .unwrap_or(feet[2]);
            *cell = BnEffectVertex {
                pos: [x, y, z],
                uv: [(sx + 1.0) * 0.5, (sz + 1.0) * 0.5],
                color: [
                    tint[0],
                    tint[1],
                    tint[2],
                    ((vert - (z - feet[2]).abs()) / (1.5 * radius)).clamp(0.0, 1.0),
                ],
            };
        }
    }
    let mut out = Vec::with_capacity(CELLS * CELLS * 6);
    for i in 0..CELLS {
        for j in 0..CELLS {
            let (a, b, c, d) = (
                lattice[i][j],
                lattice[i + 1][j],
                lattice[i + 1][j + 1],
                lattice[i][j + 1],
            );
            out.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    out
}

/// The ring under the selected unit: `1` with `out` filled and the vertices held for
/// [`bn_selection_ring_verts`], `0` when the client has no selection or lost the unit.
#[no_mangle]
pub extern "C" fn bn_selection_ring(world: *mut BnWorld, out: *mut BnSelectionRing) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller passes an exclusive, valid handle.
    let w = unsafe { &mut *world };
    let clear = |cell: &Mutex<Vec<BnEffectVertex>>| {
        cell.lock().unwrap_or_else(|e| e.into_inner()).clear();
        0
    };
    let guid = net::ui_selection();
    let Some(unit) = net::ui_unit(guid) else {
        return clear(held_verts());
    };
    let Some(self_fields) = net::ui_self_fields() else {
        return clear(held_verts());
    };
    let cats = factions(w);
    let f = &unit.fields;
    let in_party = {
        let party = net::ui_party();
        party.members[..party.count.min(party.members.len() as u32) as usize]
            .iter()
            .any(|m| m.guid == guid)
    };
    let tint = variant(
        unit.kind == 4,
        reaction(cats.as_deref(), f, &self_fields) == Some(Reaction::Hostile),
        reaction(cats.as_deref(), &self_fields, f) == Some(Reaction::Hostile),
        f.unit_flags() & UNIT_FLAG_PVP != 0,
        in_party,
        ring_rank(cats.as_deref(), f, &self_fields),
        f.unit_is_dead(),
    )
    .color();

    // The mount carries the footprint when there is one (`0x60ce70` recomputes the ring's cache from
    // the mount), and then its own row's scale sizes it.
    let mount = f.unit_mount_display_id();
    let display = if mount != 0 {
        mount
    } else {
        f.unit_displayid().unwrap_or(0).max(0) as u32
    };
    // `0x608e00` multiplies the footprint `0x60aee0` cached by the object's scale once. The catalog
    // scale already in the footprint was a second multiply: it is the same number the server writes
    // to `OBJECT_FIELD_SCALE_X` (vmangos `Unit::GetScaleForDisplayId`), so every creature's ring
    // came out a scale short of its body - a tarantula's circle at 0.4 of itself. The row's scale
    // stands in only where the field never arrived.
    let (local, row_scale) = ring_local(w, display);
    let scale = if mount != 0 {
        row_scale
    } else {
        f.object_scale_x().filter(|s| *s > 0.0).unwrap_or(row_scale)
    };
    let radius = local * scale.max(0.01);
    let fade = held_fade(w.player.as_ref().and_then(|p| fade_angle(p.camera_fwd())));

    // The body is drawn on the ground clamp (`bn_ground_clamp_z`), so the ring reads the same
    // feet: benilla clamps a creature's translation before its ring sees it, and a ring left on
    // the wire's seat floats above a body whose Z hangs, or sinks into a slope under one that
    // sits low.
    let pose = net::ui_unit_pose(guid).unwrap_or(unit.pos);
    let feet = match (unit.kind, w.player.as_ref()) {
        (3, Some(p)) => crate::player::clamped_feet(p, pose),
        _ => pose,
    };
    // The projector's box: the topmost surface at or below its roof. [`ring_box`] drops what lies
    // below its floor, and holds the feet's level where the cast finds nothing.
    let box_top = feet[2] + 2.0 * radius;
    let box_verts = ring_box(feet, radius, fade, tint, |x, y| {
        w.player
            .as_ref()
            .and_then(|p| p.ground(x, y, box_top))
            .map(|(z, _)| z)
    });
    let vert_count = box_verts.len() as u32;
    held_verts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone_from(&box_verts);

    let mut tex = [0 as c_char; 96];
    for (slot, byte) in RING_TEXTURE.bytes().take(95).enumerate() {
        tex[slot] = byte as c_char;
    }
    unsafe {
        *out = BnSelectionRing {
            guid,
            radius,
            vert_count,
            tex,
        };
    }
    1
}

/// The selected unit's ring vertices, world-space and fan-unrolled; a null `out` or `cap` 0 asks for
/// the count without copying. The host draws them additively with [`bn_selection_ring`]'s texture, as
/// the app pushes its own batch.
#[no_mangle]
pub extern "C" fn bn_selection_ring_verts(out: *mut BnEffectVertex, cap: u32) -> u32 {
    let held = held_verts().lock().unwrap_or_else(|e| e.into_inner());
    let n = held.len();
    if out.is_null() || cap == 0 {
        return n as u32;
    }
    let take = n.min(cap as usize);
    // SAFETY: caller owns `cap` writable vertices.
    unsafe {
        std::ptr::copy_nonoverlapping(held.as_ptr(), out, take);
    }
    take as u32
}

type FactionCatalogs = Option<(String, Arc<FactionCatalog>)>;

/// `FactionTemplate.dbc`, parsed once per process off the handle's chain.
pub(crate) fn factions(world: &mut BnWorld) -> Option<Arc<FactionCatalog>> {
    static CACHE: OnceLock<Mutex<FactionCatalogs>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, cat)) = guard.as_ref() {
        if *dir == world.data_dir {
            return Some(Arc::clone(cat));
        }
    }
    let chain = world.chain.as_mut()?;
    let cat = Arc::new(benilla_formats::load_faction_catalog(chain).ok()?);
    *guard = Some((world.data_dir.clone(), Arc::clone(&cat)));
    Some(cat)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over no ground the box is the texture square at the feet's own height: every vertex inside
    /// the diagonal radius, at full alpha, and the four corners carry the full UV range.
    #[test]
    fn ring_box_is_the_yawed_texture_square() {
        for fade in [0.0f32, 0.7, -2.1] {
            let v = ring_box([3.0, -4.0, 12.0], 2.0, fade, [1.0, 0.0, 0.0], |_, _| None);
            assert_eq!(v.len(), 54);
            let corner = (2.0f32 * 2.0 * 2.0).sqrt();
            for vertex in &v {
                let dx = vertex.pos[0] - 3.0;
                let dy = vertex.pos[1] + 4.0;
                assert!(dx.hypot(dy) <= corner + 1e-4, "{vertex:?}");
                assert_eq!(vertex.pos[2], 12.0);
                assert!((0.0..=1.0).contains(&vertex.uv[0]));
                assert!((0.0..=1.0).contains(&vertex.uv[1]));
                assert_eq!(vertex.color, [1.0, 0.0, 0.0, 1.0]);
            }
            for uv in [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]] {
                assert!(v.iter().any(|x| x.uv == uv), "no vertex at UV {:?}", uv);
            }
        }
    }

    /// A fade of 0 leaves the box axis-aligned: `x'` along world +X, `z'` along world +Y. The first
    /// triangle is the lattice's corner cell, so it opens at the square's own corner.
    #[test]
    fn zero_fade_keeps_the_box_axis_aligned() {
        let v = ring_box([0.0, 0.0, 0.0], 1.0, 0.0, [0.0, 1.0, 0.0], |_, _| None);
        assert_eq!(v[0].pos, [-1.0, -1.0, 0.0]);
        assert!((v[1].pos[0] + 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(v[1].pos[1], -1.0);
        assert!((v[2].pos[1] + 1.0 / 3.0).abs() < 1e-6);
        assert!(v.iter().all(|x| x.pos[0] <= 1.0 && x.pos[0] >= -1.0));
        assert!(v.iter().all(|x| x.pos[1] <= 1.0 && x.pos[1] >= -1.0));
    }

    /// The app's fade law rides on the sampled height: ground two radii under the feet is the box's
    /// own floor and reads 0, a shelf inside the box keeps its colour and half its alpha.
    #[test]
    fn the_ring_fades_where_the_ground_drops_away() {
        let v = ring_box([0.0, 0.0, 0.0], 1.0, 0.0, [0.0, 1.0, 0.0], |x, _| {
            (x < 0.0).then_some(-2.0)
        });
        for vertex in &v {
            let sunk = vertex.pos[0] < 0.0;
            assert_eq!(vertex.pos[2], if sunk { -2.0 } else { 0.0 });
            let a = if sunk { 0.0 } else { 1.0 };
            assert!((vertex.color[3] - a).abs() < 1e-6, "{vertex:?}");
        }
        let one_radius_down = ring_box([0.0, 0.0, 0.0], 1.0, 0.0, [0.0, 1.0, 0.0], |_, _| {
            Some(-1.0)
        });
        assert!(one_radius_down
            .iter()
            .all(|x| (x.color[3] - (2.0 - 1.0) / 1.5).abs() < 1e-6));
        // Nothing inside the box's band: the vertex holds the feet, not the far surface.
        let below = ring_box([0.0, 0.0, 0.0], 1.0, 0.0, [0.0, 1.0, 0.0], |_, _| {
            Some(-9.0)
        });
        assert!(below.iter().all(|x| x.pos[2] == 0.0 && x.color[3] == 1.0));
    }

    /// Straight down has no ground-projected forward, so the caller keeps the last fade.
    #[test]
    fn a_straight_down_camera_holds_the_last_fade() {
        assert!(fade_angle([0.0, 0.0, -1.0]).is_none());
        let held = held_fade(fade_angle([1.0, 1.0, -1.0]));
        assert_eq!(held_fade(None), held);
    }
}

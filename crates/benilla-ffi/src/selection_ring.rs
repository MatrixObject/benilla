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
//! collider scene (`0x6d7330`), which has no dll equivalent, so the answer is the box's own slice at
//! the feet — the triangles the projector emits over flat ground, and a quad that clips into a slope
//! elsewhere. And [`ring_rank`] is the template comparator alone (`0x606640`): the dll holds no
//! reputation standings, so a rank-2 faction reads by its template reaction and the duel,
//! contested-guard and party rungs of `0x6061e0` are not applied.

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
fn template_of(factions: Option<&FactionCatalog>, f: &ObjectFields) -> Option<FactionTemplate> {
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

/// One model's `ring_footprint` times its render scale: the box the reference sizes from the model's
/// Stand sequence (`0x60aee0`). Cached per display id, since the answer needs an M2 read. A display
/// with no row takes the degenerate-box footprint, so a unit always gets a ring.
fn ring_radius(world: &mut BnWorld, display_id: u32) -> f32 {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<u32, f32>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(r) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&display_id)
    {
        return *r;
    };
    let cat = catalog(world);
    let radius = match cat.as_ref().and_then(|c| c.model(display_id)) {
        Some(model) => {
            let path = m2_path(&model.model_path);
            let scale = model.scale;
            let footprint = crate::doodad_rig::model_bytes(world, &path)
                .as_deref()
                .and_then(|bytes| benilla_formats::parse_m2_bounds(bytes).ok())
                .map_or(RING_FALLBACK_RADIUS, |bounds| bounds.ring_footprint);
            footprint * scale
        }
        None => RING_FALLBACK_RADIUS,
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(display_id, radius);
    radius
}

/// The ring's box (`0x608e00`): the texture square, yawed by the fade and sampled at the feet.
/// Fan-unrolled as the projector emits it — six vertices, two triangles.
fn ring_box(feet: [f32; 3], radius: f32, fade: f32, tint: [f32; 3]) -> Vec<BnEffectVertex> {
    let (sin, cos) = fade.sin_cos();
    // The frame's axes in world WoW, the inverse of the projector's `in_frame` rotation:
    // `x' → (cos, −sin)`, `z' → (sin, cos)`.
    let vert = |sx: f32, sz: f32| BnEffectVertex {
        pos: [
            feet[0] + radius * (sx * cos + sz * sin),
            feet[1] + radius * (sz * cos - sx * sin),
            feet[2],
        ],
        uv: [(sx + 1.0) * 0.5, (sz + 1.0) * 0.5],
        color: [tint[0], tint[1], tint[2], 1.0],
    };
    let (a, b, c, d) = (
        vert(-1.0, -1.0),
        vert(1.0, -1.0),
        vert(1.0, 1.0),
        vert(-1.0, 1.0),
    );
    vec![a, b, c, a, c, d]
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
    // the mount); the object's own `OBJECT_FIELD_SCALE_X` rides on top of either.
    let mount = f.unit_mount_display_id();
    let display = if mount != 0 {
        mount
    } else {
        f.unit_displayid().unwrap_or(0).max(0) as u32
    };
    let radius = ring_radius(w, display) * f.object_scale_x().unwrap_or(1.0).max(0.01);
    let fade = held_fade(w.player.as_ref().and_then(|p| fade_angle(p.camera_fwd())));

    let box_verts = ring_box(
        net::ui_unit_pose(guid).unwrap_or(unit.pos),
        radius,
        fade,
        tint,
    );
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
fn factions(world: &mut BnWorld) -> Option<Arc<FactionCatalog>> {
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

    /// The box is the texture square: every corner sits one diagonal radius out, at the feet's own
    /// height, and carries the full UV range. The fade turns the square without stretching it.
    #[test]
    fn ring_box_is_the_yawed_texture_square() {
        for fade in [0.0f32, 0.7, -2.1] {
            let v = ring_box([3.0, -4.0, 12.0], 2.0, fade, [1.0, 0.0, 0.0]);
            assert_eq!(v.len(), 6);
            for vertex in &v {
                let dx = vertex.pos[0] - 3.0;
                let dy = vertex.pos[1] + 4.0;
                assert!((dx.hypot(dy) - (2.0f32 * 2.0 * 2.0).sqrt()).abs() < 1e-4);
                assert_eq!(vertex.pos[2], 12.0);
                assert!((0.0..=1.0).contains(&vertex.uv[0]));
                assert!((0.0..=1.0).contains(&vertex.uv[1]));
                assert_eq!(vertex.color, [1.0, 0.0, 0.0, 1.0]);
            }
            assert_eq!(v[0].uv, [0.0, 0.0]);
            assert_eq!(v[2].uv, [1.0, 1.0]);
            // The fan repeats the square's first two corners.
            assert_eq!(v[3].uv, v[0].uv);
            assert_eq!(v[4].uv, v[2].uv);
        }
    }

    /// A fade of 0 leaves the box axis-aligned: `x'` along world +X, `z'` along world +Y. The fan
    /// repeats the square's first corner, so the fourth corner is its last vertex.
    #[test]
    fn zero_fade_keeps_the_box_axis_aligned() {
        let v = ring_box([0.0, 0.0, 0.0], 1.0, 0.0, [0.0, 1.0, 0.0]);
        assert_eq!(v[0].pos, [-1.0, -1.0, 0.0]);
        assert_eq!(v[1].pos, [1.0, -1.0, 0.0]);
        assert_eq!(v[2].pos, [1.0, 1.0, 0.0]);
        assert_eq!(v[5].pos, [-1.0, 1.0, 0.0]);
    }

    /// Straight down has no ground-projected forward, so the caller keeps the last fade.
    #[test]
    fn a_straight_down_camera_holds_the_last_fade() {
        assert!(fade_angle([0.0, 0.0, -1.0]).is_none());
        let held = held_fade(fade_angle([1.0, 1.0, -1.0]));
        assert_eq!(held_fade(None), held);
    }
}

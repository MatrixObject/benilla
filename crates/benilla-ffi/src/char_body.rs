//! Middle-layer FFI: character appearance and animation selection.
//!
//! Player M2s carry *empty* texture-slot names: the client composites the body
//! atlas and the hair sheet at runtime from `CharSections.dbc`. The M2 also
//! ships many geosets (cloak, robe, helm, hair variants) of which only the ones
//! the appearance selector picks are drawn. Both decisions are the formats
//! crate's (`CharSections`, `CharacterGeosets`), so this module re-exports them
//! over the FFI without touching any benilla crate.
//!
//! Animation choice is likewise the reference's own: sequences are keyed by
//! `AnimationData.dbc` id through the model's `AnimationLookup`, with the
//! `PlayableAnimationLookup` substitute for ids a model does not author
//! (`0x711960`).

use std::io::Cursor;

use benilla_formats::{CharacterGeosets, EquipGeosets};
use benilla_m2::M2TextureType;

use crate::{cstr_to_str, BnWorld};

/// Read a model's bytes from the MPQ chain (same lookup `bn_model_fill` does).
fn read_model(w: &BnWorld, path: &str) -> Option<Vec<u8>> {
    for a in w.archives.iter().rev() {
        if !a.contains(path) {
            continue;
        }
        if let Ok(bytes) = a.read_file(path) {
            return Some(bytes);
        }
        break;
    }
    None
}

/// Write a NUL-terminated string into `out`.
fn write_cstr(out: *mut u8, cap: u32, s: &str) -> u32 {
    if out.is_null() || cap == 0 {
        return 0;
    }
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    // SAFETY: `out` has room for `cap` bytes and n < cap.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, n);
        *out.add(n) = 0;
    }
    (n + 1) as u32
}

/// The `CharSkinSlot` a texture record names, as the small int the renderer
/// uses: 0 body, 1 hair, 2 object, 3 skin extra (255 = not a character slot).
fn char_slot_of(tex: Option<&benilla_m2::M2Texture>) -> u8 {
    match tex.map(|t| t.texture_type) {
        Some(M2TextureType::Other(1)) => 0, // Body
        Some(M2TextureType::Other(6)) => 1, // Hair
        Some(M2TextureType::Other(2)) => 2, // Object (cape / weapon)
        Some(M2TextureType::Other(8)) => 3, // SkinExtra (tauren fur)
        _ => 255,
    }
}

/// The `CharSections` hair sheet for an appearance (M2 texture type 6). Writes
/// the internal BLP path (NUL-terminated) and returns its byte count including
/// the terminator; 0 when the row is missing.
#[no_mangle]
pub extern "C" fn bn_char_hair_texture(
    world: *mut BnWorld,
    race: u32,
    sex: u32,
    hair_style: u32,
    hair_color: u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let Ok(sections) = benilla_formats::CharSections::load(chain) else {
        return 0;
    };
    // The borrow of `sections` ends here; the path is owned by the tables.
    let path = sections
        .hair_mesh_texture(race as u8, sex as u8, hair_style as u8, hair_color as u8)
        .map(str::to_owned);
    match path {
        Some(p) => write_cstr(out, cap, &p),
        None => 0,
    }
}

/// The uncomposited extra skin (M2 texture type 8, tauren fur). Same contract
/// as [`bn_char_hair_texture`]; only the fur races author it.
#[no_mangle]
pub extern "C" fn bn_char_skin_extra_texture(
    world: *mut BnWorld,
    race: u32,
    sex: u32,
    skin: u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let Ok(sections) = benilla_formats::CharSections::load(chain) else {
        return 0;
    };
    let path = sections
        .skin_extra_texture(race as u8, sex as u8, skin as u8)
        .map(str::to_owned);
    match path {
        Some(p) => write_cstr(out, cap, &p),
        None => 0,
    }
}

/// Per-triangle character texture slot + geoset visibility, in the exact
/// triangle order `bn_model_fill` emits (index lookup applied, triples).
///
/// `out[i]` low three bits = slot (0 body, 1 hair, 2 object, 3 skin extra);
/// bit `0x80` set when the batch's geoset is hidden for this appearance (the
/// naked default look: no equipment). Returns the number of triangles written.
///
/// The visibility set is the reference's `0x477520` selector driven by the
/// naked `EquipGeosets::default()`, matching a freshly created character.
#[no_mangle]
pub extern "C" fn bn_char_model_slots(
    world: *mut BnWorld,
    model_path: *const u8,
    race: u32,
    sex: u32,
    hair_style: u32,
    facial_hair: u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path as *const std::os::raw::c_char) else {
        return 0;
    };
    let w = unsafe { &mut *world };
    let Some(bytes) = read_model(w, p) else {
        return 0;
    };
    let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) else {
        return 0;
    };
    let model = fmt.model();
    let Ok(skin) = model.parse_embedded_skin(bytes.as_slice(), 0) else {
        return 0;
    };
    let tris = skin.triangles();
    let ntri = tris.len() / 3;
    let n = ntri.min(cap as usize);
    if n == 0 {
        return 0;
    }

    let visible = w
        .chain
        .as_mut()
        .and_then(|chain| CharacterGeosets::load(chain).ok())
        .map(|gs| {
            gs.visible_geosets(
                race as u8,
                sex as u8,
                hair_style as u8,
                facial_hair as u8,
                &EquipGeosets::default(),
            )
        });

    // Default: body slot, shown (a batch-less triangle keeps the model's own
    // material, as the reference draws it).
    let mut flags = vec![0u8; tris.len()];
    let sections = skin.submeshes();
    for b in skin.batches() {
        let Some(sec) = sections.get(b.skin_section_index as usize) else {
            continue;
        };
        let start = sec.triangle_start as usize;
        let end = (start + sec.triangle_count as usize).min(flags.len());
        if start >= end {
            continue;
        }
        let tex = model
            .raw_data
            .texture_lookup_table
            .get(b.texture_combo_index as usize)
            .and_then(|&ti| model.textures.get(ti as usize));
        let slot = char_slot_of(tex);
        let hidden = visible
            .as_ref()
            .is_some_and(|v| !v.contains(&sec.id));
        let byte = if hidden { slot | 0x80 } else { slot };
        flags[start..end].fill(byte);
    }

    for i in 0..n {
        unsafe {
            *out.add(i) = flags[i * 3];
        }
    }
    n as u32
}

/// Resolve an `AnimationData.dbc` id to the model's sequence slot, the
/// reference's `0x711960` ownership test plus the `PlayableAnimationLookup`
/// substitute. Returns `None` when the model authors neither.
fn resolve_anim_slot(model: &benilla_m2::M2Model, id: u16) -> Option<u16> {
    if model.owns_animation(id) {
        return Some(model.animation_lookup[id as usize]);
    }
    let sub = model.playable_animation_lookup.get(id as usize)?.resolved_id;
    if model.owns_animation(sub) {
        Some(model.animation_lookup[sub as usize])
    } else {
        None
    }
}

/// Look up a sequence by `AnimationData.dbc` id (0 Stand, 4 Walk, 5 Run,
/// 16 AttackUnarmed, 91 Mount, …).
///
/// `out` receives `[seq_index, start_ms, end_ms, duration_ms, looping]`.
/// Returns 1 on a hit, 0 when the model has no such animation (the caller then
/// keeps its current sequence).
#[no_mangle]
pub extern "C" fn bn_model_anim_by_id(
    world: *const BnWorld,
    model_path: *const std::os::raw::c_char,
    anim_id: u32,
    out: *mut u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    let w = unsafe { &*world };
    let Some(bytes) = read_model(w, p) else {
        return 0;
    };
    let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) else {
        return 0;
    };
    let model = fmt.model();
    // The requested id, else its `PlayableAnimationLookup` substitute. An id
    // the model does not author misses (the caller keeps its current
    // sequence), as the reference's ownership test does.
    let Some(slot) = resolve_anim_slot(model, anim_id as u16) else {
        return 0;
    };
    let anims = benilla_formats::parse_m2_animations(&bytes);
    let Some(a) = anims.iter().find(|a| a.seq_index == slot as usize) else {
        return 0;
    };
    unsafe {
        *out.add(0) = a.seq_index as u32;
        *out.add(1) = a.start_ms;
        *out.add(2) = a.end_ms;
        *out.add(3) = a.end_ms.saturating_sub(a.start_ms);
        *out.add(4) = u32::from(a.looping);
    }
    1
}

/// Every sequence's authored `AnimationData` id, in file order (diagnostics:
/// which clips a model actually carries). Returns the number written.
#[no_mangle]
pub extern "C" fn bn_model_anim_ids(
    world: *const BnWorld,
    model_path: *const std::os::raw::c_char,
    out: *mut u32,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    let w = unsafe { &*world };
    let Some(bytes) = read_model(w, p) else {
        return 0;
    };
    let anims = benilla_formats::parse_m2_animations(&bytes);
    let n = anims.len().min(cap as usize);
    for (i, a) in anims.iter().take(n).enumerate() {
        unsafe {
            *out.add(i) = u32::from(a.anim_id);
        }
    }
    n as u32
}

/// Dump `AnimationData.dbc` id → name rows ("id\tname\n") for the middle-layer
/// constants. Returns bytes written (including the trailing NUL).
#[no_mangle]
pub extern "C" fn bn_anim_names(world: *mut BnWorld, out: *mut u8, cap: u32) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let Ok(cat) = benilla_formats::load_anim_data_catalog(chain) else {
        return 0;
    };
    let mut s = String::new();
    for id in 0..=207u16 {
        if let Some(name) = cat.name(id) {
            s.push_str(&format!("{}\t{}\n", id, name));
        }
    }
    write_cstr(out, cap, &s)
}

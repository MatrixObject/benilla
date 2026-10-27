//! Middle-layer FFI: character appearance and animation selection.
//!
//! Player M2s carry *empty* texture-slot names: the client composites the body
//! atlas and the hair sheet at runtime from `CharSections.dbc`. The M2 also
//! ships many geosets (cloak, robe, helm, hair variants) of which only the ones
//! the appearance selector picks are drawn. Both decisions are the formats
//! crate's (`CharSections`, `CharacterGeosets`), so this module re-exports them
//! over the FFI without touching any benilla crate.
//!
//! Animation choice is likewise the client's own: sequences are keyed by
//! `AnimationData.dbc` id through the model's `AnimationLookup`, with the
//! `PlayableAnimationLookup` substitute for ids a model does not author

use benilla_formats::EquipGeosets;
use benilla_m2::M2TextureType;

use crate::{cstr_to_str, BnWorld};

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
    if !w.ensure_char_sections() {
        return 0;
    }
    let Some(sections) = w.char_sections.as_ref() else {
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
    if !w.ensure_char_sections() {
        return 0;
    }
    let Some(sections) = w.char_sections.as_ref() else {
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
/// The visibility set is the client's selector driven by the
/// naked `EquipGeosets::default`, matching a freshly created character.
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
    char_model_slots(w, p, race, sex, hair_style, facial_hair, None, out, cap)
}

/// Same contract as [`bn_char_model_slots`], but the wearer is dressed: `equip`
/// is the 10 `CreatureDisplayInfoExtra` equipment columns in bodyslot order
/// (e0 helm, e2..e10 body), i.e. exactly what a humanoid NPC wears. The
/// visibility set then follows the client's equipment branches (helm hides
/// hair/ears, robes replace legs, tabard, ...) instead of the naked default.
#[no_mangle]
pub extern "C" fn bn_char_model_slots_gear(
    world: *mut BnWorld,
    model_path: *const u8,
    race: u32,
    sex: u32,
    hair_style: u32,
    facial_hair: u32,
    equip: *const u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() || cap == 0 || equip.is_null() {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path as *const std::os::raw::c_char) else {
        return 0;
    };
    // SAFETY: the caller passes 10 readable u32s.
    let ids: [u32; 10] = unsafe { std::array::from_fn(|i| *equip.add(i)) };
    let w = unsafe { &mut *world };
    // A failed ItemDisplayInfo pass must not hide the body: fall back to the
    // naked geoset set rather than reporting zero visible triangles.
    let eg = equipped_geosets(w, &ids);
    char_model_slots(
        w,
        p,
        race,
        sex,
        hair_style,
        facial_hair,
        eg.as_ref(),
        out,
        cap,
    )
}

/// The client's worn-equipment geoset selectors for an NPC's 10 equipment
/// columns (`EquipGeosets`), read off `ItemDisplayInfo`.
fn equipped_geosets(w: &mut BnWorld, ids: &[u32; 10]) -> Option<EquipGeosets> {
    if !w.ensure_item_display() {
        return None;
    }
    let cat = w.item_display.as_ref()?;
    let worn: [Option<&benilla_formats::ItemDisplay>; 8] =
        std::array::from_fn(|i| cat.get(ids[2 + i]));
    let mut eg = EquipGeosets::default();
    for (i, d) in worn.iter().enumerate() {
        if let Some(d) = d {
            eg.bodyslots[i] = Some(d.geoset_groups);
        }
    }
    // The head slot's HelmetGeosetVisData pair (split by sex in the selector).
    eg.helm_vis = cat.get(ids[0]).and_then(|d| d.worn_helm_vis());
    // Branch B3's gate: the shirt cuffs, as the client reads it.
    eg.forearm_dressed = benilla_formats::forearm_dressed(&worn);
    Some(eg)
}

fn char_model_slots(
    w: &mut BnWorld,
    p: &str,
    race: u32,
    sex: u32,
    hair_style: u32,
    facial_hair: u32,
    equip: Option<&EquipGeosets>,
    out: *mut u8,
    cap: u32,
) -> u32 {
    let Some(cached) = crate::m2_cached(&w.archives, p) else {
        return 0;
    };
    let model = cached.format.model();
    let Some(skin) = cached.skin.as_ref() else {
        return 0;
    };
    // One entry per triangle bn_model_fill emits, in the same order: a section
    // covered by several batches is drawn once per batch, so each copy carries
    // its own batch's texture slot (the plaza's ground and its shadow decal).
    let emit = crate::m2_emit_tris(model, skin);
    let n = emit.len().min(cap as usize);
    if n == 0 {
        return 0;
    }

    let naked = EquipGeosets::default();
    w.ensure_char_geosets();
    let visible = w.char_geosets.as_ref().map(|gs| {
        gs.visible_geosets(
            race as u8,
            sex as u8,
            hair_style as u8,
            facial_hair as u8,
            equip.unwrap_or(&naked),
        )
    });

    // Character body slots exist ONLY for a character model. On any other model
    // (`Interface\Glues\Models\UI_Human\UI_Human.m2` has 28 textures) the "slot"
    // must be the TEXTURE INDEX, so every batch reaches its own BLP.
    let is_char = p.to_ascii_lowercase().contains("character\\");
    let batches = skin.batches();
    let sections = skin.submeshes();

    for (i, t) in emit.iter().take(n).enumerate() {
        let b = &batches[t.batch as usize];
        let Some(sec) = sections.get(b.skin_section_index as usize) else {
            continue;
        };
        let tex_index = model
            .raw_data
            .texture_lookup_table
            .get(b.texture_combo_index as usize)
            .copied();
        let tex = tex_index.and_then(|ti| model.textures.get(ti as usize));
        let slot = if is_char {
            char_slot_of(tex)
        } else {
            // Low 7 bits = texture index; bit 7 stays clear (a non-character model
            // is never geoset-hidden).
            tex_index.unwrap_or(0).min(0x7f) as u8
        };
        let hidden = is_char && visible.as_ref().is_some_and(|v| !v.contains(&sec.id));
        let byte = if hidden { slot | 0x80 } else { slot & 0x7f };
        unsafe {
            *out.add(i) = byte;
        }
    }
    n as u32
}

/// Resolve an `AnimationData.dbc` id to the model's sequence slot, the
/// client's ownership test plus the `PlayableAnimationLookup`
/// substitute. Returns `None` when the model authors neither.
fn resolve_anim_slot(model: &benilla_m2::M2Model, id: u16) -> Option<u16> {
    if model.owns_animation(id) {
        return Some(model.animation_lookup[id as usize]);
    }
    let sub = model
        .playable_animation_lookup
        .get(id as usize)?
        .resolved_id;
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
    let Some(cached) = crate::m2_cached(&w.archives, p) else {
        return 0;
    };
    let model = cached.format.model();
    // The requested id, else its `PlayableAnimationLookup` substitute. An id
    // the model does not author misses (the caller keeps its current
    // sequence), as the client's ownership test does.
    let Some(slot) = resolve_anim_slot(model, anim_id as u16) else {
        return 0;
    };
    let anims = benilla_formats::parse_m2_animations(&cached.bytes);
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
    let Some(cached) = crate::m2_cached(&w.archives, p) else {
        return 0;
    };
    let anims = benilla_formats::parse_m2_animations(&cached.bytes);
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

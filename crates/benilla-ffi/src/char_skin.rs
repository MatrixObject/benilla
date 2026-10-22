//! Middle-layer FFI: a composited character body-skin atlas as raw RGBA8.
//!
//! The M2 of a player character carries *empty* texture-slot names: the body
//! skin is composited at runtime from `CharSections.dbc` plus the character
//! BLPs. The official compositor lives in `benilla-app` (Bevy), which the FFI
//! boundary deliberately excludes, so this file runs the formats-level
//! compositor (`benilla_formats::CharSections::composite_body`) directly over
//! the checked-out `Chain`.

use std::ffi::c_char;

use crate::BnWorld;

/// Composite the default look for `(race, sex)` and write level-0 RGBA8 into
/// `out` (capacity `cap` bytes). Writes the atlas size through `out_w`/`out_h`.
/// Returns 1 on success, 0 on any miss (no chain, no CharSections row, or the
/// atlas does not fit).
///
/// The dials are the 1.12 defaults (skin 0, face 0, no facial hair, hair style
/// 0, hair colour 0) and the body wears no equipment: a naked composite, which
/// is what a freshly spawned character wears.
#[no_mangle]
pub extern "C" fn bn_char_skin_rgba(
    world: *mut BnWorld,
    race: u32,
    sex: u32,
    skin: u32,
    face: u32,
    facial_hair: u32,
    hair_style: u32,
    hair_color: u32,
    out: *mut u8,
    cap: u32,
    out_w: *mut u32,
    out_h: *mut u32,
) -> u32 {
    if world.is_null() || out.is_null() || out_w.is_null() || out_h.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: caller guarantees a valid, exclusive handle.
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 10;
    };
    let Ok(sections) = benilla_formats::CharSections::load(chain) else {
        return 11;
    };
    let look = sections.composite_body(
        chain,
        race as u8,
        sex as u8,
        skin as u8,
        face as u8,
        facial_hair as u8,
        hair_style as u8,
        hair_color as u8,
        [None; 8],
        None,
        false,
    );
    let atlas = match look {
        Ok(Some(a)) => a,
        Ok(None) => return 13,
        Err(_) => return 12,
    };
    let atlas = atlas.into_rgba8();
    let (aw, ah) = (atlas.width, atlas.height);
    // Report the size even if it does not fit: the caller logs it, which makes
    // an undersized staging buffer obvious instead of a silent black body.
    unsafe {
        *out_w = aw;
        *out_h = ah;
    }
    let need = (aw as usize) * (ah as usize) * 4;
    if aw == 0 || ah == 0 {
        return 2;
    }
    if atlas.mips.is_empty() {
        return 3;
    }
    if need > cap as usize {
        return 4;
    }
    let Some(level0) = atlas.mips.first() else {
        return 3;
    };
    if level0.len() < need {
        return 5;
    }
    // SAFETY: capacity checked against the atlas size above.
    unsafe {
        std::ptr::copy_nonoverlapping(level0.as_ptr(), out as *mut u8, need);
    }
    1
}

/// Dressed variant of [`bn_char_skin_rgba`]: items are `ItemDisplayInfo` ids in
/// the body-slot order the compositor uses (`equip_blits`): shirt, chest, belt,
/// pants, boots, wrist, gloves, tabard. `displays` points at 8 `u32` (0 = bare).
/// The equipment textures are composited exactly as the reference client's
/// `0x477...` blitters do, so a shirt/pants/boots change the body atlas without
/// touching the M2 or its geosets.
#[no_mangle]
pub extern "C" fn bn_char_skin_dressed(
    world: *mut BnWorld,
    race: u32,
    sex: u32,
    skin: u32,
    face: u32,
    facial_hair: u32,
    hair_style: u32,
    hair_color: u32,
    displays: *const u32,
    out: *mut u8,
    cap: u32,
    out_w: *mut u32,
    out_h: *mut u32,
) -> u32 {
    if world.is_null() || out.is_null() || out_w.is_null() || out_h.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: caller passes 8 ids (see the UE declaration).
    let ids: [u32; 8] = if displays.is_null() {
        [0; 8]
    } else {
        unsafe { *(displays as *const [u32; 8]) }
    };
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 10;
    };
    let Ok(sections) = benilla_formats::CharSections::load(chain) else {
        return 11;
    };
    let catalog = match benilla_formats::load_item_display_catalog(chain) {
        Ok(c) => c,
        Err(_) => return 14,
    };
    let equipment: [Option<&benilla_formats::ItemDisplay>; 8] =
        std::array::from_fn(|i| if ids[i] == 0 { None } else { catalog.get(ids[i]) });
    let look = sections.composite_body(
        chain,
        race as u8,
        sex as u8,
        skin as u8,
        face as u8,
        facial_hair as u8,
        hair_style as u8,
        hair_color as u8,
        equipment,
        None,
        false,
    );
    let atlas = match look {
        Ok(Some(a)) => a,
        Ok(None) => return 13,
        Err(_) => return 12,
    };
    let atlas = atlas.into_rgba8();
    let (aw, ah) = (atlas.width, atlas.height);
    unsafe {
        *out_w = aw;
        *out_h = ah;
    }
    let need = (aw as usize) * (ah as usize) * 4;
    if aw == 0 || ah == 0 {
        return 2;
    }
    if atlas.mips.is_empty() {
        return 3;
    }
    if need > cap as usize {
        return 4;
    }
    let Some(level0) = atlas.mips.first() else {
        return 3;
    };
    if level0.len() < need {
        return 5;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(level0.as_ptr(), out as *mut u8, need);
    }
    1
}

/// Collect the display ids in `[from, to]` that author a texture for one body
/// region layer (0 ArmUpper, 1 ArmLower, 2 Hand, 3 TorsoUpper, 4 TorsoLower,
/// 5 LegUpper, 6 LegLower, 7 Foot). Writes up to `cap` ids to `out` and returns
/// the number written. Used by the host's gear picker: a chest that covers the
/// torso is any id with layer 3 set.
#[no_mangle]
pub extern "C" fn bn_item_display_scan(
    world: *mut BnWorld,
    layer: u32,
    from: u32,
    to: u32,
    out: *mut u32,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 || layer > 7 {
        return 0;
    }
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let Ok(catalog) = benilla_formats::load_item_display_catalog(chain) else {
        return 0;
    };
    let mut n = 0u32;
    let mut id = from;
    while id <= to && n < cap {
        if let Some(d) = catalog.get(id) {
            if d.region_textures[layer as usize]
                .as_deref()
                .is_some_and(|t| !t.is_empty())
            {
                unsafe { *out.add(n as usize) = id };
                n += 1;
            }
        }
        id += 1;
    }
    n
}

/// Dump one `ItemDisplayInfo` row as text (models, region textures, geoset
/// groups), for the host's gear-picking console. Returns the byte count.
#[no_mangle]
pub extern "C" fn bn_item_display_info(
    world: *mut BnWorld,
    id: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let Ok(catalog) = benilla_formats::load_item_display_catalog(chain) else {
        return 0;
    };
    let Some(d) = catalog.get(id) else {
        return 0;
    };
    let mut line = format!(
        "{}|model={}|{}|modeltex={}|{}|geoset={}|{}|{}",
        id,
        d.model[0].as_deref().unwrap_or("-"),
        d.model[1].as_deref().unwrap_or("-"),
        d.model_texture[0].as_deref().unwrap_or("-"),
        d.model_texture[1].as_deref().unwrap_or("-"),
        d.geoset_groups[0],
        d.geoset_groups[1],
        d.geoset_groups[2],
    );
    for t in &d.region_textures {
        line.push('|');
        line.push_str(t.as_deref().unwrap_or("-"));
    }
    let bytes = line.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, n);
        *buf.add(n) = 0;
    }
    n as u32
}

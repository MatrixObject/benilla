//! Middle-layer FFI: a composited character body-skin atlas as raw RGBA8.
//!
//! The M2 of a player character carries *empty* texture-slot names: the body
//! skin is composited at runtime from `CharSections.dbc` plus the character
//! BLPs. The official compositor lives in `benilla-app` (Bevy), which the FFI
//! boundary deliberately excludes, so this file runs the formats-level
//! compositor (`benilla_formats::CharSections::composite_body`) directly over
//! the checked-out `Chain`.

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

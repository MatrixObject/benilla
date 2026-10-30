//! Middle-layer FFI: the worn helm model and the cloak skin, which the body model
//! does not carry.
//!
//! The reference's head-slot handler (`0x4799a0`) cuts the display row's model name at
//! its last dot (`0x479a08`) and loads `Item\ObjectComponents\Head\<name>_<race><sex>.mdx`
//! (`0x479a41`) with attach id 11 (`0x479aae`); the object skin is
//! `Item\ObjectComponents\Head\<model texture>.blp` (`0x479a82`). The cloak builder
//! (`0x479b40`) instead binds `Item\ObjectComponents\Cape\<model texture>.blp` as
//! replaceable type 2 on the character model itself, and its geoset group selects the
//! cape geoset that `bn_char_model_slots_gear` already reports.
//!
//! The shoulder-slot handler (`0x479d10`) attaches the row's two models, `model[0]`
//! (its `LShoulder…` file) at attachment 6 and `model[1]` (the `RShoulder…` file) at
//! attachment 5, each skinned by its own `Item\ObjectComponents\Shoulder\<texture>.blp`.
//!
//! `bn_unit_helm_model(world, display, race, sex, out_model, model_cap, out_texture,
//! texture_cap)` returns 1 when there is a helm model to attach (the host hangs it at
//! attachment 11) and writes the `.m2` path plus the skin BLP; `bn_unit_cape_texture`
//! writes the cape BLP for the body's type-2 batches; `bn_unit_shoulder_model` writes
//! the left and right shoulder pairs and returns how many sides it resolved.

use std::ffi::c_char;

use crate::BnWorld;

/// Race 1-8's helm suffix, `Hu Or Dw Ni Sc Ta Gn Tr`, in race id order; sex then
/// appends `M` or `F`. The archives hold exactly these eight spellings.
const HELM_RACE_PREFIX: [&str; 8] = ["Hu", "Or", "Dw", "Ni", "Sc", "Ta", "Gn", "Tr"];

/// `Item\ObjectComponents\Head\<stem>_<race><sex>.m2`, or `None` when the row names no
/// model, the race is outside 1-8 or the sex is neither 0 nor 1.
fn helm_model(model: &str, race: u32, sex: u32) -> Option<String> {
    let prefix = HELM_RACE_PREFIX.get(race.checked_sub(1)? as usize)?;
    let letter = match sex {
        0 => "M",
        1 => "F",
        _ => return None,
    };
    let stem = model.strip_suffix(".m2").unwrap_or(model);
    (!stem.is_empty()).then(|| format!("Item\\ObjectComponents\\Head\\{stem}_{prefix}{letter}.m2"))
}

/// The helm's object skin, `Item\ObjectComponents\Head\<texture>.blp`.
fn helm_texture(texture: &str) -> String {
    format!("Item\\ObjectComponents\\Head\\{texture}.blp")
}

/// The cloak's skin, `Item\ObjectComponents\Cape\<texture>.blp` (`0x479b40`).
fn cape_texture(texture: &str) -> String {
    format!("Item\\ObjectComponents\\Cape\\{texture}.blp")
}

/// `Item\ObjectComponents\Shoulder\<stem>.m2` (`0x479d10`). The DBC writes the
/// shoulder models as `.mdx`; the archives carry the same stem as `.m2`.
fn shoulder_model(model: &str) -> Option<String> {
    let stem = model
        .strip_suffix(".mdx")
        .or_else(|| model.strip_suffix(".m2"))
        .unwrap_or(model);
    (!stem.is_empty()).then(|| format!("Item\\ObjectComponents\\Shoulder\\{stem}.m2"))
}

/// A shoulder's object skin, `Item\ObjectComponents\Shoulder\<texture>.blp`.
fn shoulder_texture(texture: &str) -> String {
    format!("Item\\ObjectComponents\\Shoulder\\{texture}.blp")
}

fn copy_out(text: &str, out: *mut c_char, cap: u32) -> u32 {
    if out.is_null() || cap == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    // SAFETY: capacity checked above; the caller owns `cap` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, n);
        *out.add(n) = 0;
    }
    n as u32
}

#[no_mangle]
pub extern "C" fn bn_unit_helm_model(
    world: *mut BnWorld,
    display_id: u32,
    race: u32,
    sex: u32,
    out_model: *mut c_char,
    model_cap: u32,
    out_texture: *mut c_char,
    texture_cap: u32,
) -> u32 {
    if world.is_null() || display_id == 0 {
        return 0;
    }
    // SAFETY: caller passes an exclusive, valid handle.
    let w = unsafe { &mut *world };
    if !w.ensure_item_display() {
        return 0;
    }
    let Some(row) = w.item_display.as_ref().and_then(|c| c.get(display_id)) else {
        return 0;
    };
    let Some(model) = row.model[0].as_deref() else {
        return 0;
    };
    let Some(path) = helm_model(model, race, sex) else {
        return 0;
    };
    copy_out(&path, out_model, model_cap);
    if let Some(tex) = row.model_texture[0].as_deref() {
        copy_out(&helm_texture(tex), out_texture, texture_cap);
    }
    1
}

#[no_mangle]
pub extern "C" fn bn_unit_cape_texture(
    world: *mut BnWorld,
    display_id: u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || display_id == 0 {
        return 0;
    }
    // SAFETY: caller passes an exclusive, valid handle.
    let w = unsafe { &mut *world };
    if !w.ensure_item_display() {
        return 0;
    }
    let Some(row) = w.item_display.as_ref().and_then(|c| c.get(display_id)) else {
        return 0;
    };
    let Some(tex) = row.model_texture[0].as_deref() else {
        return 0;
    };
    copy_out(&cape_texture(tex), out, cap);
    1
}

/// The two worn shoulder models and their skins: `model[0]` is the left pauldron
/// (attachment 6) and `model[1]` the right (attachment 5). Returns the number of
/// sides resolved, 0 to 2; a side the row leaves empty writes nothing.
#[no_mangle]
pub extern "C" fn bn_unit_shoulder_model(
    world: *mut BnWorld,
    display_id: u32,
    out_left_model: *mut c_char,
    left_model_cap: u32,
    out_left_texture: *mut c_char,
    left_texture_cap: u32,
    out_right_model: *mut c_char,
    right_model_cap: u32,
    out_right_texture: *mut c_char,
    right_texture_cap: u32,
) -> u32 {
    if world.is_null() || display_id == 0 {
        return 0;
    }
    // SAFETY: caller passes an exclusive, valid handle.
    let w = unsafe { &mut *world };
    if !w.ensure_item_display() {
        return 0;
    }
    let Some(row) = w.item_display.as_ref().and_then(|c| c.get(display_id)) else {
        return 0;
    };
    let mut sides = 0;
    let outs = [
        (
            out_left_model,
            left_model_cap,
            out_left_texture,
            left_texture_cap,
        ),
        (
            out_right_model,
            right_model_cap,
            out_right_texture,
            right_texture_cap,
        ),
    ];
    for (side, (model_out, model_cap, tex_out, tex_cap)) in outs.into_iter().enumerate() {
        let Some(model) = row.model[side].as_deref().and_then(shoulder_model) else {
            continue;
        };
        copy_out(&model, model_out, model_cap);
        if let Some(tex) = row.model_texture[side].as_deref() {
            copy_out(&shoulder_texture(tex), tex_out, tex_cap);
        }
        sides += 1;
    }
    sides as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display 15810 is `Helm_Mail_A_02`: the human-male helm file and its gold skin,
    /// both read back through the chain so the spelling resolves, not just looks right.
    #[test]
    fn a_human_male_helm_resolves_to_its_race_file() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let cat = benilla_formats::load_item_display_catalog(&mut chain).expect("catalog");
        let row = cat.get(15810).expect("row");
        let model = row.model[0].as_deref().expect("model");
        let path = helm_model(model, 1, 0).expect("helm path");
        let tex = helm_texture(row.model_texture[0].as_deref().expect("texture"));
        assert_eq!(path, "Item\\ObjectComponents\\Head\\helm_mail_a_02_HuM.m2");
        assert_eq!(tex, "Item\\ObjectComponents\\Head\\Helm_Mail_A_02Gold.blp");
        chain.read_file(&path).expect("helm model in the archives");
        chain.read_file(&tex).expect("helm skin in the archives");
    }

    /// A race outside 1-8 or a sex outside 0-1 has no helm file, as the reference's
    /// range gates (`0x4799cc`, `0x4799d8`) leave without attaching one.
    #[test]
    fn an_out_of_range_race_or_sex_names_no_helm() {
        assert!(helm_model("helm_mail_a_02.m2", 0, 0).is_none());
        assert!(helm_model("helm_mail_a_02.m2", 9, 0).is_none());
        assert!(helm_model("helm_mail_a_02.m2", 1, 2).is_none());
        assert!(helm_model("", 1, 0).is_none());
    }

    /// Display 28614 is `Cape_Leather_A_02Brown`: the cape BLP exists in the archives.
    #[test]
    fn a_cloak_display_resolves_to_its_cape_skin() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let cat = benilla_formats::load_item_display_catalog(&mut chain).expect("catalog");
        let row = cat.get(28614).expect("row");
        let tex = cape_texture(row.model_texture[0].as_deref().expect("texture"));
        assert_eq!(
            tex,
            "Item\\ObjectComponents\\Cape\\Cape_Leather_A_02Brown.blp"
        );
        chain.read_file(&tex).expect("cape skin in the archives");
    }

    /// Display 17197 is the red mail pauldron pair: `LShoulder…` is `model[0]` and
    /// `RShoulder…` `model[1]`, both `.mdx` in the row and both present as `.m2`.
    #[test]
    fn a_pauldron_pair_resolves_to_its_left_and_right_models() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let cat = benilla_formats::load_item_display_catalog(&mut chain).expect("catalog");
        let row = cat.get(17197).expect("row");
        let left = shoulder_model(row.model[0].as_deref().expect("left model")).expect("left path");
        let right =
            shoulder_model(row.model[1].as_deref().expect("right model")).expect("right path");
        let tex = shoulder_texture(row.model_texture[0].as_deref().expect("texture"));
        assert_eq!(
            left,
            "Item\\ObjectComponents\\Shoulder\\LShoulder_Mail_B_01.m2"
        );
        assert_eq!(
            right,
            "Item\\ObjectComponents\\Shoulder\\RShoulder_Mail_B_01.m2"
        );
        assert_eq!(
            tex,
            "Item\\ObjectComponents\\Shoulder\\Shoulder_Mail_B_01Red.blp"
        );
        chain
            .read_file(&left)
            .expect("left pauldron in the archives");
        chain
            .read_file(&right)
            .expect("right pauldron in the archives");
        chain
            .read_file(&tex)
            .expect("pauldron skin in the archives");
    }

    /// A name without an `.mdx`/`.m2` suffix or an empty one names no file.
    #[test]
    fn a_shoulder_name_must_stem_a_model() {
        assert_eq!(
            shoulder_model("LShoulder_Mail_B_01.mdx").as_deref(),
            Some("Item\\ObjectComponents\\Shoulder\\LShoulder_Mail_B_01.m2")
        );
        assert_eq!(shoulder_model("").as_deref(), None);
    }
}

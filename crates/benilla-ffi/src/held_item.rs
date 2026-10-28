//! Middle-layer FFI: a unit's held-item slots to the model the UE host draws.
//!
//! A creature's weapons never ride its body model: the server sends
//! `UNIT_VIRTUAL_ITEM_SLOT_DISPLAY` (an `ItemDisplayInfo` id per hand) and the
//! client resolves the item's model and object skin from that row and hangs
//! them off the hand attachment (`0x47a0c0`, `0x47a200`). benilla does this
//! inside the app crate, which the FFI boundary excludes, so this module runs
//! the formats-level `ItemDisplayCatalog` and mirrors the app's `placement`
//! rule for a creature's sheath byte.
//!
//! `bn_unit_held_item(world, display, inventory_type, slot, sheath, out_model,
//! model_cap, out_texture, texture_cap)` returns `attach_id + 1` (0 = nothing
//! to draw) and writes the `.m2` path plus the object-skin BLP basename.

use std::ffi::c_char;

use crate::BnWorld;

/// M2 attachment ids, as benilla-app's `equipment::attach_id` names them.
mod attach_id {
    pub(super) const SHIELD: u16 = 0;
    pub(super) const HAND_RIGHT: u16 = 1;
    pub(super) const HAND_LEFT: u16 = 2;
    pub(super) const BACK_RIGHT: u16 = 26;
    pub(super) const BACK_LEFT: u16 = 27;
    pub(super) const SHIELD_BACK: u16 = 28;
    pub(super) const BACK_LOWER_MAIN: u16 = 30;
    pub(super) const BACK_LOWER_OFF: u16 = 31;
    pub(super) const HIP_MAIN: u16 = 32;
    pub(super) const HIP_OFF: u16 = 33;
}

/// The attach point a slot's item hangs from, the app's `placement`
/// (`0x47a070`/`0x611e10`) over a creature's sheath byte and the item's own.
fn placement(slot: u32, inv_type: u32, item_sheath: u8, unit_sheath: u8) -> Option<u16> {
    use attach_id::*;
    let shield = inv_type == 14; // INVTYPE_SHIELD
    match slot {
        2 => (unit_sheath == 2).then_some(if inv_type == 15 {
            HAND_LEFT // INVTYPE_RANGED: bows
        } else {
            HAND_RIGHT
        }),
        0 | 1 if unit_sheath == 1 => Some(match (slot, shield) {
            (0, _) => HAND_RIGHT,
            (_, true) => SHIELD,
            (_, false) => HAND_LEFT,
        }),
        0 | 1 => match item_sheath {
            1 => Some(if slot == 0 { BACK_RIGHT } else { BACK_LEFT }),
            2 => Some(if slot == 0 { BACK_LOWER_MAIN } else { BACK_LOWER_OFF }),
            3 => Some(if slot == 0 { HIP_MAIN } else { HIP_OFF }),
            4 => Some(SHIELD_BACK),
            _ => None,
        },
        _ => None,
    }
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
pub extern "C" fn bn_unit_held_item(
    world: *mut BnWorld,
    display_id: u32,
    inventory_type: u32,
    slot: u32,
    sheath: u32,
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
    let Some(attach) = placement(slot, inventory_type, sheath as u8, 0) else {
        return 0;
    };
    let dir = if inventory_type == 14 { "Shield" } else { "Weapon" };
    let Some(model) = row.model[0].as_deref() else {
        return attach as u32 + 1;
    };
    let path = format!("Item\\ObjectComponents\\{dir}\\{model}");
    copy_out(&path, out_model, model_cap);
    if let Some(tex) = row.model_texture[0].as_deref() {
        let blp = format!("Item\\ObjectComponents\\{dir}\\{tex}.blp");
        copy_out(&blp, out_texture, texture_cap);
    }
    attach as u32 + 1
}

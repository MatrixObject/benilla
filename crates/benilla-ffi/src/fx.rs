//! Middle-layer FFI: the two hardcoded particle models the host hangs on units.
//!
//! The melee spurt resolves through three tables the app crate owns in the reference client:
//! `CreatureDisplayInfo`/`CreatureModelData` name a `UnitBloodLevels` row, whose `UnitBlood`
//! entry at violence 2 names a `SpellVisualEffectName` model (`0x624530` → `0x625010`). The
//! corpse sparkle is the hardcoded "HARDCODED Loot Art" effect armed while `UNIT_DYNAMIC_FLAGS`
//! bit 0 is set (`0x5d6e30`, `0x600640`). Both ship as `.mdl`, so the path is normalised to `.m2`
//! the way [`crate::creature::m2_path`] does for bodies.
//!
//! bn_blood_effect(world, display_id, front, large, buf, cap) -> bytes written (0 = miss)
//! bn_loot_art(world, buf, cap) -> bytes written (0 = miss)

use std::ffi::c_char;
use std::sync::{Arc, Mutex, OnceLock};

use crate::creature::{copy_out, m2_path};
use crate::BnWorld;

/// The gore level, the client's `violenceLevel`: 2 for enUS, as `creature_anim::blood` fixes it.
const VIOLENCE_LEVEL: usize = 2;

type Bloods = benilla_formats::BloodCatalog;
type Visuals = benilla_formats::SpellVisualCatalog;

/// Both tables cost a pass over several DBCs, so keep the last Data dir's parse, as
/// [`crate::ghost`] does for the visual catalog.
fn blood_catalog(world: &mut BnWorld) -> Option<Arc<Bloods>> {
    static CAT: OnceLock<Mutex<Option<(String, Arc<Bloods>)>>> = OnceLock::new();
    let cell = CAT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, cat)) = guard.as_ref() {
        if *dir == world.data_dir {
            return Some(Arc::clone(cat));
        }
    }
    let chain = world.chain.as_mut()?;
    let cat = Arc::new(benilla_formats::load_blood_catalog(chain).ok()?);
    *guard = Some((world.data_dir.clone(), Arc::clone(&cat)));
    Some(cat)
}

fn visual_catalog(world: &mut BnWorld) -> Option<Arc<Visuals>> {
    static CAT: OnceLock<Mutex<Option<(String, Arc<Visuals>)>>> = OnceLock::new();
    let cell = CAT.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, cat)) = guard.as_ref() {
        if *dir == world.data_dir {
            return Some(Arc::clone(cat));
        }
    }
    let chain = world.chain.as_mut()?;
    let cat = Arc::new(benilla_formats::load_spell_visual_catalog(chain).ok()?);
    *guard = Some((world.data_dir.clone(), Arc::clone(&cat)));
    Some(cat)
}

/// The melee spurt's model for a victim display, front or back and small or crushing. The
/// three-tier blood resolve (`0x60afb0`) then the censored `UnitBlood` row, at violence 2.
#[no_mangle]
pub extern "C" fn bn_blood_effect(
    world: *mut BnWorld,
    display_id: u32,
    front: u32,
    large: u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    let Some(creature) = crate::creature::catalog(w) else {
        return 0;
    };
    let Some(model) = creature.model(display_id) else {
        return 0;
    };
    let Some(bloods) = blood_catalog(w) else {
        return 0;
    };
    let blood_id = bloods.level_key(model.blood_display, model.blood_model);
    let Some(effect) =
        blood_id.and_then(|id| bloods.effect_id(id as i32, VIOLENCE_LEVEL, front != 0, large != 0))
    else {
        return 0;
    };
    let Some(visuals) = visual_catalog(w) else {
        return 0;
    };
    let Some(path) = visuals.effect_path(effect) else {
        return 0;
    };
    let path = m2_path(path);
    copy_out(&path, out, cap)
}

/// The corpse sparkle's model, the hardcoded "HARDCODED Loot Art" effect.
#[no_mangle]
pub extern "C" fn bn_loot_art(world: *mut BnWorld, out: *mut c_char, cap: u32) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    let Some(visuals) = visual_catalog(w) else {
        return 0;
    };
    let Some((_, path)) = visuals.loot_art_effect() else {
        return 0;
    };
    let path = m2_path(path);
    copy_out(&path, out, cap)
}

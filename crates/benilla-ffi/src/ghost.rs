//! Middle-layer FFI: the ghost's body look.
//!
//! A released spirit's body is drawn through the reference's own state kit: `Spell.dbc`
//! 8326 ("Ghost") names its `SpellVisual.dbc` row, whose state kit carries CharProc 14
//! (the body alpha factor) and CharProc 1 (the packed `0x00RRGGBB` body tint). Those are
//! the nodes `benilla-app`'s `aura_visual` installs while the aura lives; the host has no
//! aura pipeline of its own, so `bn_ghost_look` hands it the two values to bake into its
//! body atlas. The full-screen look is FFXDeath (`ffx_glow.wgsl`), gated on
//! `PLAYER_FLAGS & 0x10` ([`crate::net::BnNetPlayer::ghost`]), not on this spell.
//!
//! bn_ghost_look(world, out_alpha, out_tint) -> 1 when the tables answer, 0 when the
//! spell, its visual or the kit is missing.

use std::sync::{Arc, Mutex, OnceLock};

use crate::BnWorld;

/// `Spell.dbc` "Ghost", the aura a released spirit carries.
const GHOST_SPELL: u32 = 8326;

/// CharProc 14 installs the alpha node (`0x60d972`); CharProc 1 the tint (`0x60d840`).
const PROC_ALPHA: i32 = 14;
const PROC_TINT: i32 = 1;

type Catalog = benilla_formats::SpellVisualCatalog;

/// Parsing every visual table costs tens of ms, so keep the last Data dir's catalog.
fn catalog(world: &mut BnWorld) -> Option<Arc<Catalog>> {
    static CAT: OnceLock<Mutex<Option<(String, Arc<Catalog>)>>> = OnceLock::new();
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

/// The ghost body's `(alpha factor, packed tint)` from spell 8326's state kit.
#[no_mangle]
pub extern "C" fn bn_ghost_look(
    world: *mut BnWorld,
    out_alpha: *mut f32,
    out_tint: *mut u32,
) -> u32 {
    if world.is_null() || out_alpha.is_null() || out_tint.is_null() {
        return 0;
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    if w.spells.is_none() {
        w.spells = w
            .chain
            .as_mut()
            .and_then(|c| benilla_formats::load_spell_catalog(c).ok());
    }
    let Some(visual) = w
        .spells
        .as_ref()
        .and_then(|c| c.get(GHOST_SPELL))
        .map(|s| s.visual)
    else {
        return 0;
    };
    let Some(cat) = catalog(w) else {
        return 0;
    };
    let Some(stages) = cat.stages(visual) else {
        return 0;
    };
    let Some(kit) = cat.kit(stages.state) else {
        return 0;
    };
    let Some(alpha) = kit.char_proc_param(PROC_ALPHA) else {
        return 0;
    };
    let tint = kit
        .char_proc_param(PROC_TINT)
        .map_or(0xFF_FF_FF, |v| v as u32);
    // SAFETY: both out pointers are non-null and writable by contract.
    unsafe {
        *out_alpha = alpha;
        *out_tint = tint;
    }
    1
}

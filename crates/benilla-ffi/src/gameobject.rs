//! Middle-layer FFI: `GameObjectDisplayInfo` -> the model the UE host draws.
//!
//! A looted drop, a herb, a mailbox: the server sends only a `GAMEOBJECT_DISPLAYID`
//! and the client resolves it through `GameObjectDisplayInfo.dbc` straight to a
//! model path, with no `CreatureModelData` indirection and no skin list. This
//! module runs the formats-level catalog over the chain the handle already holds,
//! the way [`crate::creature`] does for a creature display.
//!
//! bn_gameobject_model(world, display_id, buf, cap) -> bytes written (0 = miss)
//!
//! The path keeps its authored extension: `.m2` for the models the host can draw
//! (`.mdx`/`.mdl` are normalised, as a creature's are) and `.wmo` for the handful
//! of building-sized objects, which the host skips rather than guesses at.

use std::ffi::c_char;
use std::sync::{Arc, Mutex, OnceLock};

use crate::creature::m2_path;
use crate::BnWorld;

type Catalog = benilla_formats::GameObjectCatalog;

/// The catalog costs one DBC pass, so keep the last Data dir's parse; a handle
/// opens one Data dir, so in practice this loads once per session.
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
    let cat = Arc::new(benilla_formats::load_gameobject_catalog(chain).ok()?);
    *guard = Some((world.data_dir.clone(), Arc::clone(&cat)));
    Some(cat)
}

/// `id|model=<path>` for one game object display, or 0 when the DBC has no row.
#[no_mangle]
pub extern "C" fn bn_gameobject_model(
    world: *mut BnWorld,
    display_id: u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: caller passes an exclusive, valid handle.
    let w = unsafe { &mut *world };
    let Some(cat) = catalog(w) else {
        return 0;
    };
    let Some(path) = cat.model_path(display_id) else {
        return 0;
    };
    let lower = path.to_ascii_lowercase();
    let model = if lower.ends_with(".wmo") {
        path.to_string()
    } else {
        m2_path(path)
    };
    let line = format!("{display_id}|model={model}");
    let bytes = line.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    // SAFETY: capacity checked above; the caller owns `cap` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, n);
        *out.add(n) = 0;
    }
    n as u32
}

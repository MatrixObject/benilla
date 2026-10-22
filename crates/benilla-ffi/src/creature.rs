//! Middle-layer FFI: `CreatureDisplayInfo` -> render inputs for the UE host.
//!
//! A creature's look never crosses the wire: the server sends a display id and
//! the client resolves it through `CreatureDisplayInfo` + `CreatureModelData`
//! (+ `CreatureDisplayInfoExtra` for a humanoid NPC). The reference resolves
//! these inside `benilla-app`, which the FFI boundary excludes, so this module
//! runs the formats-level `CreatureCatalog` over the chain the handle already
//! holds and hands the host a plain text line it can parse:
//!
//!   bn_creature_display_info(world, id, buf, cap) -> bytes written (0 = miss)
//!   bn_creature_scan(world, needle, from, to, buf, cap) -> bytes written
//!
//! The model path is normalised to `.m2` (`CreatureModelData` authors `.mdx`)
//! because the host feeds it straight back into `bn_model_fill`, which reads
//! exact archive paths.

use std::ffi::c_char;
use std::sync::{Arc, Mutex, OnceLock};

use crate::BnWorld;

type Catalog = benilla_formats::CreatureCatalog;

/// The catalog costs a pass over three DBCs (~tens of ms), so keep the last
/// Data dir's parse. A handle per process opens one Data dir, so this is a
/// one-time cost in practice.
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
    let cat = Arc::new(benilla_formats::load_creature_catalog(chain).ok()?);
    *guard = Some((world.data_dir.clone(), Arc::clone(&cat)));
    Some(cat)
}

/// `CreatureModelData` names `.mdx`; the M2 reader (and `bn_model_fill`'s
/// exact-path archive lookup) wants `.m2`.
fn m2_path(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".mdx") || lower.ends_with(".mdl") {
        let mut s = path[..path.len() - 4].to_string();
        s.push_str(".m2");
        s
    } else {
        path.to_string()
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

/// Write one display row as
/// `id|model=<.m2>|scale=<f>|tex=<t0>,<t1>,<t2>|npc=<race>/<sex>|bake=<name>`
/// plus, for a humanoid NPC, `|look=<skin>/<face>/<hair>/<haircolor>/<facial>`
/// and `|equip=<e0>,...,<e9>` (CreatureDisplayInfoExtra equipment, ItemDisplayInfo
/// ids in bodyslot order - e0 is the helm).
///
/// `model` is what the host renders, `scale` is the client's combined
/// `modelScale * creatureModelScale`, `npc` is 0/0 for a plain creature and
/// `bake` is the humanoid body atlas name (empty = composite live).
#[no_mangle]
pub extern "C" fn bn_creature_display_info(
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
    let Some(model) = cat.model(display_id) else {
        return 0;
    };
    let tex: Vec<&str> = model
        .textures
        .iter()
        .map(|t| t.as_deref().unwrap_or(""))
        .collect();
    let (race, sex, bake) = match model.npc_appearance.as_ref() {
        Some(a) => (a.race as u32, a.sex as u32, a.bake_name.as_deref().unwrap_or("")),
        None => (0, 0, ""),
    };
    let mut line = format!(
        "{}|model={}|scale={:.3}|tex={},{},{}|npc={}/{}|bake={}",
        display_id,
        m2_path(&model.model_path),
        model.scale,
        tex[0],
        tex[1],
        tex[2],
        race,
        sex,
        bake
    );
    if let Some(a) = model.npc_appearance.as_ref() {
        line.push_str(&format!(
            "|look={}/{}/{}/{}/{}|equip={}",
            a.skin,
            a.face,
            a.hair_style,
            a.hair_color,
            a.facial_hair,
            a.equipment
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    copy_out(&line, out, cap)
}

/// Scan `[from, to]` display ids and write one `id|model|scale` line per row
/// whose `.m2` path contains `needle` (case-insensitive; empty matches all).
/// This is how the host finds a boss/NPC display id (`bn.CreatureScan onyxia`).
#[no_mangle]
pub extern "C" fn bn_creature_scan(
    world: *mut BnWorld,
    needle: *const c_char,
    from: u32,
    to: u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let needle = if needle.is_null() {
        String::new()
    } else {
        // SAFETY: NUL-terminated by the caller.
        unsafe { std::ffi::CStr::from_ptr(needle) }
            .to_string_lossy()
            .to_ascii_lowercase()
    };
    let w = unsafe { &mut *world };
    let Some(cat) = catalog(w) else {
        return 0;
    };
    let bytes = cap as usize;
    let mut text = String::new();
    let mut id = from.max(1);
    while id <= to {
        if let Some(model) = cat.model(id) {
            let path = m2_path(&model.model_path);
            if needle.is_empty() || path.to_ascii_lowercase().contains(&needle) {
                let line = format!("{}|{}|{:.3}\n", id, path, model.scale);
                if text.len() + line.len() >= bytes {
                    break;
                }
                text.push_str(&line);
            }
        }
        id += 1;
    }
    copy_out(&text, out, cap)
}

/// Number of `CreatureDisplayInfo` rows (diagnostics; 0 = catalog failed).
#[no_mangle]
pub extern "C" fn bn_creature_count(world: *mut BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &mut *world };
    catalog(w).map(|c| c.len() as u32).unwrap_or(0)
}

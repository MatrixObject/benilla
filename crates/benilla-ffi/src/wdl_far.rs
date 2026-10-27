//! Middle-layer FFI: the WDL far-terrain band (benilla's "distant low-detail
//! terrain"), the coarse horizon the client draws beyond the ADT ring.
//!
//! The parse and the 545-vertex centre-fan mesh are `benilla_formats::wdl`.
//! Streaming policy: a Chebyshev ring
//! of radius 5 around the view, each tile one mesh section, drawn unlit in the
//! scene fog colour.

use std::ffi::c_char;

use crate::{wow_vec_to_ue, BnWorld};

fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_str().ok()
}

fn ensure_wdl(w: &mut BnWorld, map: &str) -> bool {
    if w.wdl.is_some() && w.wdl_map == map {
        return true;
    }
    let Some(chain) = w.chain.as_mut() else {
        return false;
    };
    match benilla_formats::WdlFile::load(chain, map) {
        Ok(f) => {
            w.wdl = Some(f);
            w.wdl_map = map.to_string();
            true
        }
        Err(_) => {
            w.wdl = None;
            w.wdl_map = map.to_string();
            false
        }
    }
}

/// `present_count` of the map's WDL, loading/caching it first. 0 = no file.
#[no_mangle]
pub extern "C" fn bn_wdl_present(world: *mut BnWorld, map: *const c_char) -> u32 {
    let (Some(w), Some(map)) = (unsafe { world.as_mut() }, cstr(map)) else {
        return 0;
    };
    if !ensure_wdl(w, map) {
        return 0;
    }
    w.wdl.as_ref().map_or(0, |f| f.present_count() as u32)
}

/// Present tiles within Chebyshev `radius` of world `(x, y)` in WoW yards.
/// Writes `(tile_x, tile_y)` u32 pairs and returns the number of pairs.
#[no_mangle]
pub extern "C" fn bn_wdl_ring(
    world: *mut BnWorld,
    map: *const c_char,
    x: f32,
    y: f32,
    radius: u32,
    out: *mut u32,
    cap_pairs: u32,
) -> u32 {
    let (Some(w), Some(map)) = (unsafe { world.as_mut() }, cstr(map)) else {
        return 0;
    };
    if out.is_null() || !ensure_wdl(w, map) {
        return 0;
    }
    let Some(wdl) = w.wdl.as_ref() else {
        return 0;
    };
    let tiles = wdl.tiles_in_ring(x, y, radius);
    let n = tiles.len().min(cap_pairs as usize);
    for (i, (tx, ty)) in tiles.iter().take(n).enumerate() {
        unsafe {
            *out.add(i * 2) = *tx;
            *out.add(i * 2 + 1) = *ty;
        }
    }
    n as u32
}

/// One coarse tile mesh in UE axes (yards): `out_verts` is xyz triples,
/// `out_idx` triangle-list indices, `counts` receives `[n_verts, n_indices]`.
/// Returns 1 when the tile is present and both buffers fit.
#[no_mangle]
pub extern "C" fn bn_wdl_tile(
    world: *mut BnWorld,
    map: *const c_char,
    tx: u32,
    ty: u32,
    out_verts: *mut f32,
    vcap: u32,
    out_idx: *mut u32,
    icap: u32,
    counts: *mut u32,
) -> u32 {
    let (Some(w), Some(map)) = (unsafe { world.as_mut() }, cstr(map)) else {
        return 0;
    };
    if out_verts.is_null() || out_idx.is_null() || counts.is_null() || !ensure_wdl(w, map) {
        return 0;
    }
    let Some(wdl) = w.wdl.as_ref() else {
        return 0;
    };
    let Some(mesh) = wdl.tile_mesh(tx, ty) else {
        return 0;
    };
    let nv = mesh.positions.len();
    let ni = mesh.indices.len();
    if nv > vcap as usize || ni > icap as usize {
        return 0;
    }
    for (i, p) in mesh.positions.iter().enumerate() {
        let (x, y, z) = wow_vec_to_ue(p[0], p[1], p[2]);
        unsafe {
            *out_verts.add(i * 3) = x;
            *out_verts.add(i * 3 + 1) = y;
            *out_verts.add(i * 3 + 2) = z;
        }
    }
    unsafe {
        std::ptr::copy_nonoverlapping(mesh.indices.as_ptr(), out_idx, ni);
        *counts = nv as u32;
        *counts.add(1) = ni as u32;
    }
    1
}

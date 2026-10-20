// Middle layer: doodad material animation addressed by model path.
//
// bn_doodad_mat_anim indexes the ACTIVE tile's MDDF list, so a host holding
// several streamed tiles resident must re-load each tile (full ADT re-parse)
// just to sample one tile's animated submeshes. Measured: 27.7ms per tile per
// frame - the whole frame budget at 4 resident tiles. This entry point keys
// the already-cached per-model submesh list (BnWorld::doodad_models), which
// survives tile switches, so sampling costs microseconds.
//
// Deliberately a separate module: benilla-ffi's existing exports are left
// untouched (the project rule is to grow the middleware, not patch originals).

use std::ffi::c_char;

use crate::BnWorld;

/// `.mdx`/`.mdl` -> `.m2` plus lowercasing, i.e. the same normalization
/// `bn_doodad_model_path` applies before handing a path to the host.
fn normalize_model(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    match lower
        .strip_suffix(".mdx")
        .or_else(|| lower.strip_suffix(".mdl"))
    {
        Some(stem) => format!("{stem}.m2"),
        None => lower,
    }
}

/// Submesh list for `model`, tolerating the path spellings the tile's MMDX
/// uses (raw case, `.mdx`) vs the normalized path the host was given.
fn model_subs<'a>(
    w: &'a BnWorld,
    model: &str,
) -> Option<&'a Vec<benilla_formats::RenderSubmesh>> {
    if let Some(subs) = w.doodad_models.get(model) {
        return Some(subs);
    }
    let want = normalize_model(model);
    w.doodad_models
        .iter()
        .find(|(key, _)| normalize_model(key) == want)
        .map(|(_, subs)| subs)
}

/// Material animation of `model`'s submesh `j` at `elapsed` seconds.
///
/// Same contract as `bn_doodad_mat_anim`: writes `uv.x, uv.y, r, g, b, alpha`
/// into `out` (identity for absent channels) and returns a channel mask
/// (1 uv, 2 rgb, 4 alpha), 0 when nothing animates or the model is not in the
/// world's cache. No active-tile dependency: safe to call for any resident
/// tile, any order, every frame.
#[no_mangle]
pub extern "C" fn bn_model_mat_anim(
    world: *const BnWorld,
    model: *const c_char,
    j: u32,
    elapsed: f64,
    out: *mut f32,
) -> u32 {
    if world.is_null() || model.is_null() || out.is_null() {
        return 0;
    }
    let Some(path) = crate::cstr_to_str(model) else {
        return 0;
    };
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(sm) = model_subs(w, path).and_then(|subs| subs.get(j as usize)) else {
        return 0;
    };
    let band = elapsed as f32;
    let mut mask = 0u32;

    // Shared loop first (baked against slot 0); the per-seq variant only
    // exists when the slots disagree, where a placed doodad reads slot 0.
    let mut uv = [0.0f32, 0.0];
    let uv_loop = sm
        .uv_anim
        .as_ref()
        .or_else(|| sm.uv_seq.as_ref().and_then(|s| s.seq(None)));
    if let Some(a) = uv_loop {
        uv = a.sample(a.clock(band, elapsed));
        mask |= 1;
    }

    let mut rgb = [1.0f32, 1.0, 1.0];
    let rgb_loop = sm
        .rgb_anim
        .as_ref()
        .or_else(|| sm.rgb_seq.as_ref().and_then(|s| s.seq(None)));
    if let Some(a) = rgb_loop {
        rgb = a.sample(a.clock(band, elapsed));
        mask |= 2;
    }

    // Placed-doodad lane: no playing sequence, so slot 0; color x weight.
    let alpha = match sm.alpha_anim.as_ref() {
        Some(a) => {
            mask |= 4;
            a.sample(None, band, elapsed)
        }
        None => 1.0,
    };

    unsafe {
        *out.add(0) = uv[0];
        *out.add(1) = uv[1];
        *out.add(2) = rgb[0];
        *out.add(3) = rgb[1];
        *out.add(4) = rgb[2];
        *out.add(5) = alpha;
    }
    mask
}

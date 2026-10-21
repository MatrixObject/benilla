// Middle layer: placed-doodad skeletal rigs, addressed by model path.
//
// The MDDF-placed M2 props (a fish in the water, a lamppost's flame, a
// windmill's sails) carry bone animation, but the host bakes every doodad
// placement into its static, texture-grouped batch, so those props hold
// their rest pose forever. This module exposes the rig of one model: its
// submesh layout, the arm chain the reference arms (the loader-idle seed
// and the variations it re-rolls over) and the skinned pose of one
// sequence at one time.
//
// Deliberately a separate module: benilla-ffi's existing exports are left
// untouched (the project rule is to grow the middleware, not patch
// originals), and the rig is keyed by model path, so it survives the tile
// switches of the handle that holds one tile at a time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::Arc;

use benilla_formats::{ModelAnimation, RenderSubmesh};

use crate::BnWorld;

/// The rig table's layout version (`bn_doodad_rig_info`).
const RIG_INFO_VERSION: u32 = 1;
/// Header size in u32.
const RIG_HEADER_U32: usize = 12;
/// Per-submesh stride in u32.
const RIG_SUBMESH_U32: usize = 4;
/// Per-clip stride in u32.
const RIG_CLIP_U32: usize = 8;
/// `flags` bit 0: the model has a loader-idle seed that is not the bind pose,
/// so the prop moves and must not be baked into the static batch.
const RIG_FLAG_ANIMATED: u32 = 1;

/// One sequence of the arm chain: the loader-idle seed and its variations.
#[derive(Clone, Copy)]
struct RigClip {
    /// The M2 file slot, which indexes every bone track's key ranges.
    seq_index: u32,
    start_ms: u32,
    end_ms: u32,
    duration_ms: u32,
    /// The variation's weight in the reference's per-play roll (`M2Sequence+0x14`).
    frequency: u16,
    /// `(minReplay, maxReplay)`: the play count the window rolls.
    min_replay: u32,
    max_replay: u32,
    looping: bool,
}

/// One model's rig, parsed once and cached by path.
struct Rig {
    /// The M2 bones, in UE axes (the pivot is Y-flipped at parse).
    bones: Vec<crate::BnBone>,
    /// The global-sequence durations, for the gseq bone tracks.
    global_seqs: Vec<u32>,
    /// The model's submeshes in the flat order `build_wmo_flat` bakes, so the
    /// posed vertices line up with the host's static batch.
    subs: Vec<RenderSubmesh>,
    /// The arm chain: every sequence of the idle id, in file order.
    clips: Vec<RigClip>,
    /// The idle animation id: the playable lookup's first row.
    idle_id: u16,
    /// The seed clip's index in `clips` (the loader's var-0 arm).
    seed: Option<u32>,
    /// The file's sequence count, the range a sequence slot must fall in.
    seq_count: u32,
}

impl Rig {
    /// The model's flat vertex count (every submesh concatenated).
    fn vert_count(&self) -> u32 {
        self.subs.iter().map(|s| s.positions.len() as u32).sum()
    }

    /// The model's flat index count.
    fn idx_count(&self) -> u32 {
        self.subs.iter().map(|s| s.indices.len() as u32).sum()
    }

    /// The submesh's `{vtx0, vtxN, idx0, idxN}` in the flat layout.
    fn submesh_range(&self, j: usize) -> Option<[u32; 4]> {
        let mut vtx0 = 0u32;
        let mut idx0 = 0u32;
        for (k, sm) in self.subs.iter().enumerate() {
            if k == j {
                return Some([
                    vtx0,
                    sm.positions.len() as u32,
                    idx0,
                    sm.indices.len() as u32,
                ]);
            }
            vtx0 += sm.positions.len() as u32;
            idx0 += sm.indices.len() as u32;
        }
        None
    }

    /// The model's skinned pose at (sequence slot, absolute timeline ms).
    ///
    /// Six floats per vertex: the position and the normal, both in the model's
    /// own UE axes (`(x, -y, z)`), in the flat submesh order the static batch
    /// uses. The host applies the placement matrix per placement.
    fn pose(&self, seq_index: usize, time_ms: u32) -> Vec<f32> {
        let mats: Vec<[f32; 16]> = (0..self.bones.len())
            .map(|i| {
                crate::bone_matrix_at(
                    &self.bones,
                    seq_index,
                    time_ms,
                    time_ms,
                    &self.global_seqs,
                    i,
                )
            })
            .collect();
        let mut out = Vec::with_capacity(self.vert_count() as usize * 6);
        for sm in &self.subs {
            for i in 0..sm.positions.len() {
                let p = sm.positions[i];
                // fixCoordSystem: the vertex's local position in UE axes
                // (x, -y, z), matching model.cpp's origVertices.
                let local = [p[0], -p[1], p[2]];
                let joints = sm.joints.get(i).copied().unwrap_or([0, 0, 0, 0]);
                let weights = sm.weights.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                let total: f32 = weights.iter().sum();

                // The reference model.cpp L730: v += mat * pos * w.
                let mut pos = local;
                if total > 1e-6 {
                    let mut acc = [0.0f32; 3];
                    for k in 0..4 {
                        let bi = joints[k] as usize;
                        if bi >= mats.len() || weights[k] == 0.0 {
                            continue;
                        }
                        let sk = apply_point(&mats[bi], local);
                        for j in 0..3 {
                            acc[j] += weights[k] * sk[j];
                        }
                    }
                    pos = acc;
                }

                // The authored normal rides the same blend, without the
                // translation, and is renormalized afterwards.
                let n = sm.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                let nlocal = [n[0], -n[1], n[2]];
                let mut nrm = nlocal;
                if total > 1e-6 {
                    let mut acc = [0.0f32; 3];
                    for k in 0..4 {
                        let bi = joints[k] as usize;
                        if bi >= mats.len() || weights[k] == 0.0 {
                            continue;
                        }
                        let sk = apply_dir(&mats[bi], nlocal);
                        for j in 0..3 {
                            acc[j] += weights[k] * sk[j];
                        }
                    }
                    let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt();
                    if len > 1e-6 {
                        nrm = [acc[0] / len, acc[1] / len, acc[2] / len];
                    }
                }

                out.extend_from_slice(&pos);
                out.extend_from_slice(&nrm);
            }
        }
        out
    }
}

/// Row-major `m * v` (the translation in `m[3]`, `m[7]`, `m[11]`).
fn apply_point(m: &[f32; 16], v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2] + m[3],
        m[4] * v[0] + m[5] * v[1] + m[6] * v[2] + m[7],
        m[8] * v[0] + m[9] * v[1] + m[10] * v[2] + m[11],
    ]
}

/// Row-major `m * v` without the translation (a direction).
fn apply_dir(m: &[f32; 16], v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2],
        m[4] * v[0] + m[5] * v[1] + m[6] * v[2],
        m[8] * v[0] + m[9] * v[1] + m[10] * v[2],
    ]
}

// The model cache, keyed by path. The model files are immutable, so one
// parse per path serves every tile and every frame.
thread_local! {
    static RIG_CACHE: RefCell<HashMap<String, Option<Arc<Rig>>>> =
        RefCell::new(HashMap::new());
}

/// The directory part of a model path, the way `load_m2_mesh` derives it.
fn parent_dir(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[..i],
        None => "",
    }
}

/// The model's bytes: the first archive that can actually read them.
///
/// `crate::m2_bytes` stops at the first archive that *lists* the path and
/// breaks when that read fails, so one copy that cannot be decoded would
/// leave the rig empty for the whole process (the failure is cached). The
/// archives are registered in directory order, so which copy that is varies
/// from run to run.
fn model_bytes(world: &BnWorld, path: &str) -> Option<Vec<u8>> {
    for a in world.archives.iter().rev() {
        if !a.contains(path) {
            continue;
        }
        match a.read_file(path) {
            Ok(bytes) => return Some(bytes),
            Err(e) => {
                eprintln!("doodad_rig: {} unreadable: {e}", path);
            }
        }
    }
    None
}

/// Parse one model's rig, or `None` when the path holds no M2.
fn build_rig(world: &BnWorld, path: &str) -> Option<Rig> {
    let bytes = model_bytes(world, path)?;
    // The same submesh parse the static bake uses, so the flat vertex order
    // matches the host's baked batch exactly. When the host has already
    // loaded a placement of this model, the static bake's parse is reused:
    // the model is parsed once, not once per path.
    let subs = match world.doodad_models.get(path) {
        Some(subs) => subs.clone(),
        None => benilla_formats::parse_m2_render_submeshes(&bytes, parent_dir(path), &[]).ok()?,
    };
    let bones = crate::parse_m2_bones(&bytes)?;
    let global_seqs = crate::parse_m2_global_sequences(&bytes);
    let anims: Vec<ModelAnimation> = benilla_formats::parse_m2_animations(&bytes);
    let playable = benilla_formats::parse_m2_playable_animation_lookup(&bytes).unwrap_or_default();

    // The loader-idle seed's id: the playable lookup's first row (id 0 through
    // it), the way `ModelAnimations::idle_clip` resolves it.
    let idle_id = playable.first().map_or(0, |p| p.resolved_id);

    // The arm chain: every sequence of the idle id, in file order (the
    // reference's variation chain). The seed is the first of them that is not
    // the bind pose (`ModelAnimations::first_seq`).
    let mut clips: Vec<RigClip> = Vec::new();
    let mut seed: Option<u32> = None;
    for a in anims.iter().filter(|a| a.anim_id == idle_id) {
        if seed.is_none() && !a.is_rest_pose() {
            seed = Some(clips.len() as u32);
        }
        clips.push(RigClip {
            seq_index: a.seq_index as u32,
            start_ms: a.start_ms,
            end_ms: a.end_ms,
            duration_ms: a.end_ms.saturating_sub(a.start_ms),
            frequency: a.frequency,
            min_replay: a.min_replay,
            max_replay: a.max_replay,
            looping: a.looping,
        });
    }

    Some(Rig {
        bones,
        global_seqs,
        subs,
        clips,
        idle_id,
        seed,
        seq_count: crate::m2_anim_count(&bytes),
    })
}

/// The cached rig of `path`, parsed on first use.
fn rig_for(world: &BnWorld, path: &str) -> Option<Arc<Rig>> {
    let hit = RIG_CACHE.with(|cell| cell.borrow().get(path).cloned());
    if let Some(found) = hit {
        return found;
    }
    let built = build_rig(world, path).map(Arc::new);
    RIG_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        map.insert(path.to_string(), built.clone());
        built
    })
}

/// The rig table of `model_path`: a header, one row per submesh and one row
/// per arm-chain clip, all u32.
///
/// ```text
/// header (12 u32)
///   [0]  layout version
///   [1]  flags: bit 0 = the model has a non-bind-pose seed, so it animates
///   [2]  submesh count
///   [3]  clip count (the arm chain)
///   [4]  total vertex count (flat)
///   [5]  total index count (flat)
///   [6]  the idle animation id
///   [7]  the seed clip's index in the chain (0xffffffff when there is none)
///   [8]  bone count
///   [9..12] reserved (0)
/// per submesh (4 u32): vtx0, vtx_n, idx0, idx_n
/// per clip (8 u32): seq_index, start_ms, end_ms, duration_ms, frequency,
///                   min_replay, max_replay, looping
/// ```
///
/// Returns the table's u32 length (0 when the path holds no model). The
/// caller may pass `cap` below that: the table is filled as far as it fits
/// and the full length is still returned.
#[no_mangle]
pub extern "C" fn bn_doodad_rig_info(
    world: *const BnWorld,
    model_path: *const c_char,
    out: *mut u32,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }
    let Some(path) = crate::cstr_to_str(model_path) else {
        return 0;
    };
    // SAFETY: valid handle.
    let w = unsafe { &*world };
    let Some(rig) = rig_for(w, path) else {
        return 0;
    };

    let sub_n = rig.subs.len();
    let clip_n = rig.clips.len();
    let total = RIG_HEADER_U32 + sub_n * RIG_SUBMESH_U32 + clip_n * RIG_CLIP_U32;
    if out.is_null() || cap == 0 {
        return total as u32;
    }

    let mut table: Vec<u32> = Vec::with_capacity(total);
    table.push(RIG_INFO_VERSION);
    table.push(if rig.seed.is_some() { RIG_FLAG_ANIMATED } else { 0 });
    table.push(sub_n as u32);
    table.push(clip_n as u32);
    table.push(rig.vert_count());
    table.push(rig.idx_count());
    table.push(rig.idle_id as u32);
    table.push(rig.seed.unwrap_or(u32::MAX));
    table.push(rig.bones.len() as u32);
    table.push(0);
    table.push(0);
    table.push(0);
    for j in 0..sub_n {
        let r = rig.submesh_range(j).unwrap_or([0, 0, 0, 0]);
        table.extend_from_slice(&r);
    }
    for c in &rig.clips {
        table.push(c.seq_index);
        table.push(c.start_ms);
        table.push(c.end_ms);
        table.push(c.duration_ms);
        table.push(c.frequency as u32);
        table.push(c.min_replay);
        table.push(c.max_replay);
        table.push(c.looping as u32);
    }

    let n = total.min(cap as usize);
    // SAFETY: the caller owns `n` u32 of room.
    unsafe { std::ptr::copy_nonoverlapping(table.as_ptr(), out, n) };
    total as u32
}

/// The weighted pick of an arm-chain clip, the reference's `pick_variation`
/// walk over the sequences that share `anim_id`.
///
/// `roll` is one `rand()` draw in `[0, 32767]`. Returns the clip's index in
/// the chain published by [`bn_doodad_rig_info`], or `0xffffffff` when the
/// model has no chain for `anim_id`.
#[no_mangle]
pub extern "C" fn bn_doodad_rig_pick(
    world: *const BnWorld,
    model_path: *const c_char,
    anim_id: u32,
    roll: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() {
        return u32::MAX;
    }
    let Some(path) = crate::cstr_to_str(model_path) else {
        return u32::MAX;
    };
    // SAFETY: valid handle.
    let w = unsafe { &*world };
    let Some(rig) = rig_for(w, path) else {
        return u32::MAX;
    };
    if rig.idle_id as u32 != anim_id || rig.clips.is_empty() {
        return u32::MAX;
    }
    // The reference's walk: subtract each variation's weight and stop at the
    // first that covers the roll; exhaustion keeps the head.
    let mut r = roll as i64;
    for (i, c) in rig.clips.iter().enumerate() {
        if r < i64::from(c.frequency) {
            return i as u32;
        }
        r -= i64::from(c.frequency);
    }
    0
}

/// The model's skinned pose at (sequence slot, absolute timeline ms).
///
/// Writes six floats per vertex into `out` (the position and the normal, both
/// in the model's own UE axes, in the flat submesh order the static bake
/// uses) and returns the vertex count written. `cap` is the room in floats.
#[no_mangle]
pub extern "C" fn bn_doodad_rig_pose(
    world: *const BnWorld,
    model_path: *const c_char,
    seq_index: u32,
    time_ms: u32,
    out: *mut f32,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() {
        return 0;
    }
    let Some(path) = crate::cstr_to_str(model_path) else {
        return 0;
    };
    // SAFETY: valid handle.
    let w = unsafe { &*world };
    let Some(rig) = rig_for(w, path) else {
        return 0;
    };
    if seq_index >= rig.seq_count {
        // The file slot is out of range: the track ranges would read past the
        // sequence table, so refuse instead of posing a wrong frame.
        return 0;
    }
    let posed = rig.pose(seq_index as usize, time_ms);
    let verts = posed.len() / 6;
    let floats = (verts * 6).min(cap as usize);
    // SAFETY: the caller owns `floats` floats of room.
    unsafe { std::ptr::copy_nonoverlapping(posed.as_ptr(), out, floats) };
    verts as u32
}

/// The placement matrix of doodad `i` in the active tile.
///
/// Row-major 4x4, mapping the model's own UE axes into the tile's local UE
/// axes (the tile centre at the origin, in yards): the placement's rotation
/// and scale, then the offset from the tile centre. The host multiplies the
/// result by 100 for centimetres.
///
/// Returns 1 on success, 0 when there is no active tile or no such doodad.
#[no_mangle]
pub extern "C" fn bn_doodad_placement(
    world: *const BnWorld,
    i: u32,
    out: *mut f32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: valid handle.
    let w = unsafe { &*world };
    let Some(tile) = w.tile.as_ref() else {
        return 0;
    };
    let Some(d) = tile.doodads.get(i as usize) else {
        return 0;
    };

    // The placement quaternion, mirrored into the model's UE axes: the
    // WoW->UE mirror F = diag(1, -1, 1) conjugates a rotation into
    // q' = (-x, y, -z, w), which is what `build_wmo_flat` ends up applying
    // once the mirror is folded in.
    let q = crate::placement_quat(d.rotation);
    let qp = [-q[0], q[1], -q[2], q[3]];

    // The rotation's columns, so the matrix multiplies a point the same way
    // `q_rot` does.
    let col = |j: usize| crate::q_rot(qp, [if j == 0 { 1.0 } else { 0.0 }, if j == 1 { 1.0 } else { 0.0 }, if j == 2 { 1.0 } else { 0.0 }]);

    let s = d.scale;
    let (cx, cy) = tile.center;
    let t = [d.position[0] - cx, cy - d.position[1], d.position[2]];

    let mut m = [0.0f32; 16];
    for j in 0..3 {
        let c = col(j);
        m[j] = c[0] * s;
        m[4 + j] = c[1] * s;
        m[8 + j] = c[2] * s;
    }
    m[3] = t[0];
    m[7] = t[1];
    m[11] = t[2];
    m[15] = 1.0;

    // SAFETY: the caller owns 16 floats of room.
    unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), out, 16) };
    1
}

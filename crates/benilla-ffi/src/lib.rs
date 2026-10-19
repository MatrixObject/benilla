// Benilla FFI — C ABI export layer for the UE5 render host.

//

// Keeps all WoW 1.12.1 format parsing in Rust (MPQ/ADT/M2/WMO/BLP, no Bevy,

// no app, no networking); exposes the parsed geometry and decoded textures

// through plain C structs so the UE `BnRenderer` module can rebuild meshes

// and upload textures without depending on any Rust type.

//

// Built as a cdylib (`benilla.dll`); the UE side declares the same

// functions `extern "C"` and links against the .dll.

use std::ffi::c_char;

// ---- WoW <-> UE5 coordinate-system converter (single source of truth) ----

mod coords;

use std::io::Cursor;

use std::path::Path;

use benilla_adt::McnkChunk;

use benilla_mpq::Archive;

// ---- C ABI data shapes (plain POD; no Rust types cross the boundary) ----

#[repr(C)]

#[derive(Clone, Copy)]

pub struct BnVertex {

    pub x: f32,

    pub y: f32,

    pub z: f32,

    pub nx: f32,

    pub ny: f32,

    pub nz: f32,

    pub u: f32,

    pub v: f32,

}

#[repr(C)]

#[derive(Clone, Copy)]

pub struct BnTriangle {

    pub a: u16,

    pub b: u16,

    pub c: u16,

    pub material: u8,

    /// bit 0x01 = two-sided (M2 material render flag 0x04): don't back-face cull.
    pub flags: u8,

}

/// Opaque handle to the Rust-side state (MPQ chain + loaded tiles).

pub struct BnWorld {

    archives: Vec<Archive>,

    mcnk: Vec<McnkChunk>,

}

/// Convert a WoW position to UE (same as model.cpp fixCoordSystem: (x, -y, z)).

fn wow_to_ue(px: f32, py: f32, pz: f32) -> BnVertex {

    BnVertex {

        x: px,

        y: -py,

        z: pz,

        nx: 0.0,

        ny: 1.0,

        nz: 0.0,

        u: 0.0,

        v: 0.0,

}

    }

/// Convert a WoW normal/vector to UE (same as model.cpp fixCoordSystem: (x, -y, z)).

fn wow_vec_to_ue(vx: f32, vy: f32, vz: f32) -> (f32, f32, f32) {

    (vx, -vy, vz)

}

/// A CStr->&str helper that is safe to call from FFI (the caller guarantees a

/// valid NUL-terminated C string).

fn cstr_to_str<'a>(s: *const c_char) -> Option<&'a str> {

    if s.is_null() {

        return None;

    }

    // SAFETY: FFI contract — pointers are valid, NUL-terminated C strings.

    unsafe { std::ffi::CStr::from_ptr(s) }.to_str().ok()

}

// ---- init / shutdown ----

/// Open the 1.12.1 `Data` directory: register every `*.MPQ` in the chain.

/// Returns a handle, or null on error.

#[no_mangle]

pub extern "C" fn bn_init(data_dir: *const c_char) -> *mut BnWorld {

    let dir = match cstr_to_str(data_dir) {

        Some(s) => s,

        None => return std::ptr::null_mut(),

    };

    let data = Path::new(dir);

    if !data.is_dir() {

        return std::ptr::null_mut();

    }

    let mut archives: Vec<Archive> = Vec::new();

    if let Ok(read) = std::fs::read_dir(data) {

        for entry in read.flatten() {

            let p = entry.path();

            if !p

                .extension()

                .is_some_and(|e| e.eq_ignore_ascii_case("mpq"))

            {

                continue;

            }

            if let Ok(a) = Archive::open(&p) {

                archives.push(a);

            }

        }

    }

    if archives.is_empty() {

        return std::ptr::null_mut();

    }

    Box::into_raw(Box::new(BnWorld {

        archives,

        mcnk: Vec::new(),

    }))

}

#[no_mangle]

pub extern "C" fn bn_shutdown(world: *mut BnWorld) {

    if !world.is_null() {

        let _ = unsafe { Box::from_raw(world) };

    }

}

/// How many MPQ archives are registered (diagnostics).

#[no_mangle]

pub extern "C" fn bn_num_archives(world: *const BnWorld) -> u32 {

    if world.is_null() {

        return 0;

    }

    // SAFETY: caller owns the handle and passes a valid one.

    unsafe { &*world }

        .archives

        .len() as u32

}

// ---- map / terrain ----

/// Load and parse a terrain tile `World\Maps\<map>\<map>_<x>_<z>.adt` from the

/// chain; store the MCNK chunks for `bn_terrain_*`. Returns chunks parsed.

#[no_mangle]

pub extern "C" fn bn_load_tile(

    world: *mut BnWorld,

    map_name: *const c_char,

    x: i32,

    z: i32,

) -> u32 {

    if world.is_null() {

        return 0;

    }

    let map = match cstr_to_str(map_name) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: caller guarantees a valid, exclusive handle.

    let w = unsafe { &mut *world };

    let path = format!("World\\Maps\\{map}\\{map}_{x}_{z}.adt");

    for a in w.archives.iter().rev() {

        if !a.contains(&path) {

            continue;

        }

        if let Ok(bytes) = a.read_file(&path) {

            if let Ok(parsed) = benilla_adt::parse_adt(&mut Cursor::new(bytes.as_slice())) {

                if let benilla_adt::ParsedAdt::Root(root) = parsed {

                    w.mcnk = root.mcnk_chunks;

                    return w.mcnk.len() as u32;

                }

            }

            break;

        }

    }

    0

}

/// Terrain vertex count the UE side should pre-allocate.

#[no_mangle]

pub extern "C" fn bn_terrain_vertex_count(world: *const BnWorld) -> u32 {

    if world.is_null() {

        return 0;

    }

    // SAFETY: valid const handle.

    let w = unsafe { &*world };

    // POC: a 3x3 node per chunk (chunk base position + small grid).

    (w.mcnk.len() as u32).max(1) * 9

}

/// Fill `out` (pre-allocated) with terrain vertices. Returns count written.

#[no_mangle]

pub extern "C" fn bn_terrain_fill_vertices(

    world: *const BnWorld,

    out: *mut BnVertex,

    cap: u32,

) -> u32 {

    if world.is_null() || out.is_null() {

        return 0;

    }

    let w = unsafe { &*world };

    let mut n = 0u32;

    for c in w.mcnk.iter() {

        if n >= cap {

            break;

        }

        let base = c.header.position;

        // 3x3 small grid centred on the chunk origin, Z-up in UE axes.

        for dz in -1..=1 {

            for dx in -1..=1 {

                if n >= cap {

                    break;

                }

                let (gx, gy, gz) = wow_vec_to_ue(base[0] + dx as f32, base[1], base[2] + dz as f32);

                unsafe {

                    *out.add(n as usize) = BnVertex {

                        x: gx,

                        y: gy,

                        z: gz,

                        nx: 0.0,

                        ny: 1.0,

                        nz: 0.0,

                        u: 0.0,

                        v: 0.0,

                    };

                }

                n += 1;

            }

        }

    }

    n

}

/// Fill WMO mesh vertices + triangles for a WMO path. POC: returns 0 until

/// the WMO renderer is ported through the FFI.

#[no_mangle]

pub extern "C" fn bn_wmo_fill(

    world: *const BnWorld,

    wmo_path: *const c_char,

    out_vertices: *mut BnVertex,

    out_triangles: *mut BnTriangle,

    cap: u32,

) -> u32 {

    let _ = (world, wmo_path, out_vertices, out_triangles, cap);

    0

}

// ---- M2 model ----

/// Resolve an M2 model path in the MPQ chain, returning its bytes.

fn m2_bytes(world: &BnWorld, model_path: &str) -> Option<Vec<u8>> {

    for a in world.archives.iter().rev() {

        if !a.contains(model_path) {

            continue;

        }

        if let Ok(bytes) = a.read_file(model_path) {

            return Some(bytes);

        }

        break;

    }

    None

}

/// Read a u32 at byte offset (0 on OOB).

fn rd_u32(b: &[u8], o: usize) -> Option<u32> {

    let mut s = [0u8; 4]; s.copy_from_slice(b.get(o..o + 4)?);

    Some(u32::from_le_bytes(s))

}

/// Read a u16 at byte offset (0 on OOB).

fn rd_u16(b: &[u8], o: usize) -> Option<u16> {

    let mut s = [0u8; 2]; s.copy_from_slice(b.get(o..o + 2)?);

    Some(u16::from_le_bytes(s))

}

/// Read an i16 at byte offset.

fn rd_i16(b: &[u8], o: usize) -> Option<i16> {

    let mut s = [0u8; 2]; s.copy_from_slice(b.get(o..o + 2)?);

    Some(i16::from_le_bytes(s))

}

/// Read an f32 at byte offset (0 on OOB).

fn rd_f32(b: &[u8], o: usize) -> Option<f32> {

    let mut s = [0u8; 4]; s.copy_from_slice(b.get(o..o + 4)?);

    Some(f32::from_le_bytes(s))

}

/// The M2 header array count+offset pair for a given array tag at a header

/// offset. Pre-Wrath M2 (v<256) packs arrays as (count u32 @ofs, offset u32

/// @ofs+4). Returns (count, offset) or None when the record is absent.

fn m2_array_at(b: &[u8], ofs: usize, count_off: usize, ofs_off: usize) -> Option<(u32, u32)> {

    let c = rd_u32(b, ofs + count_off)?;

    let o = rd_u32(b, ofs + ofs_off)?;

    Some((c, o))

}

/// Locate the model-arrays base: the count/ofs record where `nBones`,

/// `nVertices` etc. live. M2 v<256: arrays start at 0x0C; nVertices @0x0C,

/// nBones @0x48, nAnimations @0x54, nTextures @0x64 (12-byte spacing).

const M2_ARRAYS: usize = 0x0C;

fn m2_vertex_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0x38, 0x3C) // nVertices @0x44/0x48

}

fn m2_triangle_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0x04, 0x08) // nTriangles

}

fn m2_bones_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0x28, 0x2C) // nBones @0x34/0x38

}

fn m2_anim_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0x10, 0x14) // nAnimations @0x1C/0x20

}

fn m2_texture_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0x50, 0x54) // nTextures @0x5C/0x60

}

fn m2_attachment_array(b: &[u8]) -> Option<(u32, u32)> {

    m2_array_at(b, M2_ARRAYS, 0xF8, 0xFC) // nAttachments @0x104/0x108

}

/// Global-sequence durations (header ofsGlobalSequences @0x14 count,

/// @0x18 offset, one u32 duration each).

fn parse_m2_global_sequences(b: &[u8]) -> Vec<u32> {
    let Some((n, ofs)) = m2_array_at(b, 0x00, 0x14, 0x18) else {
        return Vec::new();
    };
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n {
        v.push(rd_u32(b, ofs as usize + i as usize * 4).unwrap_or(0));
    }
    v
}

/// Vertex count of the M2 file.

pub fn m2_vertex_count(bytes: &[u8]) -> u32 {

    m2_vertex_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Triangle count of the M2 file.

pub fn m2_triangle_count(bytes: &[u8]) -> u32 {

    m2_triangle_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Number of embedded BLP textures in the model.

pub fn m2_texture_count(bytes: &[u8]) -> u32 {

    m2_texture_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Number of attachment points in the model.

pub fn m2_attachment_count(bytes: &[u8]) -> u32 {

    m2_attachment_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Rest-pose bone world positions: walk each bone's parent chain,

/// accumulating pivot translations. Returns a `Vec<[f32;3]>` of size

/// `model.bones.len()` (in WoW Y-up coords, same space as `bone.pivot`).

pub fn m2_bone_rest_positions(bytes: &[u8]) -> Option<Vec<[f32; 3]>> {

    let (count, ofs) = m2_bones_array(bytes)?;

    let capped = std::cmp::min(count as usize, bytes.len().saturating_sub(ofs as usize) / 108);

    let mut parents = Vec::with_capacity(capped);

    let mut pivots = Vec::with_capacity(capped);

    for i in 0..capped {

        let rec = ofs as usize + i * 108;

        let parent = rd_u16(bytes, rec + 8).unwrap_or(0xffff) as i16;

        let px = rd_f32(bytes, rec + 96).unwrap_or(0.0);

        let py = rd_f32(bytes, rec + 100).unwrap_or(0.0);

        let pz = rd_f32(bytes, rec + 104).unwrap_or(0.0);

        parents.push(parent);

        pivots.push([px, py, pz]);

    }

    // Accumulate: world = pivot + (parent>=0 ? world[parent] : 0).

    let mut out = vec![[0.0f32; 3]; capped];

    let mut resolved = vec![false; capped];

    for i in 0..capped {

        fn walk(

            i: usize,

            parents: &[i16],

            pivots: &[[f32; 3]],

            out: &mut [[f32; 3]],

            resolved: &mut [bool],

        ) {

            if resolved[i] || out[i][0].is_nan() {

                return;

            }

            resolved[i] = true;

            let mut acc = pivots[i];

            let mut p = parents[i] as usize;

            let mut guard = 0;

            while p < parents.len() && guard < 512 {

                acc[0] += pivots[p][0];

                acc[1] += pivots[p][1];

                acc[2] += pivots[p][2];

                p = parents[p] as usize;

                guard += 1;

            }

            out[i] = acc;

        }

        walk(i, &parents, &pivots, &mut out, &mut resolved);

    }

    Some(out)

}

/// One M2 vertex: `position` (C3, @0), weights (@0x0c), 4 bone indices

/// (@0x10), normal (@0x14), UV (@0x20). Rest pose applies the dominant

/// bone's pivot, then the weighted skin blend:

///

///   world_pos = Σ (bone_world_matrix[k] * (position - pivot[k] + pivot[k].world)) * (w_k / 255)

///

/// In rest pose the bone rotation is identity, so only the pivot translation

/// matters. This matches benilla's `build_skeleton` / `inverse bind pose =

/// translate(-pivot)` logic.

#[no_mangle]

pub extern "C" fn bn_model_fill(

    world: *const BnWorld,

    model_path: *const c_char,

    out_vertices: *mut BnVertex,

    out_triangles: *mut BnTriangle,

    cap: u32,

) -> u32 {

    if world.is_null()

        || model_path.is_null()

        || out_vertices.is_null()

        || out_triangles.is_null()

    {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) {

                let model = fmt.model();

                let nv = model.vertices.len().min(cap as usize);

                for i in 0..nv {

                    let v = &model.vertices[i];

                    // model.cpp initCommon: just fixCoordSystem(pos), no bone transform.

                    // Bone matrices are only applied during animation ticks.

                    let (qx, qy, qz) = wow_vec_to_ue(v.position.x, v.position.y, v.position.z);

                    let (nx, ny, nz) = wow_vec_to_ue(v.normal.x, v.normal.y, v.normal.z);

                    // SAFETY: out_vertices has room for cap BnVertex.

                    unsafe {

                        *out_vertices.add(i) = BnVertex {

                            x: qx,

                            y: qy,

                            z: qz,

                            nx,

                            ny,

                            nz,

                            u: v.tex_coords.x,

                            v: v.tex_coords.y,

                        };

                    }

                }

                // Real winding: the model's embedded skin profile (index 0) holds

                // the true triangle index buffer. Fall back to sequential triples

                // when no skin is embedded, so the mesh is always closed.

                // nezha original: final_vertex_index = indexLookup[triangles[i]]

                // skin.indices() = indexLookup (vertex remap table)

                // skin.triangles() = triangle index array (indices into indexLookup)

                let skin = model.parse_embedded_skin(bytes.as_slice(), 0).ok();

                let (tri, two_sided): (Vec<u16>, Vec<bool>) = match skin {

                    Some(s) => {

                        let index_lookup = s.indices();

                        let tris = s.triangles();

                        let tri: Vec<u16> = tris.iter()

                            .map(|&t| index_lookup.get(t as usize).copied().unwrap_or(t))

                            .collect();

                        // Per-batch two-sided from M2 material render flag 0x04,
                        // same semantics as benilla-formats m2_batches: the section
                        // range covers the FLAT u16 triangle array
                        // (triangle_start..+triangle_count, no *3).
                        let mut ts = vec![false; tri.len()];

                        let sections = s.submeshes();

                        for b in s.batches() {

                            let Some(sec) = sections.get(b.skin_section_index as usize) else {

                                continue;

                            };

                            let start = sec.triangle_start as usize;

                            let end = (start + sec.triangle_count as usize).min(ts.len());

                            if start >= end {

                                continue;

                            }

                            let two = model

                                .materials

                                .get(b.material_index as usize)

                                .is_some_and(|m| m.flags.bits() & 0x04 != 0);

                            if two {

                                ts[start..end].fill(true);

                            }

                        }

                        (tri, ts)

                    }

                    None => ((0..nv).map(|i| i as u16).collect(), vec![false; nv]),

                };

                let mut ti = 0u32;

                let mut n = 0usize;

                while n + 2 < tri.len() && ti < cap {

                    // SAFETY: out_triangles has room for cap BnTriangle.

                    unsafe {

                        *out_triangles.add(ti as usize) = BnTriangle {

                            a: tri[n],

                            b: tri[n + 1],

                            c: tri[n + 2],

                            material: 0,

                            flags: if two_sided[n] { 1 } else { 0 },

                        };

                    }

                    ti += 1;

                    n += 3;

                }

                return ti;

            }

            break;

        }

    }

    0

}

/// Count of M2 vertices. The UE renderer sizes its vertex buffer from this

/// before calling `bn_model_fill`. Returns 0 on a non-M2 miss.

#[no_mangle]

pub extern "C" fn bn_model_vertex_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_vertex_count(bytes.as_slice());

        }

        break;

    }

    0

}

/// Count of M2 triangles. The UE renderer sizes its index buffer from this.

/// Returns 0 on a non-M2 miss.

#[no_mangle]

pub extern "C" fn bn_model_triangle_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            // Match bn_model_fill exactly: the true index buffer is the

            // embedded skin's triangle list (indexLookup-mapped). Count its

            // triangles (indices / 3).

            if let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) {

                let model = fmt.model();

                if let Ok(skin) = model.parse_embedded_skin(bytes.as_slice(), 0) {

                    return (skin.triangles().len() / 3) as u32;

                }

                // No embedded skin: fall back to a dense sequential triangle

                // strip over the vertex list (same fallback bn_model_fill

                // uses), which is floor(verts/3).

                return (model.vertices.len() / 3) as u32;

            }

            break;

        }

    }

    0

}

/// Count of embedded M2 textures (the `textures` array, one BLP each).

/// `0` for a non-M2 miss. The UE side iterates `[0, n)` and pulls each via

/// `bn_model_texture_name` + `bn_texture`.

#[no_mangle]

pub extern "C" fn bn_model_texture_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_texture_count(bytes.as_slice());

        }

        break;

    }

    0

}

/// Write the embedded texture `idx`'s BLP filename (or `<index idx>`) into

/// `out` (up to `cap` bytes including the NUL). Returns bytes written.

#[no_mangle]

pub extern "C" fn bn_model_texture_name(

    world: *const BnWorld,

    model_path: *const c_char,

    idx: u32,

    out: *mut u8,

    cap: u32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() || cap == 0 {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            let (_, ofs) = match m2_texture_array(bytes.as_slice()) {

                Some(v) => v,

                None => return 0,

            };

            // Each M2Texture is 0x28: name (max 0x20 bytes) + track.

            // Each M2TextureDef is 0x10: type u32 @0, flags u32 @4,

            // nameLen u32 @8, nameOfs u32 @12. The filename is a string in

            // the M2 string table (same layout benilla-m2 parses).

            let rec = ofs as usize + idx as usize * 0x10;

            // M2TextureDef: type u32 @0, flags u32 @4, nameLen u32 @8, nameOfs u32 @12.
            let tex_type = rd_u32(bytes.as_slice(), rec).unwrap_or(0);

            let (name_len, name_ofs) = match (rd_u32(bytes.as_slice(), rec + 8), rd_u32(bytes.as_slice(), rec + 12)) {

                (Some(l), Some(o)) => (l, o),

                _ => return 0,

            };

            let name_bytes = bytes

                .get(name_ofs as usize..name_ofs as usize + name_len as usize)

                .map(|s| {

                    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());

                    &s[..end]

                })

                .unwrap_or(b"");

            let is_empty = name_bytes.is_empty();

            // Monster1 (11) slots name no file: the WoW client looks the skin up
            // through CreatureDisplayInfo.dbc. POC: infer the brown-skin path
            // from the model path (`Creature\X\X.m2` → `Creature\X\XSkinBrown.blp`).
            // Other empty slots stay empty.
            let text: Vec<u8> = if is_empty && tex_type == 11 {
                let dir_end = p.rfind('\\').map(|i| i + 1).unwrap_or(0);
                let stem_end = p.rfind('.').unwrap_or(p.len());
                let dir = &p[..dir_end];
                let stem = &p[dir_end..stem_end];
                format!("{dir}{stem}SkinBrown.blp").into_bytes()
            } else if is_empty {
                return 0;
            } else {
                name_bytes.to_vec()
            };

            let n = text.len().min((cap as usize) - 1);

            unsafe {

                std::ptr::copy_nonoverlapping(text.as_ptr(), out, n);

                *out.add(n) = 0;

            }

            return (n + 1) as u32;

        }

        break;

    }

    0

}

/// Number of attachment points on the model.

#[no_mangle]

pub extern "C" fn bn_model_attachment_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_attachment_count(bytes.as_slice());

        }

        break;

    }

    0

}

/// Attachment record: `out_id`, `out_bone`, `out_pos` (3 floats). Returns 1

/// on success, 0 on miss.

///

/// An M2Attachment is 48 bytes: `id` u32 @0, `bone` u32 @4, `position` C3

/// @8. The position is converted to UE axes (same `fixCoordSystem` as every

/// other export).

#[no_mangle]

pub extern "C" fn bn_model_attachment_info(

    world: *const BnWorld,

    model_path: *const c_char,

    idx: u32,

    out_id: *mut u16,

    out_bone: *mut u16,

    out_pos: *mut f32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out_id.is_null()

        || out_bone.is_null()

        || out_pos.is_null()

    {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            let (count, ofs) = match m2_attachment_array(bytes.as_slice()) {

                Some(v) => v,

                None => return 0,

            };

            if idx >= count {

                return 0;

            }

            let rec = ofs as usize + idx as usize * 48;

            let id = rd_u32(&bytes, rec).unwrap_or(0) as u16;

            let bone = rd_u32(&bytes, rec + 4).unwrap_or(0) as u16;

            let px = rd_f32(&bytes, rec + 8).unwrap_or(0.0);

            let py = rd_f32(&bytes, rec + 12).unwrap_or(0.0);

            let pz = rd_f32(&bytes, rec + 16).unwrap_or(0.0);

            let (qx, qy, qz) = wow_vec_to_ue(px, py, pz);

            unsafe {

                *out_id = id;

                *out_bone = bone;

                *out_pos = qx;

                *out_pos.add(1) = qy;

                *out_pos.add(2) = qz;

            }

            return 1;

        }

        break;

    }

    0

}

/// Number of skeleton bones in the model.

#[no_mangle]

pub extern "C" fn bn_model_bone_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_bones_array(bytes.as_slice()).map(|(c, _)| c).unwrap_or(0);

        }

        break;

    }

    0

}

/// Rest-pose world position of bone `idx` in UE axes (Y-flipped). Returns 1

/// on success, 0 on miss.

#[no_mangle]

pub extern "C" fn bn_model_bone_rest_position(

    world: *const BnWorld,

    model_path: *const c_char,

    idx: u32,

    out: *mut f32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Some(poses) = m2_bone_rest_positions(bytes.as_slice()) {

                if (idx as usize) < poses.len() {

                    let v = poses[idx as usize];

                    let (qx, qy, qz) = wow_vec_to_ue(v[0], v[1], v[2]);

                    unsafe {

                        *out = qx;

                        *out.add(1) = qy;

                        *out.add(2) = qz;

                    }

                    return 1;

                }

            }

        }

        break;

    }

    0

}

// ---- M2 skeletal animation (Phase 3) ----

/// A row-major 4x4 matrix, 16 floats.

type BnMat4 = [f32; 16];

/// A per-sequence / gseq M2Vec3 (translation or scale) track.

#[derive(Clone)]

struct F32Track {

    interp: u16,

    seq: i16,          // -1 = per-sequence timeline, >=0 = gseq index

    ranges: Vec<(u32, u32)>, // one (lo,hi) key-index window per sequence

    times: Vec<u32>,

    keys: Vec<[f32; 3]>,

}

impl F32Track {

    fn parse(b: &[u8], ofs: usize, anim_count: u32) -> Self {

        let interp = rd_u16(b, ofs).unwrap_or(0);

        let seq = rd_i16(b, ofs + 2).unwrap_or(-1);

        let n_ranges = rd_u32(b, ofs + 4).unwrap_or(0);

        let ofs_ranges = rd_u32(b, ofs + 8).unwrap_or(0) as usize;

        let n_times = rd_u32(b, ofs + 12).unwrap_or(0);

        let ofs_times = rd_u32(b, ofs + 16).unwrap_or(0) as usize;

        let n_keys = rd_u32(b, ofs + 20).unwrap_or(0);

        let ofs_keys = rd_u32(b, ofs + 24).unwrap_or(0) as usize;

        let _ = anim_count;

        let mut ranges = Vec::with_capacity(n_ranges as usize);

        for i in 0..n_ranges as usize {

            let lo = rd_u32(b, ofs_ranges + i * 8).unwrap_or(0);

            let hi = rd_u32(b, ofs_ranges + i * 8 + 4).unwrap_or(0);

            ranges.push((lo, hi));

        }

        let mut times = Vec::with_capacity(n_times as usize);

        for i in 0..n_times as usize {

            times.push(rd_u32(b, ofs_times + i * 4).unwrap_or(0));

        }

        // keys: first n_keys u32 times, then n_keys vec3 values (12 bytes each)

        let n = n_keys as usize;

        let mut keys = Vec::with_capacity(n);

        for i in 0..n {

            let _ = rd_u32(b, 0); // key block has no time prefix; times[] is separate

            let vofs = ofs_keys + i * 12;

            keys.push([

                rd_f32(b, vofs).unwrap_or(0.0),

                rd_f32(b, vofs + 4).unwrap_or(0.0),

                rd_f32(b, vofs + 8).unwrap_or(0.0),

            ]);

        }

        F32Track { interp, seq, ranges, times, keys }

    }

    /// Sample at (anim_index, time_ms), mirroring Animated::getValue.

    /// `global_time_ms` is the model's global clock (used for gseq tracks).

    /// Returns Option (None = track unused / no keys).

    fn sample(

        &self,

        anim: usize,

        time_ms: u32,

        global_time_ms: u32,

        global_seqs: &[u32],

    ) -> Option<[f32; 3]> {

        if self.keys.is_empty() {

            return None;

        }

        if self.interp == 0 {

            // INTERPOLATION_NONE: static default value

            return Some(self.keys[0]);

        }

        let (lo, hi): (usize, usize) = if self.seq != -1 {

            // gseq track: wrap on the global sequence clock

            let gsi = self.seq as usize;

            let dur = global_seqs.get(gsi).copied().unwrap_or(0);

            let t = if dur > 0 { global_time_ms % dur } else { 0 };

            let _ = t;

            (0, self.keys.len() - 1)

        } else {

            // per-sequence track: use ranges[anim] as key window

            let (rlo, rhi) = if anim < self.ranges.len() {

                (

                    self.ranges[anim].0 as usize,

                    self.ranges[anim].1 as usize,

                )

            } else if !self.ranges.is_empty() {

                // out-of-range anim: fall back to full range

                (0, self.keys.len() - 1)

            } else {

                // no ranges at all: full key range

                (0, self.keys.len() - 1)

            };

            (rlo.min(self.keys.len() - 1), rhi.min(self.keys.len() - 1))

        };

        if lo == hi {

            return Some(self.keys[lo.min(self.keys.len() - 1)]);

        }

        // Find bracketing keys within [lo, hi).

        // Reference per-sequence semantics (Animated::getValue, animated.h):

        //   time %= times[times.size()-1];  // wrap on the FULL absolute axis

        //   then linear-scan [lo, hi) for the first bracket key.

        // `time_ms` arrives as the clip's absolute timeline time (timeStart + offset).

        let total_axis = self.times.last().copied().unwrap_or(0);

        let time_abs = if total_axis > 0 { time_ms % total_axis } else { time_ms };

        let mut found = lo.min(self.times.len().saturating_sub(1));

        let search_hi = hi.min(self.times.len().saturating_sub(1));

        for i in lo..search_hi {

            let ti = self.times.get(i).copied().unwrap_or(0);

            let ti1 = self.times.get(i + 1).copied().unwrap_or(0);

            if time_abs >= ti && time_abs < ti1 {

                found = i;

                break;

            }

        }

        if found >= search_hi {

            // No bracket found inside [lo, hi) - clamp to the last bracketable key.

            found = search_hi.saturating_sub(1).max(lo);

        }

        let i0 = found.min(self.keys.len().saturating_sub(2));

        let i1 = (i0 + 1).min(self.keys.len().saturating_sub(1));

        let t0 = self.times.get(i0).copied().unwrap_or(0);

        let t1 = self.times.get(i1).copied().unwrap_or(0);

        let t = if t1 > t0 {

            (time_abs.saturating_sub(t0)) as f32 / (t1 - t0) as f32

        } else {

            0.0

        };

        let a = &self.keys[i0];

        let b = &self.keys[i1];

        match self.interp {

            1 => Some([

                a[0] + (b[0] - a[0]) * t,

                a[1] + (b[1] - a[1]) * t,

                a[2] + (b[2] - a[2]) * t,

            ]),

            _ => Some(*a),

        }

    }

}

/// A single M2QuatTrack.

#[derive(Clone)]

struct QuatTrack {

    interp: u16,

    seq: i16,

    ranges: Vec<(u32, u32)>,

    times: Vec<u32>,

    keys: Vec<[f32; 4]>,

}

impl QuatTrack {

    fn parse(b: &[u8], ofs: usize) -> Self {

        let interp = rd_u16(b, ofs).unwrap_or(0);

        let seq = rd_i16(b, ofs + 2).unwrap_or(-1);

        let n_ranges = rd_u32(b, ofs + 4).unwrap_or(0);

        let ofs_ranges = rd_u32(b, ofs + 8).unwrap_or(0) as usize;

        let n_times = rd_u32(b, ofs + 12).unwrap_or(0);

        let ofs_times = rd_u32(b, ofs + 16).unwrap_or(0) as usize;

        let n_keys = rd_u32(b, ofs + 20).unwrap_or(0);

        let ofs_keys = rd_u32(b, ofs + 24).unwrap_or(0) as usize;

        let mut ranges = Vec::with_capacity(n_ranges as usize);

        for i in 0..n_ranges as usize {

            let lo = rd_u32(b, ofs_ranges + i * 8).unwrap_or(0);

            let hi = rd_u32(b, ofs_ranges + i * 8 + 4).unwrap_or(0);

            ranges.push((lo, hi));

        }

        let mut times = Vec::with_capacity(n_times as usize);

        for i in 0..n_times as usize {

            times.push(rd_u32(b, ofs_times + i * 4).unwrap_or(0));

        }

        let n = n_keys as usize;

        let mut keys = Vec::with_capacity(n);

        for i in 0..n {

            let _ = rd_u32(b, 0); // key block has no time prefix

            let vofs = ofs_keys + i * 16;

            {

                let qx = rd_f32(b, vofs).unwrap_or(0.0);

                let qy = -rd_f32(b, vofs + 4).unwrap_or(0.0); // Y-flip at parse (fixCoordSystemQuat)

                let qz = rd_f32(b, vofs + 8).unwrap_or(0.0);

                let qw = rd_f32(b, vofs + 12).unwrap_or(0.0);

                keys.push([qx, qy, qz, qw]);

            }

        }

        QuatTrack { interp, seq, ranges, times, keys }

    }

    fn sample(

        &self,

        anim: usize,

        time_ms: u32,

        global_time_ms: u32,

        global_seqs: &[u32],

    ) -> Option<[f32; 4]> {

        if self.keys.is_empty() {

            return None;

        }

        if self.interp == 0 {

            return Some(self.keys[0]);

        }

        let (lo, hi) = if self.seq != -1 {

            let gsi = self.seq as usize;

            let dur = global_seqs.get(gsi).copied().unwrap_or(0);

            let _t = if dur > 0 { global_time_ms % dur } else { 0 };

            (0, self.keys.len() - 1)

        } else {

            let (rlo, rhi) = if anim < self.ranges.len() {

                (self.ranges[anim].0 as usize, self.ranges[anim].1 as usize)

            } else {

                (0, self.keys.len() - 1)

            };

            (rlo.min(self.keys.len() - 1), rhi.min(self.keys.len() - 1))

        };

        if lo == hi {

            return Some(self.keys[lo.min(self.keys.len() - 1)]);

        }

        // Reference per-sequence semantics (Animated::getValue, animated.h):

        //   time %= times[times.size()-1];  // wrap on the FULL absolute axis

        //   then linear-scan [lo, hi) for the first bracket key.

        // `time_ms` arrives as the clip's absolute timeline time (timeStart + offset).

        let total_axis = self.times.last().copied().unwrap_or(0);

        let time_abs = if total_axis > 0 { time_ms % total_axis } else { time_ms };

        let mut found = lo.min(self.times.len().saturating_sub(1));

        let search_hi = hi.min(self.times.len().saturating_sub(1));

        for i in lo..search_hi {

            let ti = self.times.get(i).copied().unwrap_or(0);

            let ti1 = self.times.get(i + 1).copied().unwrap_or(0);

            if time_abs >= ti && time_abs < ti1 {

                found = i;

                break;

            }

        }

        if found >= search_hi {

            // No bracket found inside [lo, hi) - clamp to the last bracketable key.

            found = search_hi.saturating_sub(1).max(lo);

        }

        let i0 = found.min(self.keys.len().saturating_sub(2));

        let i1 = (i0 + 1).min(self.keys.len().saturating_sub(1));

        let t0 = self.times.get(i0).copied().unwrap_or(0);

        let t1 = self.times.get(i1).copied().unwrap_or(0);

        let t = if t1 > t0 {

            (time_abs.saturating_sub(t0)) as f32 / (t1 - t0) as f32

        } else {

            0.0

        };

        let a = &self.keys[i0];

        let b = &self.keys[i1];

        Some(quat_slerp(t, *a, *b))

    }

}

fn quat_slerp(r: f32, a: [f32; 4], b: [f32; 4]) -> [f32; 4] {

    let dot = a[0]*b[0] + a[1]*b[1] + a[2]*b[2] + a[3]*b[3];

    let dot2 = dot.abs();

    if dot2 > 0.9995 {

        let x = a[0]*(1.0-r) + b[0]*r;

        let y = a[1]*(1.0-r) + b[1]*r;

        let z = a[2]*(1.0-r) + b[2]*r;

        let w = a[3]*(1.0-r) + b[3]*r;

        let m = (x*x+y*y+z*z+w*w).sqrt();

        if m < 1e-8 { return [0.0, 0.0, 0.0, 1.0]; }

        return [x/m, y/m, z/m, w/m];

    }

    let theta = dot.acos();

    let sin_theta = theta.sin();

    if sin_theta < 1e-8 { return a; }

    let sa = ((1.0 - r) * theta).sin() / sin_theta;

    let sb = (r * theta).sin() / sin_theta;

    let x = a[0]*sa + b[0]*sb;

    let y = a[1]*sa + b[1]*sb;

    let z = a[2]*sa + b[2]*sb;

    let w = a[3]*sa + b[3]*sb;

    let m = (x*x+y*y+z*z+w*w).sqrt();

    if m < 1e-8 { return [0.0, 0.0, 0.0, 1.0]; }

    [x/m, y/m, z/m, w/m]

}

fn quat_to_mat4(q: [f32; 4]) -> BnMat4 {

    // Reference newQuatRotate (matrix.h L102-110), ROW-MAJOR m[row][col].

    let x = q[0]; let y = q[1]; let z = q[2]; let w = q[3];

    let xx = x*x; let yy = y*y; let zz = z*z;

    let xy = x*y; let xz = x*z; let yz = y*z;

    let wx = w*x; let wy = w*y; let wz = w*z;

    [

        1.0 - 2.0*(yy+zz), 2.0*(xy+wz), 2.0*(xz-wy), 0.0,

        2.0*(xy-wz), 1.0 - 2.0*(xx+zz), 2.0*(yz+wx), 0.0,

        2.0*(xz+wy), 2.0*(yz-wx), 1.0 - 2.0*(xx+yy), 0.0,

        0.0, 0.0, 0.0, 1.0,

    ]

}

fn mat4_mul(a: &BnMat4, b: &BnMat4) -> BnMat4 {

    // ROW-MAJOR: result[r*4+c] = sum_k a[r*4+k]*b[k*4+c]

    let mut r = [0.0f32; 16];

    for ri in 0..4 {

        for ci in 0..4 {

            let mut s = 0.0f32;

            for k in 0..4 {

                s += a[ri * 4 + k] * b[k * 4 + ci];

            }

            r[ri * 4 + ci] = s;

        }

    }

    r

}

/// A single M2 bone parsed from a 108-byte record.

struct BnBone {

    parent: i16,

    pivot: [f32; 3],

    trans: F32Track,

    rot: QuatTrack,

    scale: F32Track,

}

/// Parse all M2 bones from raw M2 bytes.

fn parse_m2_bones(bytes: &[u8]) -> Option<Vec<BnBone>> {

    let (count, ofs) = m2_bones_array(bytes)?;

    let anim_count = m2_anim_array(bytes).map(|(c, _)| c).unwrap_or(0);

    let avail = bytes.len().saturating_sub(ofs as usize);

    let capped = std::cmp::min(count as usize, avail / 108);

    let mut out = Vec::with_capacity(capped);

    for i in 0..capped {

        let rec = ofs as usize + i * 108;

        let parent = rd_u16(bytes, rec + 8)? as i16;

        let mut px = rd_f32(bytes, rec + 96)?;

        let mut py = rd_f32(bytes, rec + 100)?;

        let mut pz = rd_f32(bytes, rec + 104)?;

        if px.is_nan() { px = 0.0; }

        if py.is_nan() { py = 0.0; }

        if pz.is_nan() { pz = 0.0; }

        // fixCoordSystem: (x, -y, z)

        let pivot = [px, -py, pz];

        let trans = F32Track::parse(bytes, rec + 12, anim_count);

        let rot = QuatTrack::parse(bytes, rec + 40);

        let scale = F32Track::parse(bytes, rec + 68, anim_count);

        out.push(BnBone { parent, pivot, trans, rot, scale });

    }

    Some(out)

}

/// Compute the model-space 4x4 matrix for bone `i` at (anim_index, time_ms).

/// Mirrors the reference's Bone::calcMatrix exactly:

///   m = T(pivot) * T(trans) * R(rot) * S(scale) * T(-pivot)

/// All post-multiplies in row-major; the pivot sandwich makes a rotated bone

/// rotate about its pivot (not the model origin). Pivots are pre-flipped to

/// UE axes (x, -y, z) at parse time; trans/quat are flipped here to match.

fn bone_matrix_at(

    bones: &[BnBone],

    anim: usize,

    time_ms: u32,

    global_time_ms: u32,

    global_seqs: &[u32],

    i: usize,

) -> BnMat4 {

    let b = &bones[i];

    let mut m: BnMat4 = [

        1.0, 0.0, 0.0, 0.0,

        0.0, 1.0, 0.0, 0.0,

        0.0, 0.0, 1.0, 0.0,

        0.0, 0.0, 0.0, 1.0,

    ];

    // Always build T(pivot) * T(trans) * R(rot) * S(scale) * T(-pivot).
    // The previous guard (`interp != 0`) skipped the pivot sandwich AND the
    // static-key fallback whenever a track used INTERPOLATION_NONE — every
    // RidingHorse bone has interp==0 rot tracks, so every bone degenerated
    // to the identity and the skin never moved.

    // m = T(pivot). Row-major: translation in m[0][3]=idx3, m[1][3]=idx7, m[2][3]=idx11.

    m[3] = b.pivot[0];

    m[7] = b.pivot[1];

    m[11] = b.pivot[2];

    // * T(trans)

    if let Some(t) = b.trans.sample(anim, time_ms, global_time_ms, global_seqs) {

        m[3] += t[0];

        m[7] -= t[1];

        m[11] += t[2];

    }

    // * R(rot)

    if let Some(q) = b.rot.sample(anim, time_ms, global_time_ms, global_seqs) {

        let qf = [q[0], q[1], q[2], q[3]];

        let rm = quat_to_mat4(qf);

        m = mat4_mul(&m, &rm);

    }

    // * S(scale)

    if let Some(s) = b.scale.sample(anim, time_ms, global_time_ms, global_seqs) {

        let sm: BnMat4 = [

            s[0], 0.0, 0.0, 0.0,

            0.0, s[1], 0.0, 0.0,

            0.0, 0.0, s[2], 0.0,

            0.0, 0.0, 0.0, 1.0,

        ];

        m = mat4_mul(&m, &sm);

    }

    // * T(-pivot)  post-multiply, matching reference model.cpp:

    {

        let px = b.pivot[0]; let py = b.pivot[1]; let pz = b.pivot[2];

        let tneg: BnMat4 = [

            1.0, 0.0, 0.0, -px,

            0.0, 1.0, 0.0, -py,

            0.0, 0.0, 1.0, -pz,

            0.0, 0.0, 0.0, 1.0,

        ];

        m = mat4_mul(&m, &tneg);

    }

    if b.parent >= 0 && (b.parent as usize) < bones.len() {

        let pmat =

            bone_matrix_at(bones, anim, time_ms, global_time_ms, global_seqs, b.parent as usize);

        m = mat4_mul(&pmat, &m);

    }

    m

}

/// All bones' model-space 4x4 matrices at (anim, time_ms).

pub fn m2_bone_matrices(

    bytes: &[u8],

    anim: usize,

    time_ms: u32,

    global_time_ms: u32,

    global_seqs: &[u32],

) -> Option<Vec<BnMat4>> {

    let bones = parse_m2_bones(bytes)?;

    Some((0..bones.len())

        .map(|i| bone_matrix_at(&bones, anim, time_ms, global_time_ms, global_seqs, i))

        .collect())

}

/// Number of bones in an M2 file.

pub fn m2_bone_count(bytes: &[u8]) -> u32 {

    m2_bones_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Number of animation sequences in an M2 file.

pub fn m2_anim_count(bytes: &[u8]) -> u32 {

    m2_anim_array(bytes).map(|(c, _)| c).unwrap_or(0)

}

/// Animation sequence `idx`: (time_start_ms, time_end_ms, duration_ms).

pub fn m2_anim_time_range(

    bytes: &[u8],

    idx: u32,

) -> Option<(u32, u32, u32)> {

    let (count, ofs) = m2_anim_array(bytes)?;

    if idx >= count {

        return None;

    }

    let rec = ofs as usize + idx as usize * 40;

    let ts = rd_u32(bytes, rec + 4)?;

    let te = rd_u32(bytes, rec + 8)?;

    let dur = te.saturating_sub(ts);

    Some((ts, te, dur))

}

/// Global sequence durations in ms.

pub fn m2_global_sequences(bytes: &[u8]) -> Vec<u32> {

    parse_m2_global_sequences(bytes)

}

/// Skinned vertex positions: for each vertex, blend the 4 bone matrices

/// applied to the vertex's local position. Returns one [f32; 3] per vertex

/// in WoW axes (x, y, z).

pub fn m2_skinned_positions(

    bytes: &[u8],

    anim: usize,

    time_ms: u32,

) -> Option<Vec<[f32; 3]>> {

    let fmt = benilla_m2::parse_m2(&mut std::io::Cursor::new(bytes)).ok()?;

    let model = fmt.model();

    let global_seqs = parse_m2_global_sequences(bytes);

    let bones = parse_m2_bones(bytes)?;

    // global_time_ms: for gseq tracks, use global clock.

    // The reference's animate() passes globalTime directly;

    // for gseq tracks the time is wrapped on the gseq duration inside sample().

    let global_time_ms = time_ms;

    let mats: Vec<BnMat4> = (0..bones.len())

        .map(|i| bone_matrix_at(&bones, anim, time_ms, global_time_ms, &global_seqs, i))

        .collect();

      let apply = |m: &BnMat4, v: [f32; 3]| -> [f32; 3] {

          // ROW-MAJOR: out.x = m[0][0]*v0 + m[0][1]*v1 + m[0][2]*v2 + m[0][3]

          [

              m[0] * v[0] + m[1] * v[1] + m[2] * v[2] + m[3],

              m[4] * v[0] + m[5] * v[1] + m[6] * v[2] + m[7],

              m[8] * v[0] + m[9] * v[1] + m[10] * v[2] + m[11],

          ]

      };

    let mut out = Vec::with_capacity(model.vertices.len());

    for v in model.vertices.iter() {

        // fixCoordSystem: vertex local position in UE axes (x, -y, z),

        // matching model.cpp initCommon where origVertices[i].pos = fixCoordSystem(pos).

        // Bone matrices already operate in UE axes (pivots Y-flipped), so the

        // vertex MUST be in the same coordinate system before the skin blend.

        let local = [v.position.x, -v.position.y, v.position.z];

        let idxs = [

            v.bone_indices[0] as usize,

            v.bone_indices[1] as usize,

            v.bone_indices[2] as usize,

            v.bone_indices[3] as usize,

        ];

        let ws = [

            v.bone_weights[0] as f32 / 255.0,

            v.bone_weights[1] as f32 / 255.0,

            v.bone_weights[2] as f32 / 255.0,

            v.bone_weights[3] as f32 / 255.0,

        ];

          // Reference model.cpp L730: v += mat*pos * (w/255). M2 weights are

          // authored to sum to 255, so this already equals the standard blend.

          // Fall back to the raw (already UE-axes) local position if a vertex has

          // no weights at all.

          let total: f32 = ws.iter().sum();

          if total < 1e-6 {

              out.push(local);

              continue;

          }

          let mut acc = [0.0f32; 3];

          for k in 0..4 {

              let bi = idxs[k];

              if bi < mats.len() {

                  let sk = apply(&mats[bi], local);

                  for j in 0..3 {

                      acc[j] += ws[k] * sk[j];

                  }

              }

          }

          out.push(acc);

    }

    Some(out)

}

/// Skinned vertex positions **in UE axes** (x, -y, z), matching the

/// existing `bn_model_fill` coordinate convention.

pub fn m2_skinned_positions_ue(

    bytes: &[u8],

    anim: usize,

    time_ms: u32,

) -> Option<Vec<[f32; 3]>> {

    // m2_skinned_positions now returns UE axes directly (the vertex local

    // position is Y-flipped at the skin-blend input, matching model.cpp

    // initCommon). No additional Y flip here.

    m2_skinned_positions(bytes, anim, time_ms)

}

/// Number of bones in the model's M2 file.

#[no_mangle]

pub extern "C" fn bn_model_skeleton_bone_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_bone_count(bytes.as_slice());

        }

    }

    0

}

/// Number of animation sequences in the model.

#[no_mangle]

pub extern "C" fn bn_model_anim_count(

    world: *const BnWorld,

    model_path: *const c_char,

) -> u32 {

    if world.is_null() || model_path.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            return m2_anim_count(bytes.as_slice());

        }

    }

    0

}

/// Animation sequence info: writes (time_start, time_end, duration) in ms

/// into `out` (3 u32 slots). Returns 1 on success, 0 on miss.

#[no_mangle]

pub extern "C" fn bn_model_anim_time_range(

    world: *const BnWorld,

    model_path: *const c_char,

    anim_idx: u32,

    out: *mut u32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Some((ts, te, dur)) = m2_anim_time_range(bytes.as_slice(), anim_idx) {

                unsafe {

                    *out = ts;

                    *out.add(1) = te;

                    *out.add(2) = dur;

                }

                return 1;

            }

        }

    }

    0

}

/// Temporary diagnostic: dump the rotation-track header for every bone of the

/// walk-clip sequence so we can see why the sampler folds it to a static pose.

/// out receives 6 u32 slots per bone: interp, seq, n_ranges, n_times, n_keys,

/// range_hi_for_anim. New FFI middle-layer code only; no original code touched.

#[no_mangle]

pub extern "C" fn bn_model_diag_walking_track(

    world: *const BnWorld,

    model_path: *const c_char,

    anim_idx: u32,

    out: *mut u32,

    cap: u32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Some(bones) = parse_m2_bones(bytes.as_slice()) {

                let mut n = 0u32;

                for b in bones.iter() {

                    if n >= cap {

                        break;

                    }

                    let r = &b.rot;

                    let hi = if (anim_idx as usize) < r.ranges.len() {

                        r.ranges[anim_idx as usize].1

                    } else { 0xFFFF_FFFF };

                    unsafe {

                        out.add((n as usize) * 6 + 0).write_volatile(r.interp as u32);

                        out.add((n as usize) * 6 + 1).write_volatile((r.seq as i16) as u32);

                        out.add((n as usize) * 6 + 2).write_volatile(r.ranges.len() as u32);

                        out.add((n as usize) * 6 + 3).write_volatile(r.times.len() as u32);

                        out.add((n as usize) * 6 + 4).write_volatile(r.keys.len() as u32);

                        out.add((n as usize) * 6 + 5).write_volatile(hi);

                    }

                    n += 1;

                }

                return n;

            }

            break;

        }

    }

    0

}

/// Diagnostic: dump the ranges + times first/last of a single bone's rotation

/// track. `out` receives: [0]=n_ranges, [1]=times[0], [2]=times[last],

/// then pairs (lo, hi) per range. Returns count written.

#[no_mangle]

pub extern "C" fn bn_model_diag_ranges(

    world: *const BnWorld,

    model_path: *const c_char,

    bone_idx: u32,

    out: *mut u32,

    cap: u32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Some(bones) = parse_m2_bones(bytes.as_slice()) {

                if let Some(b) = bones.get(bone_idx as usize) {

                    let r = &b.rot;

                    unsafe {

                        out.add(0).write_volatile(r.ranges.len() as u32);

                        out.add(1).write_volatile(r.times.first().copied().unwrap_or(0));

                        out.add(2).write_volatile(r.times.last().copied().unwrap_or(0));

                    }

                    let mut n = 3u32;

                    for (i, wpair) in r.ranges.iter().enumerate() {

                        if 3 + 2u32 * (i as u32) + 2 > cap {

                            break;

                        }

                        unsafe {

                            out.add(n as usize).write_volatile(wpair.0);

                            out.add(n as usize + 1).write_volatile(wpair.1);

                        }

                        n += 2;

                    }

                    return n;

                }

                break;

            }

            break;

        }

    }

    0

}

/// Skinned vertex positions in **UE axes** (x, -y, z) at `time_ms` of

/// animation sequence `anim_idx`. `out` must hold `vertex_count * 3` floats.

/// Returns the number of vertices written (0 on miss).

#[no_mangle]

pub extern "C" fn bn_model_skinned_positions(

    world: *const BnWorld,

    model_path: *const c_char,

    anim_idx: u32,

    time_ms: u32,

    out: *mut f32,

    cap: u32,

) -> u32 {

    if world.is_null() || model_path.is_null() || out.is_null() {

        return 0;

    }

    let p = match cstr_to_str(model_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Some(verts) = m2_skinned_positions_ue(

                bytes.as_slice(),

                anim_idx as usize,

                time_ms,

            ) {

                let n = verts.len().min(cap as usize);

                for (i, v) in verts.iter().take(n).enumerate() {

                    unsafe {

                        *out.add(i * 3) = v[0];

                        *out.add(i * 3 + 1) = v[1];

                        *out.add(i * 3 + 2) = v[2];

                    }

                }

                return n as u32;

            }

        }

    }

    0

}

// ---- BLP textures ----

/// Decode a BLP texture from the chain into RGBA8.

/// `out_rgba` must hold `out_width * out_height * 4` bytes.

/// Returns bytes written (0 on miss / unknown compression).

#[no_mangle]

pub extern "C" fn bn_texture(

    world: *const BnWorld,

    internal_path: *const c_char,

    out_width: *mut u32,

    out_height: *mut u32,

    out_rgba: *mut u8,

    cap: u32,

) -> u32 {

    if world.is_null() || internal_path.is_null()

        || out_rgba.is_null()

        || out_width.is_null()

        || out_height.is_null()

    {

        return 0;

    }

    let p = match cstr_to_str(internal_path) {

        Some(s) => s,

        None => return 0,

    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    for a in w.archives.iter().rev() {

        if !a.contains(p) {

            continue;

        }

        if let Ok(bytes) = a.read_file(p) {

            if let Ok(decoded) = benilla_blp::decode(bytes.as_slice()) {

                let Some(mip) = decoded.mips.first() else {

                    continue;

                };

                let need = (mip.width * mip.height * 4) as u32;

                if need > cap {

                    return 0;

                }

                unsafe {

                    *out_width = mip.width;

                    *out_height = mip.height;

                    std::ptr::copy_nonoverlapping(

                        mip.rgba.as_ptr(),

                        out_rgba,

                        need as usize,

                    );

                }

                return need;

            }

        }

        break;

    }

    0

}


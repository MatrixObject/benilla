// Benilla FFI — C ABI export layer for the UE5 render host.

//

// Keeps all WoW 1.12.1 format parsing in Rust (MPQ/ADT/M2/WMO/BLP, no Bevy,

// no app, no networking); exposes the parsed geometry and decoded textures

// through plain C structs so the UE `BnRenderer` module can rebuild meshes

// and upload textures without depending on any Rust type.

//

// Built as a cdylib (`benilla.dll`); the UE side declares the same

// functions `extern "C"` and links against the .dll.

use std::collections::HashMap;

use std::ffi::c_char;

// ---- WoW <-> UE5 coordinate-system converter (single source of truth) ----

mod coords;

// ---- WoW FontString rasterization (client TTFs -> RGBA for the UE HUD) ----

mod ui_font;

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

    // u32, not u16: a single WMO instance bakes up to ~840k verts (Stormwind),
    // so u16 indices silently truncated every building over 65535 verts.
    pub a: u32,

    pub b: u32,

    pub c: u32,

    pub material: u8,

    /// bit 0x01 = two-sided (M2 material render flag 0x04): don't back-face cull.
    pub flags: u8,

}

/// Opaque handle to the Rust-side state (MPQ chain + loaded tiles).

pub struct BnWorld {

    archives: Vec<Archive>,

    mcnk: Vec<McnkChunk>,

    /// Real ADT terrain of the last `bn_load_tile`, when one loaded.
    tile: Option<TileFlat>,

    /// Lowercase ADT path backing `tile` (and `mcnk`), for the same-tile
    /// reload fast path: per-frame height queries must not re-parse the ADT.
    tile_path: String,

    /// Official `benilla_formats::Chain` over the same Data dir, for `load_wmo`
    /// (the WMO reader takes a `Chain`, not the raw `Archive` list).
    chain: Option<benilla_formats::Chain>,

    /// Parsed WMO geometry by root path, shared by every placement of a model.
    wmo_models: HashMap<String, Vec<benilla_formats::RenderSubmesh>>,

    /// Baked tile-local WMO geometry by instance index (rebuilt on tile load).
    wmo_flats: HashMap<u32, WmoFlat>,

    /// Model-space group MLIQ (WMO liquid) meshes by root path: read once per
    /// model, re-placed per instance into the tile's liquid buffer.
    wmo_group_liquids: HashMap<String, Vec<benilla_formats::LiquidMesh>>,

    /// Parsed doodad (terrain M2) geometry by model path, shared by placements.
    doodad_models: HashMap<String, Vec<benilla_formats::RenderSubmesh>>,

    /// Baked tile-local doodad geometry by MDDF index (rebuilt on tile load).
    doodad_flats: HashMap<u32, WmoFlat>,

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

        tile: None,

        tile_path: String::new(),

        chain: benilla_formats::Chain::open(data).ok(),

        wmo_models: HashMap::new(),

        wmo_flats: HashMap::new(),

        wmo_group_liquids: HashMap::new(),

        doodad_models: HashMap::new(),

        doodad_flats: HashMap::new(),

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

/// One chunk's slot in the flattened tile buffers (element counts).
pub struct TerrainChunkRange {
    vtx0: u32,
    vtx_n: u32,
    idx0: u32,
    idx_n: u32,
    /// MTEX base texture of the chunk ("" when none).
    tex: String,
    /// MCLY layer textures (up to 4, index 0 = base = `tex`).
    layers: Vec<String>,
    /// Combined 64x64 RGBA alpha map for layers 1..3 (None with a single layer).
    alpha: Option<Vec<u8>>,
}

/// Flattened real ADT terrain, built once in `bn_load_tile` via the official
/// `benilla_formats::adt_to_tile_mesh` (read-only, no official source edits):
/// contiguous tile-local vertices + one global u16 index buffer, one range
/// per chunk. Also keeps the tile's WMO/doodad placements for later phases.
pub struct TileFlat {
    verts: Vec<BnVertex>,
    idx: Vec<u16>,
    chunks: Vec<TerrainChunkRange>,
    /// WMO instances (MODF) in raw WoW world coords.
    wmos: Vec<benilla_formats::WmoInstance>,
    /// M2 doodad placements (MDDF) in raw WoW world coords.
    doodads: Vec<benilla_formats::Doodad>,
    /// Tile centre in WoW world axes (X/Y only); verts are world - centre.
    center: (f32, f32),
    /// Flattened ADT liquid (MCLQ) surfaces: one vertex/index run per mesh,
    /// tile-local through the same WoW->UE mirror as the terrain.
    lverts: Vec<BnVertex>,
    lidx: Vec<u16>,
    liquids: Vec<LiquidRange>,
}

/// One liquid mesh's index range in the flattened liquid buffer and its
/// material class: 0 water (still/rapids), 1 ocean, 2 magma/slime.
struct LiquidRange {
    idx0: u32,
    idx_n: u32,
    kind: u32,
}

/// The liquid kind as a material class (benilla_formats::LiquidKind).
fn liquid_kind_code(k: benilla_formats::LiquidKind) -> u32 {
    use benilla_formats::LiquidKind::*;
    match k {
        Still | Rapids => 0,
        Ocean => 1,
        Magma | Slime => 2,
    }
}

/// Build the flattened tile from ADT bytes. Vertices become tile-local
/// (tile centre at origin, X/Y only; Z stays world-absolute) and then go
/// through the same WoW->UE mirror as every other mesh.
fn build_tile_flat(bytes: &[u8]) -> Option<TileFlat> {
    let tile = benilla_formats::adt_to_tile_mesh(bytes).ok()?;
    if tile.chunks.is_empty() {
        return None;
    }
    let (mut min_x, mut max_x) = (f32::MAX, f32::MIN);
    let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
    for c in &tile.chunks {
        for p in &c.positions {
            min_x = min_x.min(p[0]);
            max_x = max_x.max(p[0]);
            min_y = min_y.min(p[1]);
            max_y = max_y.max(p[1]);
        }
    }
    let center = ((min_x + max_x) * 0.5, (min_y + max_y) * 0.5);

    let mut out = TileFlat {
        verts: Vec::new(),
        idx: Vec::new(),
        chunks: Vec::new(),
        wmos: tile.wmos.clone(),
        doodads: tile.doodads.clone(),
        center,
        lverts: Vec::new(),
        lidx: Vec::new(),
        liquids: Vec::new(),
    };
    for c in &tile.chunks {
        // The UE builder uses u16 indices; never overflow the global buffer.
        if out.verts.len() + c.positions.len() > u16::MAX as usize {
            break;
        }
        let vtx0 = out.verts.len() as u32;
        for (i, p) in c.positions.iter().enumerate() {
            let (nx, ny, nz) = if c.normals.len() == c.positions.len() {
                wow_vec_to_ue(c.normals[i][0], c.normals[i][1], c.normals[i][2])
            } else {
                (0.0, 0.0, 1.0) // no MCNR: flat up (WoW +Z == UE +Z)
            };
            let (x, y, z) = wow_vec_to_ue(p[0] - center.0, p[1] - center.1, p[2]);
            let uv = c.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
            out.verts.push(BnVertex {
                x,
                y,
                z,
                nx,
                ny,
                nz,
                // Official layer tiling: base texture repeats 8x per chunk.
                u: uv[0] * benilla_formats::TERRAIN_LAYER_TILES,
                v: uv[1] * benilla_formats::TERRAIN_LAYER_TILES,
            });
        }
        let idx0 = out.idx.len() as u32;
        for &i in &c.indices {
            out.idx.push((vtx0 + i) as u16);
        }
        out.chunks.push(TerrainChunkRange {
            vtx0,
            vtx_n: c.positions.len() as u32,
            idx0,
            idx_n: out.idx.len() as u32 - idx0,
            tex: c.base_texture.clone().unwrap_or_default(),
            layers: c.layer_textures.clone(),
            alpha: c.alpha_map.clone(),
        });
    }
    // ADT liquid (MCLQ): own buffer, same tile-local + WoW->UE mirror as the
    // terrain; each wet mesh is one contiguous index range for one material.
    for c in &tile.chunks {
        for m in &c.liquids {
            if out.lverts.len() + m.positions.len() > u16::MAX as usize {
                break; // u16 builder limit; past any shipped tile
            }
            let vtx0 = out.lverts.len() as u32;
            for (i, p) in m.positions.iter().enumerate() {
                let (x, y, z) = wow_vec_to_ue(p[0] - center.0, p[1] - center.1, p[2]);
                let uv = m.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
                out.lverts.push(BnVertex {
                    x,
                    y,
                    z,
                    // No MCNR on liquid: flat up (WoW +Z == UE +Z).
                    nx: 0.0,
                    ny: 0.0,
                    nz: 1.0,
                    u: uv[0],
                    v: uv[1],
                });
            }
            let idx0 = out.lidx.len() as u32;
            for &i in &m.indices {
                out.lidx.push((vtx0 + i) as u16);
            }
            out.liquids.push(LiquidRange {
                idx0,
                idx_n: out.lidx.len() as u32 - idx0,
                kind: liquid_kind_code(m.kind),
            });
        }
    }
    Some(out)
}


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

    // Same-tile fast path: the host re-loads its tile before every height
    // query (the shared handle holds one active tile), so a repeat must not
    // re-parse the ADT - mcnk/tile/flats are all still valid for this path.
    let path_lower = path.to_ascii_lowercase();
    if w.tile.is_some() && w.tile_path == path_lower {
        return w.mcnk.len() as u32;
    }

    for a in w.archives.iter().rev() {

        if !a.contains(&path) {

            continue;

        }

        if let Ok(bytes) = a.read_file(&path) {

            if let Ok(parsed) = benilla_adt::parse_adt(&mut Cursor::new(bytes.as_slice())) {

                if let benilla_adt::ParsedAdt::Root(root) = parsed {

                    w.mcnk = root.mcnk_chunks;
                    w.tile = build_tile_flat(&bytes);
                    w.tile_path = path_lower;

                    // Instance indices address the new tile's MODF/MDDF lists;
                    // drop stale bakes (models stay cached: path-keyed).
                    w.wmo_flats.clear();
                    w.doodad_flats.clear();

                    return w.mcnk.len() as u32;

                }

            }

            break;

        }

    }

    0

}

// ---- streaming: the official residency window (benilla-world
// terrain_stream/window.rs reimplemented as a pure function, because
// benilla-world pulls in the whole bevy stack and cannot be a dependency
// here): inner = 1 + trunc(farclip / CHUNK_SIZE) chunks,
// outer = max(inner + 2, 8) chunks; a tile is wanted when any of its chunks
// lies in the outer window.

/// Chunks per tile edge (`chunk >> 4` is the tile); grid runs 0..=1023.
const STREAM_CHUNKS_PER_TILE: i32 = 16;
const STREAM_CHUNK_MAX: i32 = 64 * STREAM_CHUNKS_PER_TILE - 1;

/// Wanted tiles `(x, z)` around the WoW-space focus for `farclip` view
/// distance, filtered to tiles whose ADT exists in the archive chain (ocean
/// / never-authored tiles drop out). Writes pairs into `out` (capacity
/// `cap` pairs); returns the number of pairs written.
#[no_mangle]
pub extern "C" fn bn_stream_tiles(
    world: *const BnWorld,
    map_name: *const c_char,
    farclip: f32,
    wow_x: f32,
    wow_y: f32,
    out: *mut i32,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let Some(map) = cstr_to_str(map_name) else {
        return 0;
    };
    // SAFETY: caller guarantees a valid, exclusive handle and out buffer.
    let w = unsafe { &*world };

    let (fx, fy) = benilla_formats::world_to_chunk(wow_x, wow_y);
    let inner = 1 + (farclip / benilla_formats::CHUNK_SIZE).trunc() as i32;
    let outer = (inner + 2).max(8);
    let lo = |c: i32| (c - outer).clamp(0, STREAM_CHUNK_MAX) / STREAM_CHUNKS_PER_TILE;
    let hi = |c: i32| (c + outer).clamp(0, STREAM_CHUNK_MAX) / STREAM_CHUNKS_PER_TILE;
    let (x0, y0) = (lo(fx as i32), lo(fy as i32));
    let (x1, y1) = (hi(fx as i32), hi(fy as i32));

    let mut n = 0u32;
    for tx in x0..=x1 {
        for ty in y0..=y1 {
            if n >= cap {
                return n;
            }
            let path = format!("World\\Maps\\{map}\\{map}_{tx}_{ty}.adt");
            if !w.archives.iter().rev().any(|a| a.contains(&path)) {
                continue;
            }
            unsafe {
                *out.add(n as usize * 2) = tx;
                *out.add(n as usize * 2 + 1) = ty;
            }
            n += 1;
        }
    }
    n
}

/// The loaded tile's centre in WoW yards (pre-mirror). Writes `[cx, cy]`;
/// returns 2 on success, 0 when no tile is loaded. The UE side places the
/// tile actor at `(cx, -cy) * kWowUnitToCm` — vertices are tile-local with
/// the centre at the origin, mirrored and in the same units.
#[no_mangle]
pub extern "C" fn bn_tile_center(world: *const BnWorld, out: *mut f32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees a valid handle and a 2-float buffer.
    let w = unsafe { &*world };
    let Some(t) = w.tile.as_ref() else {
        return 0;
    };
    unsafe {
        *out = t.center.0;
        *out.add(1) = t.center.1;
    }
    2
}

/// Terrain vertex count the UE side should pre-allocate.

#[no_mangle]

pub extern "C" fn bn_terrain_vertex_count(world: *const BnWorld) -> u32 {

    if world.is_null() {

        return 0;

    }

    // SAFETY: valid const handle.

    let w = unsafe { &*world };

    w.tile.as_ref().map_or(0, |t| t.verts.len() as u32)

}

/// Terrain index count the UE side should pre-allocate.

#[no_mangle]

pub extern "C" fn bn_terrain_index_count(world: *const BnWorld) -> u32 {

    if world.is_null() {

        return 0;

    }

    let w = unsafe { &*world };

    w.tile.as_ref().map_or(0, |t| t.idx.len() as u32)

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

    let Some(t) = w.tile.as_ref() else { return 0 };

    let n = t.verts.len().min(cap as usize);

    // SAFETY: caller pre-allocated `cap` BnVertex slots at `out`.

    unsafe { std::ptr::copy_nonoverlapping(t.verts.as_ptr(), out, n) };

    n as u32

}

/// Fill `out` (pre-allocated) with terrain triangle indices. Returns count written.

#[no_mangle]

pub extern "C" fn bn_terrain_fill_indices(

    world: *const BnWorld,

    out: *mut u16,

    cap: u32,

) -> u32 {

    if world.is_null() || out.is_null() {

        return 0;

    }

    let w = unsafe { &*world };

    let Some(t) = w.tile.as_ref() else { return 0 };

    let n = t.idx.len().min(cap as usize);

    unsafe { std::ptr::copy_nonoverlapping(t.idx.as_ptr(), out, n) };

    n as u32

}

/// Liquid meshes (wet MCLQ surfaces) in the loaded tile (0 when no tile).
#[no_mangle]
pub extern "C" fn bn_liquid_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.liquids.len() as u32)
}

/// Range of liquid mesh `i`: writes {idx0, idx_n, kind} (kind: 0 water,
/// 1 ocean, 2 magma). Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_liquid_range(world: *const BnWorld, i: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(r) = w.tile.as_ref().and_then(|t| t.liquids.get(i as usize)) else {
        return 0;
    };
    unsafe {
        *out = r.idx0;
        *out.add(1) = r.idx_n;
        *out.add(2) = r.kind;
    }
    1
}

/// Liquid vertex count the UE side should pre-allocate.
#[no_mangle]
pub extern "C" fn bn_liquid_vertex_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.lverts.len() as u32)
}

/// Liquid index count the UE side should pre-allocate.
#[no_mangle]
pub extern "C" fn bn_liquid_index_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.lidx.len() as u32)
}

/// Fill `out` (pre-allocated) with liquid vertices. Returns count written.
#[no_mangle]
pub extern "C" fn bn_liquid_fill_vertices(
    world: *const BnWorld,
    out: *mut BnVertex,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(t) = w.tile.as_ref() else { return 0 };
    let n = t.lverts.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(t.lverts.as_ptr(), out, n) };
    n as u32
}

/// Fill `out` (pre-allocated) with liquid triangle indices. Returns count written.
#[no_mangle]
pub extern "C" fn bn_liquid_fill_indices(world: *const BnWorld, out: *mut u16, cap: u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(t) = w.tile.as_ref() else { return 0 };
    let n = t.lidx.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(t.lidx.as_ptr(), out, n) };
    n as u32
}

/// The animated liquid frame sets per material class (0 water, 1 ocean,
/// 2 magma): `XTextures\<dir>\<stem>.<1..=count>.blp`, the reference's fixed
/// queues (`lake_a` `0x6851b0`, `ocean_h` `0x685010`, `lava` `0x6855a0`),
/// mirroring `benilla-world`'s FRAME_SETS.
const LIQUID_FRAME_SETS: [(&str, &str, u32); 3] = [
    ("river", "lake_a", 30),
    ("ocean", "ocean_h", 30),
    ("lava", "lava", 30),
];

/// Scan `kind`'s frame set, writing {width, height, frame_count} to `out`
/// (stops at the first missing or mis-sized frame, as the official loader).
/// Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_liquid_frame_info(world: *mut BnWorld, kind: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let Some(&(dir, stem, max)) = LIQUID_FRAME_SETS.get(kind as usize) else {
        return 0;
    };
    // SAFETY: caller guarantees an exclusive valid handle.
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let mut wh = (0u32, 0u32);
    let mut count = 0u32;
    for i in 1..=max {
        let path = format!("XTextures\\{dir}\\{stem}.{i}.blp");
        let Ok((ww, hh, _)) = benilla_formats::read_texture_rgba(chain, &path) else {
            break;
        };
        if count == 0 {
            wh = (ww, hh);
        } else if (ww, hh) != wh {
            break;
        }
        count += 1;
    }
    if count == 0 {
        return 0;
    }
    unsafe {
        *out = wh.0;
        *out.add(1) = wh.1;
        *out.add(2) = count;
    }
    1
}

/// Decode `kind`'s frames into one single-row RGBA atlas (frame `f` over rows
/// `[f·h, (f+1)·h)`, so a mip chain stays frame-aligned). Returns bytes
/// written, 0 on a miss or cap overrun. `out` holds w·count·h·4 bytes.
#[no_mangle]
pub extern "C" fn bn_liquid_frame_atlas(
    world: *mut BnWorld,
    kind: u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let Some(&(dir, stem, max)) = LIQUID_FRAME_SETS.get(kind as usize) else {
        return 0;
    };
    // SAFETY: caller guarantees an exclusive valid handle and an out buffer
    // of `cap` bytes.
    let w = unsafe { &mut *world };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    let mut wh = (0u32, 0u32);
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for i in 1..=max {
        let path = format!("XTextures\\{dir}\\{stem}.{i}.blp");
        let Ok((ww, hh, rgba)) = benilla_formats::read_texture_rgba(chain, &path) else {
            break;
        };
        if frames.is_empty() {
            wh = (ww, hh);
        } else if (ww, hh) != wh {
            break;
        }
        if rgba.len() != ww as usize * hh as usize * 4 {
            break;
        }
        frames.push(rgba);
    }
    if frames.is_empty() {
        return 0;
    }
    let frame_bytes = wh.0 as usize * wh.1 as usize * 4;
    let total = frame_bytes * frames.len();
    if total > cap as usize {
        return 0;
    }
    for (f, rgba) in frames.iter().enumerate() {
        unsafe {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), out.add(f * frame_bytes), frame_bytes);
        }
    }
    total as u32
}

/// Per-chunk count of the loaded tile (0 when no tile).

#[no_mangle]

pub extern "C" fn bn_terrain_chunk_count(world: *const BnWorld) -> u32 {

    if world.is_null() {

        return 0;

    }

    let w = unsafe { &*world };

    w.tile.as_ref().map_or(0, |t| t.chunks.len() as u32)

}

/// Chunk `i` index range: writes {vtx0, vtx_n, idx0, idx_n}. Returns 1 on success.

#[no_mangle]

pub extern "C" fn bn_terrain_chunk_range(

    world: *const BnWorld,

    i: u32,

    out: *mut u32,

) -> u32 {

    if world.is_null() || out.is_null() {

        return 0;

    }

    let w = unsafe { &*world };

    let Some(c) = w.tile.as_ref().and_then(|t| t.chunks.get(i as usize)) else {

        return 0;

    };

    unsafe {

        *out = c.vtx0;

        *out.add(1) = c.vtx_n;

        *out.add(2) = c.idx0;

        *out.add(3) = c.idx_n;

    }

    1

}

/// Chunk `i` MTEX base texture name into `buf` (NUL-terminated). Returns 1 on success.

#[no_mangle]

pub extern "C" fn bn_terrain_chunk_texture(

    world: *const BnWorld,

    i: u32,

    buf: *mut c_char,

    cap: u32,

) -> u32 {

    if world.is_null() || buf.is_null() || cap == 0 {

        return 0;

    }

    let w = unsafe { &*world };

    let Some(c) = w.tile.as_ref().and_then(|t| t.chunks.get(i as usize)) else {

        return 0;

    };

    let bytes = c.tex.as_bytes();

    let n = bytes.len().min(cap as usize - 1);

    unsafe {

        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);

        *buf.add(n) = 0;

    }

    1

}

/// Chunk `i` MCLY layer count (0..4; 0 when no tile/chunk).
#[no_mangle]
pub extern "C" fn bn_terrain_chunk_layer_count(world: *const BnWorld, i: u32) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile
        .as_ref()
        .and_then(|t| t.chunks.get(i as usize))
        .map_or(0, |c| c.layers.len().min(4) as u32)
}

/// Chunk `i` layer `slot` texture path into `buf` (NUL-terminated). Returns 1
/// on success (path may be "" when the slot is empty).
#[no_mangle]
pub extern "C" fn bn_terrain_chunk_layer(
    world: *const BnWorld,
    i: u32,
    slot: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(c) = w.tile.as_ref().and_then(|t| t.chunks.get(i as usize)) else {
        return 0;
    };
    let Some(path) = c.layers.get(slot as usize) else {
        // Empty slot: write the NUL so callers see "".
        unsafe { *buf = 0 };
        return 1;
    };
    let bytes = path.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    1
}

/// Chunk `i` combined 64x64 RGBA alpha map (R/G/B = layers 1/2/3 blend
/// weights, A unused). Returns the byte count written (0 = single-layer
/// chunk, caller should bind a black alpha). Channels for absent layers are
/// forced to 0 so a partial layer stack blends correctly.
#[no_mangle]
pub extern "C" fn bn_terrain_chunk_alpha(
    world: *const BnWorld,
    i: u32,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(c) = w.tile.as_ref().and_then(|t| t.chunks.get(i as usize)) else {
        return 0;
    };
    let Some(alpha) = c.alpha.as_ref() else {
        return 0;
    };
    let n = (64 * 64 * 4).min(alpha.len()).min(cap as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(alpha.as_ptr(), out, n);
    }
    // R/G/B serve layers 1..3; zero the channels whose layer is absent.
    let layers = c.layers.len().min(4);
    for ch in (layers.saturating_sub(1))..3 {
        let mut k = ch;
        while k < n {
            unsafe { *out.add(k) = 0; }
            k += 4;
        }
    }
    n as u32
}

/// Terrain height (UE-local, WoW units) under tile-local (x, y): nearest

/// flattened vertex height minus tile centre Z offset (Z is world-absolute

/// in `verts`, so this returns the raw world Z in WoW units).

#[no_mangle]

pub extern "C" fn bn_terrain_height_at(world: *const BnWorld, x: f32, y: f32) -> f32 {

    if world.is_null() {

        return 0.0;

    }

    let w = unsafe { &*world };

    let Some(t) = w.tile.as_ref() else { return 0.0 };

    // verts are (world - center) through wow_vec_to_ue: X/Y are mirrored.

    // Invert the mirror to search in tile-local space.

    let (mut best, mut best_d) = (0.0f32, f32::MAX);

    for v in &t.verts {

        let d = (v.x - x) * (v.x - x) + (v.y - y) * (v.y - y);

        if d < best_d {

            best_d = d;

            best = v.z;

        }

    }

    best

}

// ---- WMO buildings (phase 2) ----
//
// Placement math ports `benilla-assets::coords::placement_rotation` (read-only,
// no official source edits): the wowdev `createPlacementMatrix`
// `Rx(90°)·Ry(ry−180°)·Rz(−rx)·Rx(rz−90°)` runs in WoW space, then the baked
// vertex goes through the same tile-local + WoW->UE mirror as the terrain.

/// One baked WMO submesh's slot in the flat buffers (element counts).
pub struct WmoSub {
    vtx0: u32,
    vtx_n: u32,
    idx0: u32,
    idx_n: u32,
    /// MOMT material texture path ("" when none).
    tex: String,
    /// MOMT flag 0x04: emit triangles two-sided (no back-face culling).
    two_sided: bool,
    /// Blend class (0..4, 0x100 = additive), `bn_doodad_blend`'s encoding.
    blend: u32,
}

/// One WMO instance baked into tile-local UE axes: `world = position + in_wow·v`,
/// then `(world − centre)` through the WoW->UE mirror, contiguous per submesh.
pub struct WmoFlat {
    verts: Vec<BnVertex>,
    idx: Vec<u32>,
    subs: Vec<WmoSub>,
}

/// Hamilton product `a·b` on `[x, y, z, w]` (apply `b` first, then `a`), the
/// convention bevy's `Quat * Quat` uses, so the port composes identically.
fn q_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

/// Rotate `v` by unit quaternion `q` (`t = 2·q.xyz × v` form).
fn q_rot(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let t = [
        2.0 * (q[1] * v[2] - q[2] * v[1]),
        2.0 * (q[2] * v[0] - q[0] * v[2]),
        2.0 * (q[0] * v[1] - q[1] * v[0]),
    ];
    [
        v[0] + q[3] * t[0] + q[1] * t[2] - q[2] * t[1],
        v[1] + q[3] * t[1] + q[2] * t[0] - q[0] * t[2],
        v[2] + q[3] * t[2] + q[0] * t[1] - q[1] * t[0],
    ]
}

fn q_rot_x(a: f32) -> [f32; 4] {
    let (s, c) = (a * 0.5).sin_cos();
    [s, 0.0, 0.0, c]
}

fn q_rot_y(a: f32) -> [f32; 4] {
    let (s, c) = (a * 0.5).sin_cos();
    [0.0, s, 0.0, c]
}

fn q_rot_z(a: f32) -> [f32; 4] {
    let (s, c) = (a * 0.5).sin_cos();
    [0.0, 0.0, s, c]
}

/// An MDDF/MODF Euler triple (degrees) as the placement quaternion, the port of
/// `benilla-assets::coords::placement_rotation` without the bevy conjugation
/// (we rotate in WoW space and mirror afterwards, not bake into a foreign basis).
fn placement_quat(rotation_deg: [f32; 3]) -> [f32; 4] {
    use std::f32::consts::{FRAC_PI_2, PI};
    let (rx, ry, rz) = (
        rotation_deg[0].to_radians(),
        rotation_deg[1].to_radians(),
        rotation_deg[2].to_radians(),
    );
    q_mul(
        q_rot_x(FRAC_PI_2),
        q_mul(q_rot_y(ry - PI), q_mul(q_rot_z(-rx), q_rot_x(rz - FRAC_PI_2))),
    )
}

/// Bake every submesh of one placement into tile-local UE axes. Submesh ranges
/// stay contiguous (vertex order = submesh order). Indices are u32: a city
/// WMO (Stormwind ~840k verts) far exceeds the old u16 vertex cap, which
/// dropped every submesh past 65535 verts. `scale` is the
/// MDDF uniform scale (WMO placements pass 1.0): applied to model-space
/// positions before rotation, matching the official placement order.
fn build_wmo_flat(
    subs: &[benilla_formats::RenderSubmesh],
    position: [f32; 3],
    rotation_deg: [f32; 3],
    center: (f32, f32),
    scale: f32,
    model_path: &str,
    chain: Option<&benilla_formats::Chain>,
) -> WmoFlat {
    let q = placement_quat(rotation_deg);
    let mut out = WmoFlat {
        verts: Vec::new(),
        idx: Vec::new(),
        subs: Vec::new(),
    };
    for sm in subs {
        let vtx0 = out.verts.len() as u32;
        for (i, p) in sm.positions.iter().enumerate() {
            let r = q_rot(q, [p[0] * scale, p[1] * scale, p[2] * scale]);
            let (wx, wy, wz) = (
                r[0] + position[0],
                r[1] + position[1],
                r[2] + position[2],
            );
            let (x, y, z) = wow_vec_to_ue(wx - center.0, wy - center.1, wz);
            let (nx, ny, nz) = if sm.normals.len() == sm.positions.len() {
                let n = q_rot(q, sm.normals[i]);
                wow_vec_to_ue(n[0], n[1], n[2])
            } else {
                (0.0, 0.0, 1.0)
            };
            let uv = sm.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
            out.verts.push(BnVertex {
                x,
                y,
                z,
                nx,
                ny,
                nz,
                u: uv[0],
                v: uv[1],
            });
        }
        let idx0 = out.idx.len() as u32;
        for t in sm.indices.chunks(3) {
            if t.len() < 3 {
                break;
            }
            out.idx.push((vtx0 + t[0]) as u32);
            out.idx.push((vtx0 + t[1]) as u32);
            out.idx.push((vtx0 + t[2]) as u32);
        }
        // Monster1/2/3 slots name no file: the client looks the skin up through
        // CreatureDisplayInfo.dbc. Skin-less load, so infer candidates beside the
        // model (`Creature\X\X.m2` -> XSkinBrown/White/Yellow.blp, then X.blp)
        // and keep the first the chain holds (bn_model_texture_name does the
        // same); otherwise the batch renders textureless.
        let tex = sm.texture.clone().unwrap_or_else(|| {
            if sm.skin_slot.is_none() {
                return String::new();
            }
            let dir_end = model_path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
            let stem_end = model_path.rfind('.').unwrap_or(model_path.len());
            let dir = &model_path[..dir_end];
            let stem = &model_path[dir_end..stem_end];
            ["SkinBrown", "SkinWhite", "SkinYellow", ""]
                .iter()
                .map(|s| format!("{dir}{stem}{s}.blp"))
                .find(|c| chain.is_some_and(|ch| ch.contains(c)))
                .unwrap_or_default()
        });
        out.subs.push(WmoSub {
            vtx0,
            vtx_n: sm.positions.len() as u32,
            idx0,
            idx_n: out.idx.len() as u32 - idx0,
            tex,
            two_sided: sm.two_sided,
            blend: match sm.blend {
                benilla_formats::ModelBlend::Opaque => 0,
                benilla_formats::ModelBlend::AlphaTest => 1,
                benilla_formats::ModelBlend::Blend => 2,
                benilla_formats::ModelBlend::Mod => 3,
                benilla_formats::ModelBlend::Mod2x => 4,
            } | if sm.additive { 0x100 } else { 0 },
        });
    }
    out
}

/// The baked flat for instance `i`, or `None` before `bn_wmo_instance_load`.
fn wmo_flat<'a>(world: *const BnWorld, i: u32) -> Option<&'a WmoFlat> {
    if world.is_null() {
        return None;
    }
    // SAFETY: caller owns a valid const handle.
    unsafe { &*world }.wmo_flats.get(&i)
}

/// A WMO's model-space group MLIQ surfaces, read once per root path: the root
/// for its group count, then each `{stem}_{gi:03}.wmo` for its liquid. Path
/// handling mirrors the official `load_wmo` exactly (lowercased root, stem
/// without the extension, three-digit group suffix).
fn read_wmo_group_liquids(
    chain: Option<&mut benilla_formats::Chain>,
    raw_path: &str,
) -> Vec<benilla_formats::LiquidMesh> {
    let Some(chain) = chain else {
        return Vec::new();
    };
    let root_path = raw_path.to_ascii_lowercase();
    let Ok(root_bytes) = chain.read_file(&root_path) else {
        return Vec::new();
    };
    let Ok(root) = benilla_formats::parse_wmo_root(&root_bytes) else {
        return Vec::new();
    };
    let stem = root_path.strip_suffix(".wmo").unwrap_or(&root_path);
    (0..root.group_count())
        .filter_map(|gi| {
            let bytes = chain.read_file(&format!("{stem}_{gi:03}.wmo")).ok()?;
            benilla_formats::wmo_group_liquid_mesh(&bytes)
        })
        .collect()
}

/// Bake one WMO placement's MLIQ surfaces into the tile's liquid buffer: model
/// space through the placement quaternion and position, then the same
/// tile-local WoW->UE mirror `build_wmo_flat` applies to the geometry, so the
/// host's existing liquid pass picks the canals up unchanged.
fn append_wmo_liquids(
    w: &mut BnWorld,
    model_path: &str,
    position: [f32; 3],
    rotation_deg: [f32; 3],
    center: (f32, f32),
) {
    if !w.wmo_group_liquids.contains_key(model_path) {
        let meshes = read_wmo_group_liquids(w.chain.as_mut(), model_path);
        w.wmo_group_liquids.insert(model_path.to_string(), meshes);
    }
    // Disjoint field borrows: the tile buffer and the path-keyed cache.
    let (Some(tile), Some(meshes)) = (
        w.tile.as_mut(),
        w.wmo_group_liquids.get(model_path),
    ) else {
        return;
    };
    let q = placement_quat(rotation_deg);
    for m in meshes {
        // u16 builder cap, same as the ADT liquid loop.
        if tile.lverts.len() + m.positions.len() > u16::MAX as usize {
            return;
        }
        let vtx0 = tile.lverts.len() as u32;
        for (vi, p) in m.positions.iter().enumerate() {
            let r = q_rot(q, *p);
            let (wx, wy, wz) = (
                r[0] + position[0],
                r[1] + position[1],
                r[2] + position[2],
            );
            let (x, y, z) = wow_vec_to_ue(wx - center.0, wy - center.1, wz);
            let uv = m.uvs.get(vi).copied().unwrap_or([0.0, 0.0]);
            tile.lverts.push(BnVertex {
                x,
                y,
                z,
                // Flat up as on the ADT path; the host draws liquids with a
                // constant up normal.
                nx: 0.0,
                ny: 0.0,
                nz: 1.0,
                u: uv[0],
                v: uv[1],
            });
        }
        let idx0 = tile.lidx.len() as u32;
        for &i in &m.indices {
            tile.lidx.push((vtx0 + i) as u16);
        }
        tile.liquids.push(LiquidRange {
            idx0,
            idx_n: tile.lidx.len() as u32 - idx0,
            kind: liquid_kind_code(m.kind),
        });
    }
}

/// MODF WMO instances in the loaded tile (0 when no tile).
#[no_mangle]
pub extern "C" fn bn_wmo_instance_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.wmos.len() as u32)
}

/// MODF `unique_id` of instance `i` into `out`. Returns 1 on success.
/// A straddling WMO appears in every overlapping tile's ADT with the same
/// uid; the reference dedups by it (`0x694bc0`), and so does the host.
#[no_mangle]
pub extern "C" fn bn_wmo_unique_id(world: *const BnWorld, i: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(uid) = w
        .tile
        .as_ref()
        .and_then(|t| t.wmos.get(i as usize).map(|m| m.unique_id))
    else {
        return 0;
    };
    unsafe {
        *out = uid;
    }
    1
}

/// Bake instance `i` (load its WMO model through the official `load_wmo`,
/// transform into tile-local UE axes, cache). Returns the submesh count on
/// success (UE iterates `[0, n)`), 0 on a miss or load failure.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_load(world: *mut BnWorld, i: u32) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees an exclusive valid handle.
    let w = unsafe { &mut *world };
    if let Some(f) = w.wmo_flats.get(&i) {
        return f.subs.len() as u32;
    }
    let Some((model_path, pos, rot, center)) = w.tile.as_ref().and_then(|t| {
        t.wmos
            .get(i as usize)
            .map(|inst| (inst.model.clone(), inst.position, inst.rotation, t.center))
    }) else {
        return 0;
    };
    if !w.wmo_models.contains_key(&model_path) {
        let Some(chain) = w.chain.as_mut() else {
            return 0;
        };
        let Ok(subs) = benilla_formats::load_wmo(chain, &model_path) else {
            return 0;
        };
        w.wmo_models.insert(model_path.clone(), subs);
    }
    let subs = &w.wmo_models[&model_path];
    let flat = build_wmo_flat(subs, pos, rot, center, 1.0, &model_path, w.chain.as_ref());
    let n = flat.subs.len() as u32;
    w.wmo_flats.insert(i, flat);
    // MLIQ (Stormwind canals): place the model's group liquids beside the ADT
    // MCLQ so the host's liquid pass picks them up unchanged. The cache above
    // early-returns, so each instance appends exactly once per tile rebuild
    // (`bn_load_tile` clears wmo_flats together with the old tile buffer).
    append_wmo_liquids(w, &model_path, pos, rot, center);
    n
}

/// Baked vertex count of instance `i` (0 before load).
#[no_mangle]
pub extern "C" fn bn_wmo_instance_vertex_count(world: *const BnWorld, i: u32) -> u32 {
    wmo_flat(world, i).map_or(0, |f| f.verts.len() as u32)
}

/// Baked triangle-index count of instance `i` (0 before load).
#[no_mangle]
pub extern "C" fn bn_wmo_instance_index_count(world: *const BnWorld, i: u32) -> u32 {
    wmo_flat(world, i).map_or(0, |f| f.idx.len() as u32)
}

/// Fill `out` (pre-allocated to `cap`) with instance `i`'s vertices.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_fill_vertices(
    world: *const BnWorld,
    i: u32,
    out: *mut BnVertex,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = wmo_flat(world, i) else { return 0 };
    let n = f.verts.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(f.verts.as_ptr(), out, n) };
    n as u32
}

/// Fill `out` (pre-allocated to `cap`) with instance `i`'s triangles;
/// `material` carries the submesh index, `flags` the submesh two-sided bit
/// (MOMT 0x04), matching the M2/terrain convention the UE builder reads.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_fill_triangles(
    world: *const BnWorld,
    i: u32,
    out: *mut BnTriangle,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = wmo_flat(world, i) else { return 0 };
    let n = f.idx.len().min(cap as usize);
    for (j, sm) in f.subs.iter().enumerate() {
        if (sm.idx0 as usize) >= n {
            break;
        }
        let end = (sm.idx0 as usize + sm.idx_n as usize).min(n);
        for k in (sm.idx0 as usize..end).step_by(3) {
            unsafe {
                *out.add(k) = BnTriangle {
                    a: f.idx[k],
                    b: f.idx[k + 1],
                    c: f.idx[k + 2],
                    material: j.min(255) as u8,
                    flags: if sm.two_sided { 1 } else { 0 },
                };
            }
        }
    }
    n as u32
}

/// Submesh `j` of instance `i`: writes {vtx0, vtx_n, idx0, idx_n}. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_submesh(
    world: *const BnWorld,
    i: u32,
    j: u32,
    out: *mut u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(sm) = wmo_flat(world, i).and_then(|f| f.subs.get(j as usize)) else {
        return 0;
    };
    unsafe {
        *out = sm.vtx0;
        *out.add(1) = sm.vtx_n;
        *out.add(2) = sm.idx0;
        *out.add(3) = sm.idx_n;
    }
    1
}

/// Submesh `j` of instance `i` texture path into `buf` (NUL-terminated).
#[no_mangle]
pub extern "C" fn bn_wmo_instance_texture(
    world: *const BnWorld,
    i: u32,
    j: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let Some(sm) = wmo_flat(world, i).and_then(|f| f.subs.get(j as usize)) else {
        return 0;
    };
    let bytes = sm.tex.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    1
}

/// Submesh `j` of instance `i` blend class: 0 Opaque, 1 AlphaTest, 2 Blend,
/// 3 Mod, 4 Mod2x; bit 0x100 = additive. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_blend(
    world: *const BnWorld,
    i: u32,
    j: u32,
    out: *mut u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(sm) = wmo_flat(world, i).and_then(|f| f.subs.get(j as usize)) else {
        return 0;
    };
    unsafe {
        *out = sm.blend;
    }
    1
}

// ---- doodads (MDDF terrain M2 placements): the WMO instance FFI mirrored ----

/// MDDF doodad placements in the loaded tile (0 when no tile).
#[no_mangle]
pub extern "C" fn bn_doodad_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.doodads.len() as u32)
}

/// The baked flat for doodad `i`, or `None` before `bn_doodad_load`.
fn doodad_flat<'a>(world: *const BnWorld, i: u32) -> Option<&'a WmoFlat> {
    if world.is_null() {
        return None;
    }
    // SAFETY: caller owns a valid const handle.
    unsafe { &*world }.doodad_flats.get(&i)
}

/// Bake doodad `i` (load its M2 through the official `load_m2_mesh`, apply the
/// MDDF placement with scale, cache). Returns the submesh count on success
/// (UE iterates `[0, n)`), 0 on a miss or load failure.
#[no_mangle]
pub extern "C" fn bn_doodad_load(world: *mut BnWorld, i: u32) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees an exclusive valid handle.
    let w = unsafe { &mut *world };
    if let Some(f) = w.doodad_flats.get(&i) {
        return f.subs.len() as u32;
    }
    let Some((model_path, pos, rot, scale, center)) = w.tile.as_ref().and_then(|t| {
        t.doodads.get(i as usize).map(|d| {
            (
                d.model.clone(),
                d.position,
                d.rotation,
                d.scale,
                t.center,
            )
        })
    }) else {
        return 0;
    };
    if !w.doodad_models.contains_key(&model_path) {
        let Some(chain) = w.chain.as_mut() else {
            return 0;
        };
        // `.mdx` -> `.m2` normalization happens inside load_m2_mesh.
        let Ok(subs) = benilla_formats::load_m2_mesh(chain, &model_path) else {
            return 0;
        };
        w.doodad_models.insert(model_path.clone(), subs);
    }
    let subs = &w.doodad_models[&model_path];
    let flat = build_wmo_flat(subs, pos, rot, center, scale, &model_path, w.chain.as_ref());
    let n = flat.subs.len() as u32;
    w.doodad_flats.insert(i, flat);
    n
}

/// Baked vertex count of doodad `i` (0 before load).
#[no_mangle]
pub extern "C" fn bn_doodad_vertex_count(world: *const BnWorld, i: u32) -> u32 {
    doodad_flat(world, i).map_or(0, |f| f.verts.len() as u32)
}

/// Baked triangle-index count of doodad `i` (0 before load).
#[no_mangle]
pub extern "C" fn bn_doodad_index_count(world: *const BnWorld, i: u32) -> u32 {
    doodad_flat(world, i).map_or(0, |f| f.idx.len() as u32)
}

/// Fill `out` (pre-allocated to `cap`) with doodad `i`'s vertices.
#[no_mangle]
pub extern "C" fn bn_doodad_fill_vertices(
    world: *const BnWorld,
    i: u32,
    out: *mut BnVertex,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = doodad_flat(world, i) else { return 0 };
    let n = f.verts.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(f.verts.as_ptr(), out, n) };
    n as u32
}

/// Fill `out` (pre-allocated to `cap`) with doodad `i`'s triangles;
/// `material` carries the submesh index, `flags` the M2 material two-sided bit
/// (0x04), matching the WMO/terrain convention the UE builder reads.
#[no_mangle]
pub extern "C" fn bn_doodad_fill_triangles(
    world: *const BnWorld,
    i: u32,
    out: *mut BnTriangle,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = doodad_flat(world, i) else { return 0 };
    let n = f.idx.len().min(cap as usize);
    for (j, sm) in f.subs.iter().enumerate() {
        if (sm.idx0 as usize) >= n {
            break;
        }
        let end = (sm.idx0 as usize + sm.idx_n as usize).min(n);
        for k in (sm.idx0 as usize..end).step_by(3) {
            unsafe {
                *out.add(k) = BnTriangle {
                    a: f.idx[k],
                    b: f.idx[k + 1],
                    c: f.idx[k + 2],
                    material: j.min(255) as u8,
                    flags: if sm.two_sided { 1 } else { 0 },
                };
            }
        }
    }
    n as u32
}

/// Submesh `j` of doodad `i`: writes {vtx0, vtx_n, idx0, idx_n}. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_doodad_submesh(
    world: *const BnWorld,
    i: u32,
    j: u32,
    out: *mut u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(sm) = doodad_flat(world, i).and_then(|f| f.subs.get(j as usize)) else {
        return 0;
    };
    unsafe {
        *out = sm.vtx0;
        *out.add(1) = sm.vtx_n;
        *out.add(2) = sm.idx0;
        *out.add(3) = sm.idx_n;
    }
    1
}

/// Submesh `j` of doodad `i` texture path into `buf` (NUL-terminated).
#[no_mangle]
pub extern "C" fn bn_doodad_texture(
    world: *const BnWorld,
    i: u32,
    j: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let Some(sm) = doodad_flat(world, i).and_then(|f| f.subs.get(j as usize)) else {
        return 0;
    };
    let bytes = sm.tex.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    1
}

/// Model path of doodad `i` into `buf` (NUL-terminated). 1 on success.
/// Lets the host pick animation state through the `bn_model_anim_*` family
/// without the tile's MDDF reaching across the FFI any other way.
#[no_mangle]
pub extern "C" fn bn_doodad_model_path(
    world: *const BnWorld,
    i: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(path) = w
        .tile
        .as_ref()
        .and_then(|t| t.doodads.get(i as usize).map(|d| d.model.clone()))
    else {
        return 0;
    };
    // MMDX ships `.mdx`; the archives hold `.m2` (load_m2_mesh's normalizer,
    // mirrored here) so the returned path is directly usable by the
    // bn_model_* family (anim counts, skinned positions).
    let path = {
        let lower = path.to_ascii_lowercase();
        match lower
            .strip_suffix(".mdx")
            .or_else(|| lower.strip_suffix(".mdl"))
        {
            Some(stem) => format!("{stem}.m2"),
            None => lower,
        }
    };
    let bytes = path.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    1
}

/// MDDF `unique_id` of doodad `i` into `out`. Returns 1 on success.
/// A straddling doodad appears in every overlapping tile's ADT with the same
/// uid; the reference dedups by it (`0x694bc0`), and so does the host.
#[no_mangle]
pub extern "C" fn bn_doodad_unique_id(world: *const BnWorld, i: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(uid) = w
        .tile
        .as_ref()
        .and_then(|t| t.doodads.get(i as usize).map(|d| d.unique_id))
    else {
        return 0;
    };
    unsafe {
        *out = uid;
    }
    1
}

/// The baked submesh `j` of loaded doodad `i`, or `None` before load / out of
/// range (the flat's submesh order is the model's, possibly truncated at the
/// u16 cap, so indexing the model cache mirrors the published ranges).
fn doodad_submesh(w: &BnWorld, i: u32, j: u32) -> Option<&benilla_formats::RenderSubmesh> {
    if !w.doodad_flats.contains_key(&i) {
        return None;
    }
    let path = w.tile.as_ref()?.doodads.get(i as usize)?.model.clone();
    w.doodad_models.get(&path)?.get(j as usize)
}

/// Material animation of loaded doodad `i` submesh `j` at `elapsed` seconds
/// (the instance's attach age; one clock feeds both the gseq and band
/// channels). Writes 6 floats into `out`: `uv.x, uv.y, r, g, b, alpha`
/// (identity for absent channels: `[0,0]`, `[1,1,1]`, `1`). Returns a channel
/// mask — 1 uv, 2 rgb, 4 alpha — or 0 when nothing animates (or the doodad
/// is not loaded / has no such submesh). The host builds a parameter-driven
/// MID only for masked submeshes and re-samples each frame.
#[no_mangle]
pub extern "C" fn bn_doodad_mat_anim(
    world: *const BnWorld,
    i: u32,
    j: u32,
    elapsed: f64,
    out: *mut f32,
) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(sm) = doodad_submesh(w, i, j) else {
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

    // Placed-doodad lane: no playing sequence, so slot 0; color × weight.
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

/// Blend class of loaded doodad `i` submesh `j` into `out`: 0 opaque, 1
/// alpha-test, 2 alpha-blend, 3 mod, 4 mod2x — plus bit `0x100` when the batch
/// draws additive (M2 blend 3/4 glow). The host picks the material's blend mode
/// from it (foliage cutout needs masked, not opaque). Returns 1 on success, 0
/// when the doodad is not loaded / has no such submesh.
#[no_mangle]
pub extern "C" fn bn_doodad_blend(world: *const BnWorld, i: u32, j: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(sm) = doodad_submesh(w, i, j) else {
        return 0;
    };
    let mut v = match sm.blend {
        benilla_formats::ModelBlend::Opaque => 0,
        benilla_formats::ModelBlend::AlphaTest => 1,
        benilla_formats::ModelBlend::Blend => 2,
        benilla_formats::ModelBlend::Mod => 3,
        benilla_formats::ModelBlend::Mod2x => 4,
    };
    if sm.additive {
        v |= 0x100;
    }
    unsafe {
        *out = v;
    }
    1
}

/// Texture address mode of loaded doodad `i` submesh `j` into `out`: bit0 =
/// wrap U, bit1 = wrap V (M2 texture record `flags & 0x1/0x2`; absent record
/// repeats both). The host keys its BLP cache per mode so a cutout card
/// clamps to its transparent margin instead of wrapping into the opaque
/// middle. Returns 1 on success, 0 when the doodad is not loaded / has no
/// such submesh.
#[no_mangle]
pub extern "C" fn bn_doodad_wrap(world: *const BnWorld, i: u32, j: u32, out: *mut u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };
    let Some(sm) = doodad_submesh(w, i, j) else {
        return 0;
    };
    unsafe {
        *out = (sm.wrap_x as u32) | ((sm.wrap_y as u32) << 1);
    }
    1
}

#[cfg(test)]
mod wmo_placement_tests {
    use super::*;

    /// Golden from `benilla-assets::coords` `placement_rotation_goldens`. The
    /// official test asserts `to_bevy · in_wow · to_bevy⁻¹` (rotation in baked
    /// Bevy space); our pipeline keeps `in_wow` (rotate in WoW space, mirror
    /// afterwards). Conjugating by a unit quaternion rotates the vector part
    /// by `wow_to_bevy` = (−y, z, −x) and leaves w, so apply that to compare —
    /// q and −q are the same rotation, hence |dot|.
    #[test]
    fn placement_quat_matches_official_goldens() {
        use std::f32::consts::FRAC_1_SQRT_2;
        let cases = [
            (
                [86.0_f32, 252.0, 93.5],
                [-0.691160, -0.107332, -0.156292, 0.697388],
            ),
            ([0.0, 0.0, 90.0], [FRAC_1_SQRT_2, -FRAC_1_SQRT_2, 0.0, 0.0]),
            ([30.0, 45.0, 60.0], [0.360423, -0.822363, -0.391904, 0.200562]),
        ];
        for (deg, expected) in cases {
            let q = placement_quat(deg);
            let bevy = [-q[1], q[2], -q[0], q[3]]; // B·q·B⁻¹, B = wow_to_bevy
            let dot: f32 = (0..4).map(|k| bevy[k] * expected[k]).sum();
            assert!(dot.abs() > 0.9999, "placement_quat({deg:?}) = {q:?} → bevy {bevy:?}, expected ~{expected:?} (dot {dot})");
        }
    }
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

                            a: tri[n] as u32,

                            b: tri[n + 1] as u32,

                            c: tri[n + 2] as u32,

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

            // Monster1/2/3 (11..=13) slots name no file: the WoW client looks
            // the skin up through CreatureDisplayInfo.dbc. Skin-less load, so
            // infer candidates beside the model (`Creature\X\X.m2` ->
            // XSkinBrown/White/Yellow.blp, then X.blp) and keep the first the
            // chain holds; other empty slots stay empty.
            let text: Vec<u8> = if is_empty && (11..=13).contains(&tex_type) {
                let dir_end = p.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
                let stem_end = p.rfind('.').unwrap_or(p.len());
                let dir = &p[..dir_end];
                let stem = &p[dir_end..stem_end];
                ["SkinBrown", "SkinWhite", "SkinYellow", ""]
                    .iter()
                    .map(|s| format!("{dir}{stem}{s}.blp"))
                    .find(|c| w.chain.as_ref().is_some_and(|ch| ch.contains(c)))
                    .unwrap_or_default()
                    .into_bytes()
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

// ---- UI (stage 3): stock FrameXML load + widget-tree dump ----
//
// Read-only use of the official `benilla-ui` engine: `bn_ui_load` runs every
// row of the chain's `FrameXML.toc` into a fresh in-game VM, `bn_ui_dump`
// walks the resulting frame/region tree through the engine's own Lua API and
// returns it as TSV text (phase 1: verify the data link in Rust; UE renders
// in phase 2).

use std::cell::RefCell;

thread_local! {
    /// The in-game UI session, created by `bn_ui_load`. mlua's Lua is `!Send`
    /// (no `send` feature), so the VM lives on the loading thread; the host
    /// drives load and dump from its one UI thread.
    static BN_UI: RefCell<Option<benilla_ui::script::UiScript>> = const { RefCell::new(None) };
}

/// The dump script: one `F` line per frame, one `R` line per region, tabs
/// within any field squashed. Each frame is `pcall`ed so one bad widget never
/// sinks the whole dump. WoW 1.12 runs Lua 5.0: no `#` length operator —
/// `table.getn`/`table.insert` instead.
const UI_DUMP_LUA: &str = r#"
    local function num(v)
        if v == nil then return "0" end
        return string.format("%.1f", v)
    end
    local function ratio(v)
        if type(v) ~= "number" then return "1.00" end
        return string.format("%.2f", v)
    end
    local function clean(s)
        if s == nil then return "" end
        return (string.gsub(tostring(s), "[\t\r\n]", " "))
    end
    -- The colour slot (GetTextColor / GetVertexColor): r g b a, 0..1.
    -- pcall'd because one bad region must not sink its frame's rows.
    local function colour_str(fn)
        local ok, a, b, c, d = pcall(fn)
        if not ok then return "1.00\t1.00\t1.00\t1.00" end
        return table.concat({ ratio(a), ratio(b), ratio(c), ratio(d) }, "\t")
    end
    -- The region's own alpha slot (SetAlpha), kept apart from the colour's
    -- alpha: the drawn product multiplies both with the owner frame's.
    local function region_alpha_str(r)
        local ok, ra = pcall(function() return r:GetAlpha() end)
        if not ok then return "1.00" end
        return ratio(ra)
    end
    -- Texture UV corners. GetTexCoord answers eight values in the order
    -- UL, LL, UR, LR (paint.rs maps stored Corners [TL,TR,BR,BL] to
    -- [c0,c3,c1,c2], and Rect edges to [[l,t],[l,b],[r,t],[r,b]]), so:
    --   TL = out0, BL = out1, TR = out2, BR = out3.
    -- The dump emits all four corners as TL,TR,BR,BL (cols 19..26) because a
    -- backdrop's top/bottom edge strips transpose the atlas axes (atlas u runs
    -- down the screen, v right-to-left, backdrop.rs run_uvs) - an axis-aligned
    -- (u1,v1)-(u2,v2) quad cannot express that; the renderer needs per-corner
    -- UVs to draw those with two triangles. Non-texture regions and absent
    -- methods fall back to the full quad.
    local FULL = "0.0000\t0.0000\t1.0000\t0.0000\t1.0000\t1.0000\t0.0000\t1.0000"
    local function texcoord_str(r)
        if not r.GetTexCoord then return FULL end
        local ok, ax, ay, bx, by, cx, cy, dx, dy =
            pcall(function() return r:GetTexCoord() end)
        if not ok or type(ax) ~= "number" or type(dx) ~= "number" then
            return FULL
        end
        -- %.4f, not num()'s %.1f: 9-slice insets like 6/256 = 0.023 round
        -- to 0.0 at one decimal and shift the border by half the texture.
        return table.concat({
            string.format("%.4f", ax), string.format("%.4f", ay),  -- TL
            string.format("%.4f", cx), string.format("%.4f", cy),  -- TR
            string.format("%.4f", dx), string.format("%.4f", dy),  -- BR
            string.format("%.4f", bx), string.format("%.4f", by)   -- BL
        }, "\t")
    end
    -- Draw layer (col 27), the ZKey field above batch rank: the client emits
    -- every frame's BACKGROUND quads before any BORDER/ARTWORK, and within a
    -- layer every quad before every font string (order.rs). The renderer must
    -- sort on this or a button's own texture paints over its label.
    local function layer_str(r)
        local ok, dl = pcall(function() return r:GetDrawLayer() end)
        if not ok or type(dl) ~= "string" then return "ARTWORK" end
        return dl
    end
    local function dump_one(f)
        local mine = {}
        -- IsVisible, not IsShown: the shown bit ignores hidden ancestors, so
        -- gating on it would emit every child of a hidden screen (RealmList,
        -- AddonList, ... at the login screen: 299 shown vs 12 visible) and
        -- the renderer would draw all of them stacked.
        table.insert(mine, table.concat({ "F", clean(f:GetName()),
            f:IsVisible() and "1" or "0", clean(f:GetFrameStrata()),
            tostring(f:GetFrameLevel() or 0), num(f:GetLeft()), num(f:GetTop()),
            num(f:GetWidth()), num(f:GetHeight()),
            ratio(f:GetAlpha()) }, "\t"))
        for _, r in ipairs({ f:GetRegions() }) do
            -- A hidden region draws nothing: button state textures and
            -- highlights are Show/Hide-swapped, so emitting them stacks the
            -- pressed/disabled look over the resting one. Absent method (a
            -- non-region object) keeps the row.
            if r.IsShown and not r:IsShown() then
                -- skip
            else
            local ty = clean(r:GetObjectType())
            local extra = ""
            local colr = ""
            if ty == "Texture" then
                extra = clean(r:GetTexture())
                colr = colour_str(function() return r:GetVertexColor() end)
            elseif ty == "FontString" then
                extra = clean(r:GetText())
                colr = colour_str(function() return r:GetTextColor() end)
            end
            -- Font/justification slots, appended for every region row so the
            -- TSV stays rectangular (textures carry the defaults).
            local font, size, flags, jh, jv = "", "0.0", "", "LEFT", "CENTER"
            if ty == "FontString" then
                local okf, fp, fs, ff = pcall(function() return r:GetFont() end)
                if okf then
                    font = clean(fp)
                    size = num(fs)
                    flags = clean(ff)
                end
                local okh, vh = pcall(function() return r:GetJustifyH() end)
                if okh then jh = clean(vh) end
                local okv, vv = pcall(function() return r:GetJustifyV() end)
                if okv then jv = clean(vv) end
            end
            table.insert(mine, table.concat({ "R", clean(f:GetName()),
                clean(r:GetName()), ty, num(r:GetLeft()), num(r:GetTop()),
                num(r:GetWidth()), num(r:GetHeight()), extra,
                colr, region_alpha_str(r), font, size, flags, jh, jv,
                texcoord_str(r), layer_str(r) }, "\t"))
            end
        end
        return mine
    end
    local out = {}
    local f = EnumerateFrames()
    while f do
        -- Invisible frames emit nothing at all, not even the F row. Their
        -- regions can never draw (the renderer gates on IsVisible), and the
        -- per-row string building is the whole cost of the dump: walking all
        -- frames is 4ms, dumping every row was 567ms at 3749 frames / 12482
        -- regions (12ms skipping invisible frames; only ~58 frames are ever
        -- visible). Creation order still holds for the rows that remain -
        -- SortFrame only compares visible frames against each other - so the
        -- paint order is unchanged.
        if f:IsVisible() then
            local ok, lines = pcall(dump_one, f)
            if ok and type(lines) == "table" then
                for i = 1, table.getn(lines) do
                    table.insert(out, lines[i])
                end
            end
        end
        f = EnumerateFrames(f)
    end
    return table.concat(out, "\n")
"#;

/// Load the stock interface into a fresh in-game VM: every row of the chain's
/// `Interface\FrameXML\FrameXML.toc`, in order (`.lua` runs as a chunk, `.xml`
/// through the official loader). File and loader failures are tolerated and
/// skipped. Returns the live frame count at load end, or 0 on failure.
#[no_mangle]
pub extern "C" fn bn_ui_load(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees a valid exclusive handle for the call's span.
    let w = unsafe { &*world };
    let Some(chain) = w.chain.as_ref() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(script) = benilla_ui::script::UiScript::new() else {
            return 0u32;
        };
        let toc_bytes = chain
            .read(r"Interface\FrameXML\FrameXML.toc")
            .unwrap_or_default();
        let toc = benilla_ui::toc::Toc::parse(&benilla_ui::source::decode(&toc_bytes));
        // The loader resolves `<Script file=>`/`<Include file=>` against the
        // document's dir (`dir_of` splits on `/` only), so hand `load_in` the
        // entry with forward slashes; MPQ reads take backslashes.
        let read = |req: &str| -> Option<Vec<u8>> {
            chain.read(&req.replace('/', "\\")).ok()
        };
        let mut rows = 0u32;
        let mut failures: Vec<String> = Vec::new();
        for row in &toc.files {
            let path = format!(r"Interface\FrameXML\{row}");
            let Some(bytes) = read(&path) else {
                failures.push(format!("{path}: not found"));
                continue;
            };
            rows += 1;
            if row.to_ascii_lowercase().ends_with(".lua") {
                let name = format!("@{}", path.replace('/', "\\"));
                if let Err(e) = script.run_chunk_named(&bytes, &name) {
                    failures.push(format!("{path}: {e}"));
                }
                continue;
            }
            let Ok(doc) = benilla_ui::framexml::parse(&benilla_ui::source::decode(&bytes)) else {
                failures.push(format!("{path}: parse error"));
                continue;
            };
            let entry = path.replace('\\', "/");
            let report = benilla_ui::loader::load_in(&script, &doc, &entry, &read);
            failures.extend(
                report
                    .errors
                    .iter()
                    .map(|e| format!("{path}: {e}"))
                    .chain(
                        report
                            .missing_files
                            .iter()
                            .map(|m| format!("{path}: missing {m}")),
                    ),
            );
        }
        let frames = script.eval::<f64>("return GetNumFrames()").unwrap_or(0.0) as u32;
        BN_UI.with(|slot| *slot.borrow_mut() = Some(script));
        eprintln!(
            "bn_ui_load: {} toc rows, {} failures, {frames} frames",
            rows,
            failures.len()
        );
        for f in failures.iter().take(12) {
            eprintln!("  fail {f}");
        }
        frames
    }));
    out.unwrap_or(0)
}

/// Dump the loaded UI tree as TSV text (see `UI_DUMP_LUA` for the format).
/// Writes at most `cap - 1` bytes plus a NUL; returns bytes written (0 = no
/// session or failure). Only `IsVisible()` frames come out (invisible ones
/// can never paint), in creation order, each frame's regions directly beneath
/// it.
#[no_mangle]
pub extern "C" fn bn_ui_dump(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let text = {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            BN_UI.with(|slot| match slot.borrow().as_ref() {
                Some(s) => match s.eval::<String>(UI_DUMP_LUA) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!("bn_ui_dump: eval failed: {e}");
                        None
                    }
                },
                None => {
                    eprintln!("bn_ui_dump: no UI session (call bn_ui_load first)");
                    None
                }
            })
        }));
        match out {
            Ok(Some(t)) => t,
            _ => return 0,
        }
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// Rasterize one FontString to RGBA8 for the UE HUD painter (see `ui_font`).
///
/// `size_px` is the drawn pixel size (the host applies WoW's em cap and the
/// seam scale); `outline` is 0 none / 1 thin / 2 thick. Metrics
/// (`out_w/h` = bitmap size, `out_ox/oy` = pen-box origin inside the bitmap,
/// `out_pw/ph` = the WoW-measured string extent) are written whenever the
/// raster succeeds — including when `cap` is too small, so the host can grow
/// its buffer and retry. Returns the byte count written, 0 = nothing to draw
/// (empty text, face missing, or `cap` too small).
#[no_mangle]
pub extern "C" fn bn_ui_text_bitmap(
    world: *const BnWorld,
    font_path: *const c_char,
    size_px: f32,
    outline: u32,
    text: *const c_char,
    out_rgba: *mut u8,
    cap: u32,
    out_w: *mut u32,
    out_h: *mut u32,
    out_ox: *mut f32,
    out_oy: *mut f32,
    out_pw: *mut f32,
    out_ph: *mut f32,
) -> u32 {
    // SAFETY: FFI contract — pointers are valid (or null) for the call's span.
    let Some(text_str) = cstr_to_str(text) else {
        return 0;
    };
    let font_str = cstr_to_str(font_path);
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if world.is_null() {
            return None;
        }
        // SAFETY: caller guarantees a valid exclusive handle for the call's span.
        let w = unsafe { &*world };
        let Some(chain) = w.chain.as_ref() else {
            return None;
        };
        ui_font::rasterize(
            chain,
            font_str.unwrap_or(ui_font::DEFAULT_FONT),
            size_px,
            outline,
            text_str,
        )
    }));
    let Ok(Some(bm)) = out else {
        return 0;
    };
    unsafe {
        if !out_w.is_null() {
            *out_w = bm.w;
        }
        if !out_h.is_null() {
            *out_h = bm.h;
        }
        if !out_ox.is_null() {
            *out_ox = bm.ox;
        }
        if !out_oy.is_null() {
            *out_oy = bm.oy;
        }
        if !out_pw.is_null() {
            *out_pw = bm.pw;
        }
        if !out_ph.is_null() {
            *out_ph = bm.ph;
        }
    }
    let need = bm.rgba.len() as u32;
    if out_rgba.is_null() || cap < need {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bm.rgba.as_ptr(), out_rgba, need as usize);
    }
    need
}

// ---- UI glue (stage 7): the main menu (GlueXML) + pointer feedback ----
//
// The client's menus live in `Interface\GlueXML`, on a VM the reference never
// shares with the in-game FrameXML state (so loading one replaces the other in
// `BN_UI`). `bn_ui_load_glue` builds that state from the stock toc with a stub
// `GlueHost` behind `CGlueMgr`: benilla-ffi stays offline (no networking), so
// login and the character roster answer in place, and world entry is queued for
// the render host to collect through `bn_ui_poll_event`. `bn_ui_tick` drives
// OnUpdate and applies the ops the stub queued (a call fan-out borrows the
// script, so the roster cannot load inside the call itself). Geometry arrives
// through the same `bn_ui_dump` as the in-game UI.

use benilla_ui::script::{
    GlueCall, GlueCharacter, GlueHost, GlueNotice, GlueSignal, GlueText, NoticeState, ScriptValue,
    UiScript,
};

/// Side-channel ops the stub host queues while a verb's call fan-out holds the
/// script: the fake roster (loaded by the next `bn_ui_tick`) and the events the
/// render host polls.
#[derive(Default)]
struct MenuPending {
    roster: Option<Vec<GlueCharacter>>,
    events: Vec<String>,
}

thread_local! {
    static BN_MENU: RefCell<MenuPending> =
        const { RefCell::new(MenuPending { roster: None, events: Vec::new() }) };
}

/// The menu's `CGlueMgr` host, offline: every screen transition the reference
/// would reach through the login server happens in place, and the one step that
/// needs a world server (entering the world) becomes a queued event instead.
struct MenuHost;

impl MenuHost {
    fn queue_roster() {
        BN_MENU.with(|p| {
            p.borrow_mut().roster = Some(vec![GlueCharacter {
                guid: 1,
                name: "Nezha".to_string(),
                race: "人类".to_string(),
                class: "战士".to_string(),
                level: 1,
                zone: None,
                file_string: String::new(),
                gender: 0,
                flags: 0,
                // No M2 build: the render host draws no glue scene pane anyway.
                model: false,
            }]);
        });
    }
}

impl GlueHost for MenuHost {
    fn call(&self, call: GlueCall) -> Vec<GlueSignal> {
        match call {
            // `CGlueMgr::Login`'s own dialog half; auth "succeeds" at once:
            // drop the status dialog, tear any roster down and put character
            // select up, whose OnShow asks for the character list.
            GlueCall::Login(c) => {
                if c.account.is_empty() {
                    return vec![GlueSignal::Open {
                        which: "OKAY",
                        text: Some(GlueText::Key("LOGIN_ENTER_NAME")),
                        data: None,
                    }];
                }
                if c.password.is_empty() {
                    return vec![GlueSignal::Open {
                        which: "OKAY",
                        text: Some(GlueText::Key("LOGIN_ENTER_PASSWORD")),
                        data: None,
                    }];
                }
                vec![
                    GlueSignal::Close,
                    GlueSignal::Teardown,
                    GlueSignal::Screen("charselect"),
                ]
            }
            // The reference's reply lands a frame later and loads the roster
            // with the script free; queue it for `bn_ui_tick`.
            GlueCall::GetCharacterList => {
                Self::queue_roster();
                vec![]
            }
            // The one flow a world server owns: report it to the render host.
            GlueCall::EnterWorld { .. } => {
                BN_MENU.with(|p| p.borrow_mut().events.push("enter_world".to_string()));
                vec![]
            }
            GlueCall::StatusDialogClick => vec![GlueSignal::Close],
            // No servers to list or dial in the offline menu.
            GlueCall::RequestRealmList { .. }
            | GlueCall::CancelRealmListQuery
            | GlueCall::CancelLogon
            | GlueCall::ChangeRealm(_)
            | GlueCall::DisconnectFromServer
            | GlueCall::QuitGame
            | GlueCall::DeleteCharacter(_)
            | GlueCall::RenameCharacter { .. }
            | GlueCall::CreateCharacter(_) => vec![],
        }
    }

    fn resume(&self, _step: u32) -> Vec<GlueSignal> {
        Vec::new()
    }

    /// The offline menu is always "connected": the EnterWorld guards are the
    /// host's half and must not open a disconnect dialog.
    fn connected(&self) -> bool {
        true
    }

    fn selected_realm_type(&self) -> Option<u32> {
        Some(0)
    }
}

/// Load the stock menus (`Interface\GlueXML\GlueXML.toc`) into a fresh glue VM
/// (`UiScript::new_glue`), seat the offline `MenuHost`, and put the login
/// screen up (`SET_GLUE_SCREEN "login"`), mirroring the reference's glue build.
/// Returns the live frame count, or 0 on failure. Loading glue replaces any
/// in-game UI session in `BN_UI` and vice versa (the reference keeps the two
/// states apart).
#[no_mangle]
pub extern "C" fn bn_ui_load_glue(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees a valid exclusive handle for the call's span.
    let w = unsafe { &*world };
    let Some(chain) = w.chain.as_ref() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut script) = UiScript::new_glue() else {
            return 0u32;
        };
        BN_MENU.with(|p| *p.borrow_mut() = MenuPending::default());
        // The agreements the login's first tests read; a returning player's
        // flags, so the menu is not parked on the ToU screen.
        for notice in GlueNotice::ALL {
            script.set_glue_notice(
                notice,
                NoticeState {
                    accepted: true,
                    show_notice: false,
                },
            );
        }
        let toc_bytes = chain
            .read(r"Interface\GlueXML\GlueXML.toc")
            .unwrap_or_default();
        let toc = benilla_ui::toc::Toc::parse(&benilla_ui::source::decode(&toc_bytes));
        // Same loader contract as `bn_ui_load`: forward slashes in `load_in`
        // entries (dir_of splits on `/` only), backslashes for the MPQ reads.
        let read = |req: &str| -> Option<Vec<u8>> {
            chain.read(&req.replace('/', "\\")).ok()
        };
        let mut rows = 0u32;
        let mut failures: Vec<String> = Vec::new();
        for row in &toc.files {
            let path = format!(r"Interface\GlueXML\{row}");
            let Some(bytes) = read(&path) else {
                failures.push(format!("{path}: not found"));
                continue;
            };
            rows += 1;
            if row.to_ascii_lowercase().ends_with(".lua") {
                let name = format!("@{}", path.replace('/', "\\"));
                if let Err(e) = script.run_chunk_named(&bytes, &name) {
                    failures.push(format!("{path}: {e}"));
                }
                continue;
            }
            let Ok(doc) = benilla_ui::framexml::parse(&benilla_ui::source::decode(&bytes)) else {
                failures.push(format!("{path}: parse error"));
                continue;
            };
            let entry = path.replace('\\', "/");
            let report = benilla_ui::loader::load_in(&script, &doc, &entry, &read);
            failures.extend(
                report
                    .errors
                    .iter()
                    .map(|e| format!("{path}: {e}"))
                    .chain(
                        report
                            .missing_files
                            .iter()
                            .map(|m| format!("{path}: missing {m}")),
                    ),
            );
        }
        // `GetBillingPlan` is one of benilla's NOT_YET glue verbs (upstream's own
        // list), but zhCN's `CharacterSelect_OnShow` calls it and aborts on the
        // nil, skipping its tail (`GlueFrameFadeIn`). Stand it in only while
        // benilla leaves it unset, so a real binding is never shadowed.
        let _ = script.run(
            "if not GetBillingPlan then GetBillingPlan = function() return 0, 0, 0 end end",
        );
        script.fire_event("FRAMES_LOADED", Vec::new());
        // Seat the host after the load (as the reference does) and run the
        // calls the load made while it waited.
        let host = std::rc::Rc::new(MenuHost);
        script.set_glue_host(host.clone());
        for call in script.take_glue_calls() {
            for signal in host.call(call) {
                script.fire_glue_signal(signal);
            }
        }
        script.set_glue_screen_name("login");
        script.fire_event(
            "SET_GLUE_SCREEN",
            vec![ScriptValue::Str("login".to_string())],
        );
        let frames = script.eval::<f64>("return GetNumFrames()").unwrap_or(0.0) as u32;
        BN_UI.with(|slot| *slot.borrow_mut() = Some(script));
        eprintln!(
            "bn_ui_load_glue: {} toc rows, {} failures, {frames} frames",
            rows,
            failures.len()
        );
        for f in failures.iter().take(12) {
            eprintln!("  fail {f}");
        }
        frames
    }));
    out.unwrap_or(0)
}

/// Set the screen root to a pixel size (origin top-left, y up inside the VM).
/// The render host calls this with its viewport size after a load so dump
/// coordinates and pointer coordinates are both viewport pixels; returns 1
/// when the root changed (a layout reflow is due), 0 without a session.
#[no_mangle]
pub extern "C" fn bn_ui_set_screen_size(w: f32, h: f32) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => u32::from(script.set_screen_size(w, h)),
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

/// Advance the loaded UI (OnUpdate handlers, hover re-pick, queued events) and
/// apply the menu host's pending ops (the fake roster load). `elapsed` is
/// seconds since the last call. Returns 1 with a session, 0 without.
#[no_mangle]
pub extern "C" fn bn_ui_tick(elapsed: f32) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            let roster = BN_MENU.with(|p| p.borrow_mut().roster.take());
            if let Some(roster) = roster {
                script.load_glue_roster(roster);
            }
            script.tick(elapsed);
            for e in script.take_errors() {
                eprintln!("bn_ui_tick: {e}");
            }
            1u32
        })
    }));
    out.unwrap_or(0)
}

/// Move the UI pointer to `(x, y)` in the root's screen space (`bn_ui_set_
/// screen_size`, y up). Fires hover enter/leave like the reference's cursor
/// walk. Returns 1 with a session, 0 without.
#[no_mangle]
pub extern "C" fn bn_ui_mouse_move(x: f32, y: f32) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => {
                    script.mouse_move(x, y);
                    1u32
                }
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

/// Press or release a mouse button at `(x, y)`; `button` is the WoW name
/// ("LeftButton", ...), `down` 1 = press, 0 = release. Returns whether the UI
/// consumed the event (0 without a session).
#[no_mangle]
pub extern "C" fn bn_ui_mouse_button(x: f32, y: f32, button: *const c_char, down: u32) -> u32 {
    if button.is_null() {
        return 0;
    }
    let Ok(name) = (unsafe { std::ffi::CStr::from_ptr(button) }).to_str() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => u32::from(script.mouse_button(x, y, name, down != 0)),
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

// ---- keyboard: key names in, one fixed Windows chord table applied here ----
// (the reference leaves the per-OS keymap to the host; keeping it in the
// middle layer leaves the UE side a pure event forwarder).

/// The editing action for a taken key-down on an EditBox, from the key name
/// and chord bits (bit0 Shift, bit1 Ctrl, bit2 Alt); `None` for keys with no
/// editing action (letters, ENTER/ESCAPE/TAB — those the box handles itself).
fn editbox_chord(key: &str, mods: u32) -> Option<benilla_ui::widget::EditAction> {
    use benilla_ui::widget::{EditAction, EditUnit};
    let shift = mods & 1 != 0;
    let ctrl = mods & 2 != 0;
    let k = key.to_ascii_uppercase();
    let mv = |unit: EditUnit, back: bool| {
        Some(EditAction::Move { unit, back, extend: shift })
    };
    match (k.as_str(), ctrl) {
        ("LEFT", false) => mv(EditUnit::Char, true),
        ("LEFT", true) => mv(EditUnit::Word, true),
        ("RIGHT", false) => mv(EditUnit::Char, false),
        ("RIGHT", true) => mv(EditUnit::Word, false),
        ("UP", _) => mv(EditUnit::Row, true),
        ("DOWN", _) => mv(EditUnit::Row, false),
        ("HOME", false) => mv(EditUnit::Line, true),
        ("HOME", true) => mv(EditUnit::Edge, true),
        ("END", false) => mv(EditUnit::Line, false),
        ("END", true) => mv(EditUnit::Edge, false),
        ("BACKSPACE", false) => Some(EditAction::Delete { unit: EditUnit::Char, back: true }),
        ("BACKSPACE", true) => Some(EditAction::Delete { unit: EditUnit::Word, back: true }),
        ("DELETE", false) => Some(EditAction::Delete { unit: EditUnit::Char, back: false }),
        ("DELETE", true) => Some(EditAction::Delete { unit: EditUnit::Word, back: false }),
        ("A", true) => Some(EditAction::SelectAll),
        ("U", true) => Some(EditAction::DeleteToEdge { back: true }),
        ("K", true) => Some(EditAction::DeleteToEdge { back: false }),
        ("P", true) => Some(EditAction::History { older: true }),
        ("N", true) => Some(EditAction::History { older: false }),
        _ => None,
    }
}

/// Press or release a key by WoW name ("A", "ENTER", "LEFT", ...). `mods` is
/// the chord state (bit0 Shift, bit1 Ctrl, bit2 Alt; for a modifier's own
/// event include its own bit — the state after the event), `down` 1 = press.
/// A press runs the key-down walk and, when it lands on an EditBox, our keymap
/// applies the key's editing action (the reference's host job). Returns the
/// KeyWalk of a press (0 declined / 1 frame / 2 box), 1/0 taken for a release,
/// 0 with no session.
#[no_mangle]
pub extern "C" fn bn_ui_key(key: *const c_char, mods: u32, down: u32) -> u32 {
    if key.is_null() {
        return 0;
    }
    let Ok(name) = (unsafe { std::ffi::CStr::from_ptr(key) }).to_str() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else { return 0u32 };
            // The alt-arrow gate reads this: arrows pass through an
            // ignoreArrows box only while ALT is held.
            script.set_alt_down(mods & 4 != 0);
            if down != 0 {
                let walk = script.key_down(name);
                if walk == benilla_ui::script::KeyWalk::Box {
                    if let Some(action) = editbox_chord(name, mods) {
                        script.editbox_action(action);
                    }
                }
                match walk {
                    benilla_ui::script::KeyWalk::Declined => 0u32,
                    benilla_ui::script::KeyWalk::Frame => 1,
                    benilla_ui::script::KeyWalk::Box => 2,
                }
            } else {
                u32::from(script.key_up(name).taken())
            }
        })
    }));
    out.unwrap_or(0)
}

/// A typed character (UTF-8: "a", "5", "中") into the focused EditBox, or the
/// char walk with no box. Returns 1 if consumed, 0 otherwise (or no session).
#[no_mangle]
pub extern "C" fn bn_ui_char(text: *const c_char) -> u32 {
    if text.is_null() {
        return 0;
    }
    let Ok(s) = (unsafe { std::ffi::CStr::from_ptr(text) }).to_str() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => u32::from(script.char_input(s)),
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

/// Paste host clipboard text into the focused EditBox, typed a character at a
/// time as the reference's paste is. Returns 1 if consumed, 0 otherwise.
#[no_mangle]
pub extern "C" fn bn_ui_paste(text: *const c_char) -> u32 {
    if text.is_null() {
        return 0;
    }
    let Ok(s) = (unsafe { std::ffi::CStr::from_ptr(text) }).to_str() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => u32::from(script.paste(s)),
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

/// Pop one event the menu host queued ("enter_world", ...) into `buf` as NUL-
/// terminated UTF-8; returns bytes written (0 = none pending). The render host
/// polls this to learn when the UI wants the game world up.
#[no_mangle]
pub extern "C" fn bn_ui_poll_event(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let event = BN_MENU.with(|p| {
        let mut p = p.borrow_mut();
        if p.events.is_empty() {
            None
        } else {
            Some(p.events.remove(0))
        }
    });
    let Some(text) = event else {
        return 0;
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// Run `code` (a Lua chunk, `return` for a value) in the loaded VM and write
/// its result to `buf` as text; returns bytes written (0 = no session or no
/// result). For menu actions the host drives directly and for probes.
#[no_mangle]
pub extern "C" fn bn_ui_eval(code: *const c_char, buf: *mut c_char, cap: u32) -> u32 {
    if code.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    let Ok(code) = (unsafe { std::ffi::CStr::from_ptr(code) }).to_str() else {
        return 0;
    };
    let text = {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            BN_UI.with(|slot| {
                let mut slot = slot.borrow_mut();
                let Some(script) = slot.as_mut() else {
                    return None;
                };
                let result = {
                    use benilla_ui::mlua::Value;
                    match script.eval::<Value>(code) {
                        Ok(v) => Some(match v {
                            Value::Nil => String::new(),
                            Value::String(s) => s.to_string_lossy().to_string(),
                            Value::Number(n) => format!("{n}"),
                            Value::Boolean(b) if b => "true".to_string(),
                            Value::Boolean(_) => "false".to_string(),
                            other => other.type_name().to_string(),
                        }),
                        Err(e) => {
                            eprintln!("bn_ui_eval: {e}");
                            None
                        }
                    }
                };
                // pcall'd handlers the eval's events ran (screen switches etc.)
                for e in script.take_errors() {
                    eprintln!("bn_ui_eval handler: {e}");
                }
                result
            })
        }));
        match out {
            Ok(Some(t)) => t,
            _ => return 0,
        }
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

// ---- glue booth ----

/// The login / character-screen backdrop booth (benilla-app's glue_booth):
/// the shown `<Model>` frame's inputs for the UE render host. `cam_*` is
/// **raw WoW model space** - the UE side maps it through (x, -y, z) × 100
/// exactly like `bn_model_fill` maps the vertices, so camera and mesh land in
/// one space. `path` is `.m2`-normalized, ready for `bn_model_fill`.
#[repr(C)]
pub struct BnBooth {
    /// NUL-terminated model path (GlueXML's `.mdx`, normalized to `.m2`).
    pub path: [c_char; 260],
    /// The Model frame rect: left, top, width, height (VM units, Y up).
    pub rect: [f32; 4],
    /// `GetFacing()` yaw (radians), `GetModelScale()`, `GetPosition()`.
    pub facing: f32,
    pub scale: f32,
    pub pos: [f32; 3],
    /// camera0, the glue pane's `Model:SetCamera(0)` record.
    pub cam_eye: [f32; 3],
    pub cam_target: [f32; 3],
    pub cam_roll: f32,
    /// Diagonal radians; 0 = the model has no camera record.
    pub cam_fov: f32,
    pub cam_near: f32,
    pub cam_far: f32,
    /// The scene's stock fog, mirroring glue_booth.rs `stock_fog`
    /// (AccountLogin.xml `<FogColor>`/`fogFar`, GlueParent.lua `CharModelFogInfo`).
    pub fog_rgb: [f32; 3],
    pub fog_near: f32,
    pub fog_far: f32,
}

#[no_mangle]
pub extern "C" fn bn_ui_booth(world: *const BnWorld, out: *mut BnBooth) -> u32 {
    if out.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees a valid out pointer.
    unsafe { std::ptr::write_bytes(out, 0, 1) };
    if world.is_null() {
        return 0;
    }
    // SAFETY: valid handle.
    let w = unsafe { &*world };

    // First shown Model frame with a model set (the login screen's booth is
    // AccountLogin; later screens swap the token, e.g. UI_Orc). The VM's
    // script dialect has no `#` and EnumerateFrames takes the frame itself.
    const LUA: &str = r#"
        local f = EnumerateFrames()
        while f do
            local okm, m = pcall(function()
                if f.GetModel then return f:GetModel() end
            end)
            if okm and type(m) == "string" and m ~= "" then
                local oks, sh = pcall(function() return f:IsShown() end)
                if oks and sh then
                    local okb, l, t, w, h, fa, sc, px, py, pz = pcall(function()
                        return f:GetLeft(), f:GetTop(), f:GetWidth(), f:GetHeight(),
                               f:GetFacing(), f:GetModelScale(), f:GetPosition()
                    end)
                    if not okb then
                        l, t, w, h, fa, sc, px, py, pz = 0, 0, 0, 0, 0, 1, 0, 0, 0
                    end
                    return string.format(
                        "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s",
                        tostring(f:GetName()), m, tostring(l), tostring(t),
                        tostring(w), tostring(h), tostring(fa), tostring(sc),
                        tostring(px), tostring(py), tostring(pz))
                end
            end
            f = EnumerateFrames(f)
        end
        return ""
    "#;
    let Ok(code) = std::ffi::CString::new(LUA) else {
        return 0;
    };
    let mut buf = vec![0u8; 2048];
    let n = bn_ui_eval(code.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    if n == 0 {
        return 0;
    }
    let text = String::from_utf8_lossy(&buf[..n as usize]).to_string();
    let f: Vec<&str> = text.split('\t').collect();
    if f.len() < 11 {
        return 0;
    }
    let num = |i: usize, d: f32| f.get(i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(d);

    // GlueXML names a .mdx stem, the archives hold .m2 only (booth_probe).
    let mut path = f[1].trim().to_string();
    if path.get(path.len().saturating_sub(4)..).is_some_and(|e| e.eq_ignore_ascii_case(".mdx")) {
        path.truncate(path.len() - 4);
        path.push_str(".m2");
    }

    // The stock fog by the scene token after `UI_` (glue_booth stock_fog);
    // an unknown stem falls on MainMenu's own row (the login backdrop).
    let stem = path.rsplit(['\\', '/']).next().unwrap_or("");
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    let token = if stem.len() > 3 && stem[..3].eq_ignore_ascii_case("UI_") {
        stem[3..].to_ascii_lowercase()
    } else {
        String::new()
    };
    let (fog_rgb, fog_far): ([f32; 3], f32) = match token.as_str() {
        "orc" => ([0.5, 0.5, 0.5], 270.0),
        "dwarf" => ([0.85, 0.88, 1.0], 500.0),
        "nightelf" => ([0.25, 0.22, 0.55], 611.0),
        "scourge" => ([0.0, 0.22, 0.22], 26.0),
        "tauren" => ([1.0, 0.61, 0.42], 153.0),
        "human" => ([0.8, 0.65, 0.73], 222.0),
        _ => ([0.25, 0.06, 0.015], 1200.0), // MainMenu
    };

    // camera0 by model path: the booth M2 is ~866KB decompressed, far too
    // heavy to re-read from the MPQ on every poll (files are immutable).
    thread_local! {
        static CAM_CACHE: std::cell::RefCell<HashMap<String, Option<[f32; 10]>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let cam = CAM_CACHE.with(|cell| {
        let mut cell = cell.borrow_mut();
        if !cell.contains_key(&path) {
            let v = m2_bytes(w, &path)
                .and_then(|b| benilla_formats::parse_m2_camera(&b, 0))
                .map(|c| {
                    [
                        c.position[0], c.position[1], c.position[2],
                        c.target[0], c.target[1], c.target[2],
                        c.roll, c.fov, c.near_clip, c.far_clip,
                    ]
                });
            cell.insert(path.clone(), v);
        }
        cell.get(&path).copied().flatten()
    });

    // SAFETY: out points at a caller-owned BnBooth.
    unsafe {
        let o = &mut *out;
        for (i, b) in path.bytes().enumerate().take(o.path.len() - 1) {
            o.path[i] = b as c_char;
        }
        o.rect = [num(2, 0.0), num(3, 0.0), num(4, 0.0), num(5, 0.0)];
        o.facing = num(6, 0.0);
        o.scale = num(7, 1.0);
        o.pos = [num(8, 0.0), num(9, 0.0), num(10, 0.0)];
        if let Some(c) = cam {
            o.cam_eye = [c[0], c[1], c[2]];
            o.cam_target = [c[3], c[4], c[5]];
            o.cam_roll = c[6];
            o.cam_fov = c[7];
            o.cam_near = c[8];
            o.cam_far = c[9];
        }
        o.fog_rgb = fog_rgb;
        o.fog_near = 0.0;
        o.fog_far = fog_far;
    }
    1
}


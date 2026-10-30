// Benilla FFI — C ABI export layer for the UE5 render host.

//

// Keeps all WoW 1.12.1 format parsing in Rust (MPQ/ADT/M2/WMO/BLP, no Bevy,

// no app, no networking); exposes the parsed geometry and decoded textures

// through plain C structs so the UE `BnRenderer` module can rebuild meshes

// and upload textures without depending on any Rust type.

//

// Built as a cdylib (`benilla.dll`); the UE side declares the same

// functions `extern "C"` and links against the.dll.

use std::collections::HashMap;

use std::ffi::c_char;

use benilla_ui::script::{GossipMenu, GossipOptionView, GossipQuestRow, QuestPanel, QuestState};

// ---- WoW <-> UE5 coordinate-system converter (single source of truth) ----

mod coords;

// ---- WoW FontString rasterization (client TTFs -> RGBA for the UE HUD) ----

mod ui_font;

// ---- Doodad material animation addressed by model path (no tile switch) ----

mod model_anim;

// ---- Placed-doodad skeletal rigs (bone animation of the MDDF props) ----

mod doodad_rig;

// ---- Character body-skin atlas composites (CharSections -> RGBA8) ----

mod char_skin;

// ---- Character textures, geosets and animation-id selection (middle layer) ----

mod char_body;

// ---- M2 particle emitters (glue backdrop flames / motes) ----

pub mod model_particles;

// ---- Player locomotion, collision probe and third-person camera ----
//
// The player state, mover and camera of the full client live in crates this
// one cannot link; this module carries the same laws over the
// ADT/WMO data this handle already loads.

mod player;

// ---- Distant low-detail terrain (WDL far band) ----

mod wdl_far;

// ---- Network session (realm logon + world stream on a worker thread) ----

mod net;

// ---- CreatureDisplayInfo -> model path / scale / skins (host render inputs) ----

mod creature;

// ---- GameObjectDisplayInfo -> model path (a lootable object's look) ----

mod gameobject;

// ---- A unit's virtual weapon slots -> attach point, item model and object skin ----

mod held_item;

// ---- Overhead questgiver markers: the ask sweep and the status -> model table ----

mod quest_markers;

// ---- The wire chat line as the stock UI's `CHAT_MSG_*` event ----

mod chat_vm;

// ---- The gossip menu: the text latch, the stock `GossipFrame`'s show, its selects ----

mod gossip_vm;
mod quest_log_vm;

// ---- The ground ring under the selected unit: footprint, fade and selector colour ----

mod selection_ring;

// ---- The health-bar plate over a unit: its gate, its seat, and the widget layer's states ----

mod nameplates;
mod smart_rect;

// ---- The ghost body's alpha factor and tint from spell 8326's state kit ----

mod ghost;

// ---- The melee spurt and corpse sparkle models: hardcoded effects resolved off the chain ----

mod fx;

mod exterior;

// ---- In-game interface feed: session state in, FrameXML model; verbs back out ----

mod ui_feed;

// ---- The bag buttons' item-push cards: the icon quad posed for the host's 2D pass ----

mod ui_cards;

// ---- The reference's registered CVars, for the VM the stock interface loads into ----

mod cvar_defaults;

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

    /// The Data directory the chain was opened from: the background WMO
    /// worker opens its own chain there (the handle itself is not shareable).
    data_dir: String,

    mcnk: Vec<McnkChunk>,

    /// Real ADT terrain of the last `bn_load_tile`, when one loaded.
    tile: Option<TileFlat>,

    /// Lowercase ADT path backing `tile` (and `mcnk`), for the same-tile
    /// reload fast path: per-frame height queries must not re-parse the ADT.
    tile_path: String,

    /// Official `benilla_formats::Chain` over the same Data dir, for `load_wmo`
    /// (the WMO reader takes a `Chain`, not the raw `Archive` list).
    chain: Option<benilla_formats::Chain>,

    /// Parsed `Light.dbc`/`LightParams`/`LightIntBand` tables for the zone
    /// atmosphere (fog, sky gradient, sun/ambient), loaded once on the first
    /// `bn_light_sample` and reused every query.
    lights: Option<benilla_formats::LightCatalog>,

    /// Parsed `Spell.dbc`/`SpellIcon` rows for the spell book and cast bar,
    /// loaded once on the first `bn_spell_info` and reused every query.
    spells: Option<benilla_formats::SpellCatalog>,

    /// Parsed WMO geometry by root path, shared by every placement of a model.
    wmo_models: HashMap<String, std::sync::Arc<Vec<benilla_formats::RenderSubmesh>>>,

    /// Distant low-detail WDL of `wdl_map` (the coarse horizon band).
    wdl: Option<benilla_formats::WdlFile>,
    wdl_map: String,

    /// Bumped on every tile load: background flat jobs are keyed by it so a
    /// stale parse can never answer for the tile that replaced it.
    tile_seq: u64,

    /// Baked tile-local WMO geometry by instance index (rebuilt on tile load).
    wmo_flats: HashMap<u32, std::sync::Arc<WmoFlat>>,

    /// Model-space group MLIQ (WMO liquid) meshes by root path: read once per
    /// model, re-placed per instance into the tile's liquid buffer.
    wmo_group_liquids: HashMap<String, Arc<Vec<benilla_formats::LiquidMesh>>>,

    /// Baked tile-local doodad geometry by MDDF index (rebuilt on tile load).
    doodad_flats: HashMap<u32, WmoFlat>,

    /// The tile's WMO-internal doodads (MODD), world-composed; indexed after
    /// the MDDF placements in the merged doodad enumeration.
    wmo_doodads: Vec<WmoDoodadInst>,

    /// Middle-layer player (locomotion, collision, camera) once `bn_player_reset`.
    player: Option<player::Player>,

    /// WMO walking-collision triangles by root path (the gather the player and
    /// camera probe use), shared by every placement of the model.
    wmo_collision: HashMap<String, benilla_formats::CollisionMesh>,

    /// `md5translate.trs`: ADT tile -> minimap BLP hash, loaded on the first
    /// `bn_minimap_tile_path`. The minimap tiles are authored art, not a 3D pass.
    minimap: Option<benilla_formats::MinimapTranslate>,

    /// Character DBCs, parsed once per session. Spawning one humanoid NPC asks
    /// for `CharSections` six times and `ItemDisplayInfo` twice, and every pass
    /// used to re-read the tables from the MPQ (tens of ms per load).
    char_sections: Option<benilla_formats::CharSections>,
    item_display: Option<benilla_formats::ItemDisplayCatalog>,
    char_geosets: Option<benilla_formats::CharacterGeosets>,
}

impl BnWorld {
    /// Load `CharSections.dbc` unless already cached. `false` = no chain or a
    /// read error (the caller keeps its previous failure code).
    fn ensure_char_sections(&mut self) -> bool {
        if self.char_sections.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(s) = benilla_formats::CharSections::load(chain) else {
                return false;
            };
            self.char_sections = Some(s);
        }
        true
    }

    fn ensure_item_display(&mut self) -> bool {
        if self.item_display.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(c) = benilla_formats::load_item_display_catalog(chain) else {
                return false;
            };
            self.item_display = Some(c);
        }
        true
    }

    fn ensure_char_geosets(&mut self) -> bool {
        if self.char_geosets.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(g) = benilla_formats::CharacterGeosets::load(chain) else {
                return false;
            };
            self.char_geosets = Some(g);
        }
        true
    }
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

pub(crate) fn wow_vec_to_ue(vx: f32, vy: f32, vz: f32) -> (f32, f32, f32) {
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

            if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mpq")) {
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

        data_dir: dir.to_string(),

        mcnk: Vec::new(),

        tile: None,

        tile_path: String::new(),

        chain: benilla_formats::Chain::open(data).ok(),

        lights: None,

        spells: None,

        wmo_models: HashMap::new(),

        wdl: None,
        wdl_map: String::new(),

        tile_seq: 0,

        wmo_flats: HashMap::new(),

        wmo_group_liquids: HashMap::new(),

        doodad_flats: HashMap::new(),

        wmo_doodads: Vec::new(),

        player: None,

        wmo_collision: HashMap::new(),

        minimap: None,

        char_sections: None,

        item_display: None,

        char_geosets: None,
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

    unsafe { &*world }.archives.len() as u32
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
    /// Per-liquid-vertex depth coordinate, parallel to `lverts`: the ADT
    /// depth byte through the client's LUT (0..1) and the WMO per-vertex
    /// authored alpha. `liquid.wgsl` carries it in UV1.x and it drives both the
    /// swatch row and the sheet opacity.
    ldepth: Vec<f32>,
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
    build_tile_flat_mesh(&benilla_formats::adt_to_tile_mesh(bytes).ok()?)
}

/// [`build_tile_flat`] over an already parsed mesh (the tile worker parses
/// once and uses it for both the render tile and the collision tile).
fn build_tile_flat_mesh(tile: &benilla_formats::TileMesh) -> Option<TileFlat> {
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
        ldepth: Vec::new(),
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
                out.ldepth.push(m.depths.get(i).copied().unwrap_or(0.0));
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

// ---- Background ADT tile prefetch ----
//
// A tile entering the residency window used to be parsed (twice: MCNK for the
// height field, then the flattened mesh) and have its chunk layer BLPs decoded
// on the game thread - one 10-60 ms hitch per tile walked into. The host names
// every wanted-but-missing tile to `bn_tile_prefetch`; a worker opens its own
// chain, parses the tile once, queues the layer textures through the existing
// decode pool, and parks the result for `bn_load_tile` to install.

struct TilePrep {
    mcnk: Vec<McnkChunk>,

    flat: Option<TileFlat>,

    /// The tile's WMO-internal doodads, resolved on the worker so the adopted
    /// tile's doodad enumeration is complete when the host first samples it.
    wmo_doodads: Arc<Vec<WmoDoodadInst>>,
}

struct TileJob {
    dir: String,

    path: String,

    /// Lowercase path, the cache key (MPQ lookups are case-insensitive).
    key: String,
}

struct TileJobs {
    queue: Mutex<VecDeque<TileJob>>,

    ready: Condvar,
}

fn tile_jobs() -> &'static TileJobs {
    static JOBS: OnceLock<TileJobs> = OnceLock::new();

    JOBS.get_or_init(|| TileJobs {
        queue: Mutex::new(VecDeque::new()),

        ready: Condvar::new(),
    })
}

/// Prepared tiles keyed by lowercase ADT path, least-recently queued first.
/// `None` = the worker is still on it; a ready entry is moved (not cloned)
/// into the world by `bn_load_tile`.
struct TilePreps {
    entries: HashMap<String, Option<Arc<TilePrep>>>,

    order: VecDeque<String>,
}

/// The host spawns wanted tiles nearest-first, so a few prepared tiles are
/// always enough lead; the body also parks its own 5x5 walking window (the
/// ring a border step needs, one tile early), so this covers both.
const TILE_PREFETCH_BUDGET: usize = 32;

fn tile_prep_cache() -> &'static Mutex<TilePreps> {
    static CACHE: OnceLock<Mutex<TilePreps>> = OnceLock::new();

    CACHE.get_or_init(|| {
        Mutex::new(TilePreps {
            entries: HashMap::new(),

            order: VecDeque::new(),
        })
    })
}

fn tile_prep_trim(cache: &mut TilePreps) {
    while cache.order.len() > TILE_PREFETCH_BUDGET {
        let Some(old) = cache.order.pop_front() else {
            return;
        };

        cache.entries.remove(&old);
    }
}

// The body's walking collision used to be re-read and re-parsed on the game
// thread (`player::load_tile`, 40-60 ms) the first tick after it crossed a
// tile border - three hitches in a row when a corner brought three new tiles
// into its 3x3 window. The same ADT is already in the tile worker's hands, so
// the worker parks its collision half here, keyed by the same lowercase path.

struct TileCols {
    entries: HashMap<String, player::TileCol>,

    order: VecDeque<String>,
}

/// The body's 3x3 window, the parked ring around it (the 5x5 prefetch makes a
/// border step ready before it happens), and the tiles it just left.
const COLLISION_TILE_BUDGET: usize = 32;

fn tile_cols() -> &'static Mutex<TileCols> {
    static CACHE: OnceLock<Mutex<TileCols>> = OnceLock::new();

    CACHE.get_or_init(|| {
        Mutex::new(TileCols {
            entries: HashMap::new(),

            order: VecDeque::new(),
        })
    })
}

fn tile_col_key(map: &str, tx: i32, ty: i32) -> String {
    format!("World\\Maps\\{map}\\{map}_{tx}_{ty}.adt").to_ascii_lowercase()
}

/// The parsed collision tile for `map`/`tx`/`ty`, moved out of the cache.
pub(crate) fn collision_tile_take(map: &str, tx: i32, ty: i32) -> Option<player::TileCol> {
    let key = tile_col_key(map, tx, ty);

    let mut cache = tile_cols().lock().ok()?;

    let col = cache.entries.remove(&key)?;

    if let Some(pos) = cache.order.iter().position(|k| k == &key) {
        cache.order.remove(pos);
    }

    Some(col)
}

/// Park a collision tile for `player::ensure_tiles` to take.
pub(crate) fn collision_tile_put(map: &str, tx: i32, ty: i32, col: player::TileCol) {
    collision_tile_put_key(tile_col_key(map, tx, ty), col);
}

fn collision_tile_put_key(key: String, col: player::TileCol) {
    let Ok(mut cache) = tile_cols().lock() else {
        return;
    };

    if cache.entries.insert(key.clone(), col).is_none() {
        cache.order.push_back(key);
    }

    while cache.order.len() > COLLISION_TILE_BUDGET {
        let Some(old) = cache.order.pop_front() else {
            return;
        };

        cache.entries.remove(&old);
    }
}

fn tile_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();

    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("bn-tile".to_string())
            .spawn(|| {
                let mut chain: Option<(String, benilla_formats::Chain)> = None;

                loop {
                    let job = {
                        let jobs = tile_jobs();

                        let mut q = match jobs.queue.lock() {
                            Ok(q) => q,

                            Err(_) => return,
                        };

                        while q.is_empty() {
                            q = match jobs.ready.wait(q) {
                                Ok(q) => q,

                                Err(_) => return,
                            };
                        }

                        q.pop_front()
                    };

                    let Some(job) = job else { continue };

                    let job_t0 = std::time::Instant::now();

                    if chain.as_ref().map_or(true, |(d, _)| d != &job.dir) {
                        chain = benilla_formats::Chain::open(Path::new(&job.dir))
                            .ok()
                            .map(|c| (job.dir.clone(), c));
                    }

                    let mut models: Vec<(String, String)> = Vec::new();

                    let mut wmo_models: Vec<String> = Vec::new();

                    // The WMO roots' props, for the model prefetch after the
                    // publish (the resolved list rides the prep itself).
                    let mut wmo_doodad_models: Vec<(String, String)> = Vec::new();

                    // Held back until the models above are parked: the body
                    // folds whatever is parked on the tick it adopts the tile,
                    // so publishing first means an MPQ parse on that tick.
                    let mut collision_tile: Option<player::TileCol> = None;

                    let prep = chain.as_mut().and_then(|(_, c)| {
                        if !c.contains(&job.path) {
                            return None;
                        }

                        // Filled only when the tile mesh exists; the prep
                        // carries the (possibly empty) list either way.
                        let mut wmo_doodad_resolved: Option<Vec<WmoDoodadInst>> = None;

                        let bytes = c.read_file(&job.path).ok()?;

                        let mcnk = match benilla_adt::parse_adt(&mut Cursor::new(bytes.as_slice()))
                            .ok()?
                        {
                            benilla_adt::ParsedAdt::Root(root) => root.mcnk_chunks,

                            _ => return None,
                        };

                        // One ADT parse feeds both halves: the flattened
                        // render mesh and the body's collision tile.
                        let tile = benilla_formats::adt_to_tile_mesh(&bytes).ok();

                        if let Some(t) = &tile {
                            collision_tile = Some(player::tile_col_from_mesh(t));
                        }

                        let flat = tile.as_ref().and_then(build_tile_flat_mesh);

                        // Queue the chunk layer BLPs now: the terrain stage
                        // then finds them decoded instead of decoding up to
                        // four 256x256 sheets on the game thread.
                        if let Some(t) = &flat {
                            let mut seen = std::collections::HashSet::new();

                            for chunk in &t.chunks {
                                for layer in chunk.layers.iter().chain(std::iter::once(&chunk.tex))
                                {
                                    if !layer.is_empty() && seen.insert(tex_key(layer)) {
                                        tex_request(&job.dir, layer);
                                    }
                                }
                            }

                            // The tile's prop models, for the parse cache and
                            // the body's hull parking (under the raw ADT
                            // path, which is what the collision tile asks for).
                            let mut model_seen = std::collections::HashSet::new();
                            for d in &t.doodads {
                                let model = d.model.trim();
                                if !model.is_empty() && model_seen.insert(model.to_string()) {
                                    models.push((model.to_string(), chain_model_path(model)));
                                }
                            }

                            let mut wmo_seen = std::collections::HashSet::new();
                            for w in &t.wmos {
                                let model = w.model.trim();
                                if !model.is_empty() && wmo_seen.insert(model.to_string()) {
                                    wmo_models.push(model.to_string());
                                }
                            }

                            // The buildings' own props, resolved here (before
                            // the publish) so the host's first doodad count
                            // samples the full enumeration; their models join
                            // the prefetch queue below.
                            let resolved = resolve_wmo_doodads(Some(c), &t.wmos);
                            for d in &resolved {
                                let model = d.model.trim();
                                if !model.is_empty() && model_seen.insert(model.to_string()) {
                                    wmo_doodad_models
                                        .push((model.to_string(), chain_model_path(model)));
                                }
                            }
                            wmo_doodad_resolved = Some(resolved);
                        }

                        Some(TilePrep {
                            mcnk,
                            flat,
                            wmo_doodads: Arc::new(wmo_doodad_resolved.unwrap_or_default()),
                        })
                    });

                    let parse_ms = job_t0.elapsed().as_secs_f64() * 1000.0;

                    if let Ok(mut cache) = tile_prep_cache().lock() {
                        // A trimmed request is not resurrected: the host only
                        // keeps entries it still wants.
                        if cache.entries.contains_key(&job.key) {
                            cache.entries.insert(job.key.clone(), prep.map(Arc::new));
                        }
                    }

                    // Prop model parses run after the prep is published, so
                    // the ADT is never late to the build that wants it: the
                    // build reaches its doodad gather a few hundred ms later.
                    let models_t0 = std::time::Instant::now();

                    if let Some((_, c)) = chain.as_mut() {
                        prefetch_doodad_models(c, &models);

                        prefetch_doodad_models(c, &wmo_doodad_models);

                        // The body folds these meshes into its tile on the tick
                        // it adopts the new ADT; parking them here keeps that
                        // tick free of an MPQ parse. One parking per model is
                        // enough until the body takes it.
                        for m in &wmo_models {
                            if player::wmo_collision_parked(m) {
                                continue;
                            }
                            if let Ok(mesh) = benilla_formats::load_wmo_collision_tris(c, m) {
                                player::wmo_collision_park(m, mesh);
                            }
                        }
                    }

                    // The tile's WMO meshes are parked now, so the body can
                    // adopt the collision tile without a parse on that tick.
                    if let Some(col) = collision_tile.take() {
                        collision_tile_put_key(job.key.clone(), col);
                    }

                    // Group liquids last: the body's MLIQ slice must find them
                    // cached, and a city model's group files are hundreds of
                    // MPQ reads. Parked after the tile is published so this
                    // read never delays the body's adoption.
                    if let Some((_, c)) = chain.as_mut() {
                        for m in &wmo_models {
                            let key = wmo_key(m);
                            if wmo_liquid_has(&key) {
                                continue;
                            }
                            let meshes = read_wmo_group_liquids(Some(c), m);
                            wmo_liquid_store(&key, Arc::new(meshes));
                        }
                    }

                    let models_ms = models_t0.elapsed().as_secs_f64() * 1000.0;
                    let job_ms = job_t0.elapsed().as_secs_f64() * 1000.0;
                    if job_ms > 30.0 {
                        eprintln!(
                            "bn_tile: {} {:.1}ms (parse {:.1}, {} doodads {:.1})",
                            job.path,
                            job_ms,
                            parse_ms,
                            models.len(),
                            models_ms
                        );
                    }
                }
            });
    });
}

/// Prepare `World\Maps\<map>\<map>_<x>_<z>.adt` off the game thread, so a later
/// `bn_load_tile` installs it without reading the MPQ or parsing. 1 = ready,
/// 3 = already queued, 0 = queued now, 2 = bad arguments.
#[no_mangle]
pub extern "C" fn bn_tile_prefetch(
    world: *const BnWorld,
    map_name: *const c_char,
    x: i32,
    z: i32,
) -> u32 {
    if world.is_null() {
        return 2;
    }

    let Some(map) = cstr_to_str(map_name) else {
        return 2;
    };

    // SAFETY: caller owns a valid const handle.
    let w = unsafe { &*world };

    prefetch_tile_inner(w, map, x, z, false)
}

/// Queue the body's walking window to the tile worker: the 3x3 collision ring
/// plus the ring beyond it. The host streams around the camera focus, which can
/// lag the tile the body is about to walk into; the worker parses the same ADT
/// for the render mesh anyway, so this only asks it to stay ahead. Queuing the
/// outer ring too means the ring a border step will need is already parsed when
/// the body arrives, instead of a 27-46 ms ADT parse on that tick. The queue
/// dedupes, so a repeat call is a cache lookup.
///
/// Nearest tiles go to the head of the queue, farthest first so the centre
/// ends up at the front: a job takes tens of ms (its doodad models take far
/// longer), and behind whatever the stream asked for earlier the ring the body
/// is about to step into was the last thing parsed.
pub(crate) fn player_prefetch_window(w: &BnWorld, map: &str, tx: i32, ty: i32) {
    let mut window: Vec<(i32, i32, i32)> = Vec::with_capacity(25);

    for dx in -2..=2 {
        for dy in -2..=2 {
            let (x, z) = (tx + dx, ty + dy);

            if (0..=63).contains(&x) && (0..=63).contains(&z) {
                window.push((dx * dx + dy * dy, x, z));
            }
        }
    }

    window.sort_unstable_by_key(|&(d, _, _)| d);

    for &(_, x, z) in window.iter().rev() {
        prefetch_tile_inner(w, map, x, z, true);
    }
}

/// Lift a still-queued job to the head of the queue.
fn promote_tile_job(key: &str) {
    let Ok(mut q) = tile_jobs().queue.lock() else {
        return;
    };

    let Some(pos) = q.iter().position(|j| j.key == key) else {
        return;
    };

    let Some(job) = q.remove(pos) else {
        return;
    };

    q.push_front(job);
}

fn prefetch_tile_inner(w: &BnWorld, map: &str, x: i32, z: i32, front: bool) -> u32 {
    let path = format!("World\\Maps\\{map}\\{map}_{x}_{z}.adt");

    let key = path.to_ascii_lowercase();

    if w.tile_path == key {
        return 1;
    }

    let mut queued = true;

    {
        let Ok(mut cache) = tile_prep_cache().lock() else {
            return 2;
        };

        match cache.entries.get(&key) {
            Some(Some(_)) => return 1,

            Some(None) => {}

            None => {
                cache.entries.insert(key.clone(), None);

                cache.order.push_back(key.clone());

                tile_prep_trim(&mut cache);

                queued = false;
            }
        }
    }

    if queued {
        if front {
            promote_tile_job(&key);
        }

        return 3;
    }

    tile_worker();

    if let Ok(mut q) = tile_jobs().queue.lock() {
        let job = TileJob {
            dir: w.data_dir.clone(),

            path,

            key,
        };

        if front {
            q.push_front(job);
        } else {
            q.push_back(job);
        }

        tile_jobs().ready.notify_one();
    }

    0
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

    // A prefetched tile was read and parsed off the game thread: take it over
    // without touching the MPQ. The cache held the only reference, so the
    // unwrap never copies; a failed prep falls through to the direct path.
    let prefetched = tile_prep_cache()
        .lock()
        .ok()
        .and_then(|mut c| c.entries.remove(&path_lower))
        .flatten()
        .and_then(|p| Arc::try_unwrap(p).ok());

    if let Some(prep) = prefetched {
        w.mcnk = prep.mcnk;

        w.tile = prep.flat;

        w.tile_path = path_lower;

        w.wmo_doodads = Arc::try_unwrap(prep.wmo_doodads).unwrap_or_else(|a| (*a).clone());

        // Instance indices address the new tile's MODF/MDDF lists; drop stale
        // bakes (models stay cached: path-keyed).
        w.tile_seq += 1;

        w.wmo_flats.clear();

        w.doodad_flats.clear();

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
                    // No worker ran for this tile: resolve the buildings'
                    // props here. The root cache keeps every repeat free;
                    // the first city tile pays one root read + parse.
                    let wmos = w.tile.as_ref().map(|t| t.wmos.clone()).unwrap_or_default();
                    w.wmo_doodads = resolve_wmo_doodads(w.chain.as_mut(), &wmos);

                    // Instance indices address the new tile's MODF/MDDF lists;
                    // drop stale bakes (models stay cached: path-keyed).
                    w.tile_seq += 1;
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

// ---- streaming: the tile residency window as a pure function:
// inner = 1 + trunc(farclip / CHUNK_SIZE) chunks,
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

pub extern "C" fn bn_terrain_fill_indices(world: *const BnWorld, out: *mut u16, cap: u32) -> u32 {
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

/// Fill `out` (pre-allocated) with the per-liquid-vertex depth coordinate,
/// parallel to `bn_liquid_fill_vertices`. Returns count written.
///
/// `liquid.wgsl` reads this as UV1.x: on ADT water it indexes the depth swatch
/// (and its alpha), on WMO water it is the authored per-vertex opacity.
#[no_mangle]
pub extern "C" fn bn_liquid_fill_depths(world: *const BnWorld, out: *mut f32, cap: u32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(t) = w.tile.as_ref() else { return 0 };
    let n = t.ldepth.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(t.ldepth.as_ptr(), out, n) };
    n as u32
}

/// The animated liquid frame sets per material class (0 water, 1 ocean,
/// 2 magma): `XTextures\<dir>\<stem>.<1..=count>.blp`, the client's fixed
/// queues (`lake_a`, `ocean_h`, `lava`).
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

pub extern "C" fn bn_terrain_chunk_range(world: *const BnWorld, i: u32, out: *mut u32) -> u32 {
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
            unsafe {
                *out.add(k) = 0;
            }
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
    /// Blend class (0..4, `0x100` = additive), `bn_doodad_blend`'s encoding.
    blend: u32,
    /// The WMO group this batch was authored in (`u16::MAX` when the model's
    /// group table is unknown): the portal PVS answers per group.
    group: u16,
}

/// One WMO instance baked into tile-local UE axes: `world = position + in_wow·v`,
/// then `(world − centre)` through the WoW->UE mirror, contiguous per submesh.
pub struct WmoFlat {
    verts: Vec<BnVertex>,
    idx: Vec<u32>,
    subs: Vec<WmoSub>,
    /// The placement's world bounding sphere (WoW yards): the far-clip test's
    /// `center`/`radius` inputs. Baked from the
    /// same transformed positions the verts carry, so it needs no second pass.
    sphere: ([f32; 3], f32),
    /// The placement's world AABB (WoW yards): min and max corners.
    aabb: ([f32; 3], [f32; 3]),
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
        q_mul(
            q_rot_y(ry - PI),
            q_mul(q_rot_z(-rx), q_rot_x(rz - FRAC_PI_2)),
        ),
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
    build_wmo_flat_q(
        subs,
        position,
        placement_quat(rotation_deg),
        center,
        scale,
        model_path,
        chain,
    )
}

/// The same bake with the world quaternion prepared by the caller: a WMO's
/// own doodads compose the placement quaternion with the MODD orientation.
fn build_wmo_flat_q(
    subs: &[benilla_formats::RenderSubmesh],
    position: [f32; 3],
    q: [f32; 4],
    center: (f32, f32),
    scale: f32,
    model_path: &str,
    chain: Option<&benilla_formats::Chain>,
) -> WmoFlat {
    let mut out = WmoFlat {
        verts: Vec::new(),
        idx: Vec::new(),
        subs: Vec::new(),
        sphere: ([0.0; 3], 0.0),
        aabb: ([0.0; 3], [0.0; 3]),
    };
    // World-space AABB of the placement, for the bounding sphere below.
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    let group_of = wmo_groups_lookup(&wmo_key(model_path));
    for (k, sm) in subs.iter().enumerate() {
        let group = group_of
            .as_ref()
            .and_then(|g| g.get(k).copied())
            .unwrap_or(u16::MAX);
        let vtx0 = out.verts.len() as u32;
        for (i, p) in sm.positions.iter().enumerate() {
            let r = q_rot(q, [p[0] * scale, p[1] * scale, p[2] * scale]);
            let (wx, wy, wz) = (r[0] + position[0], r[1] + position[1], r[2] + position[2]);
            lo[0] = lo[0].min(wx);
            lo[1] = lo[1].min(wy);
            lo[2] = lo[2].min(wz);
            hi[0] = hi[0].max(wx);
            hi[1] = hi[1].max(wy);
            hi[2] = hi[2].max(wz);
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
            group,
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
    if !out.verts.is_empty() {
        let c = [
            (lo[0] + hi[0]) * 0.5,
            (lo[1] + hi[1]) * 0.5,
            (lo[2] + hi[2]) * 0.5,
        ];
        // Circumsphere (half the box diagonal): the far-clip wall must never
        // eat a corner the box test would have kept.
        let r = (0..3)
            .map(|a| hi[a] - lo[a])
            .fold(0.0f32, |acc, v| acc + v * v)
            .sqrt()
            * 0.5;
        out.sphere = (c, r.max(1.0));
        out.aabb = (lo, hi);
    }
    out
}

/// The baked flat for instance `i`, or `None` before `bn_wmo_instance_load`.
fn wmo_flat<'a>(world: *const BnWorld, i: u32) -> Option<&'a WmoFlat> {
    if world.is_null() {
        return None;
    }
    // SAFETY: caller owns a valid const handle.
    unsafe { &*world }.wmo_flats.get(&i).map(|f| f.as_ref())
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

/// Group MLIQ surfaces keyed by normalized model path. Filled by the WMO
/// background worker (and by the sync fallback), so the gather only has to
/// transform them. A city model like Stormwind reads 3042 group files.
fn wmo_liquid_cache() -> &'static Mutex<HashMap<String, Arc<Vec<benilla_formats::LiquidMesh>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<benilla_formats::LiquidMesh>>>>> =
        OnceLock::new();

    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn wmo_liquid_has(key: &str) -> bool {
    wmo_liquid_cache()
        .lock()
        .map(|c| c.contains_key(key))
        .unwrap_or(false)
}

fn wmo_liquid_lookup(key: &str) -> Option<Arc<Vec<benilla_formats::LiquidMesh>>> {
    wmo_liquid_cache().lock().ok()?.get(key).cloned()
}

fn wmo_liquid_store(key: &str, meshes: Arc<Vec<benilla_formats::LiquidMesh>>) {
    if let Ok(mut cache) = wmo_liquid_cache().lock() {
        cache.insert(key.to_string(), meshes);
    }
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
        let key = wmo_key(model_path);

        let meshes = match wmo_liquid_lookup(&key) {
            Some(m) => m,

            None => {
                let meshes = Arc::new(read_wmo_group_liquids(w.chain.as_mut(), model_path));

                wmo_liquid_store(&key, meshes.clone());

                meshes
            }
        };

        w.wmo_group_liquids.insert(model_path.to_string(), meshes);
    }
    // Disjoint field borrows: the tile buffer and the path-keyed cache.
    let (Some(tile), Some(meshes)) = (w.tile.as_mut(), w.wmo_group_liquids.get(model_path)) else {
        return;
    };
    let q = placement_quat(rotation_deg);
    for m in meshes.iter() {
        // u16 builder cap, same as the ADT liquid loop.
        if tile.lverts.len() + m.positions.len() > u16::MAX as usize {
            return;
        }
        let vtx0 = tile.lverts.len() as u32;
        for (vi, p) in m.positions.iter().enumerate() {
            let r = q_rot(q, *p);
            let (wx, wy, wz) = (r[0] + position[0], r[1] + position[1], r[2] + position[2]);
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
            // WMO water carries the authored per-vertex opacity byte through
            // the zone's alpha ramp; the client reads it as the sheet alpha.
            tile.ldepth.push(m.depths.get(vi).copied().unwrap_or(0.0));
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

// ---- Background WMO parse ----
//
// A city WMO (Stormwind) costs ~700 ms to parse, and the tile's gather stage
// used to pay that on the game thread in one frame. The host names the tile's
// WMO placements right after `bn_load_tile`; a worker opens its own chain over
// the same Data directory and parses them while the tile keeps building.
// `bn_wmo_instance_load` then consumes the shared result, and while the parse
// is still running `bn_wmo_instance_pending` lets the host retry on a later
// frame instead of freezing the frame.

use std::sync::Arc;

struct WmoEntry {
    ready: bool,

    subs: Option<Arc<Vec<benilla_formats::RenderSubmesh>>>,
}

/// Case-folded root path, the key both the tile's placements and the host's
/// prefetch requests use.
fn wmo_key(path: &str) -> String {
    path.trim().to_ascii_lowercase()
}

fn wmo_cache() -> &'static Mutex<HashMap<String, WmoEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, WmoEntry>>> = OnceLock::new();

    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One background WMO job: parse a model, or bake one placement of it for a
/// specific tile (the bake needs the placement's transform and the tile
/// centre, so it can only be prepared once the tile is loaded).
enum WmoJob {
    Parse {
        dir: String,
        key: String,
        model: String,
    },
    Flat {
        dir: String,
        /// Tile path the placement belongs to (the flat cache key).
        path: String,
        i: u32,
        /// MODF `unique_id`: the key the far-clip cull addresses this placement by.
        uid: u32,
        model: String,
        position: [f32; 3],
        rotation: [f32; 3],
        center: (f32, f32),
    },
}

struct WmoJobs {
    queue: Mutex<VecDeque<WmoJob>>,

    ready: Condvar,
}

fn wmo_jobs() -> &'static WmoJobs {
    static JOBS: OnceLock<WmoJobs> = OnceLock::new();

    JOBS.get_or_init(|| WmoJobs {
        queue: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
    })
}

fn wmo_lookup(key: &str) -> Option<Arc<Vec<benilla_formats::RenderSubmesh>>> {
    let cache = wmo_cache().lock().ok()?;

    cache.get(key).and_then(|e| e.subs.clone())
}

/// The WMO group each submesh of a model came from, parallel to `load_wmo`'s
/// submesh list. The portal PVS answers per group, so a placement's geometry is
/// only toggleable one room at a time while each batch knows its group.
fn wmo_groups() -> &'static Mutex<HashMap<String, Arc<Vec<u16>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u16>>>>> = OnceLock::new();

    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn wmo_groups_lookup(key: &str) -> Option<Arc<Vec<u16>>> {
    wmo_groups().lock().ok()?.get(key).cloned()
}

fn wmo_groups_store(key: &str, groups: Vec<u16>) {
    if let Ok(mut cache) = wmo_groups().lock() {
        cache.insert(key.to_string(), Arc::new(groups));
    }
}

/// `load_wmo`'s walk keeping the group index: the root, then every group file,
/// one submesh per render batch.
fn load_wmo_grouped(
    chain: &mut benilla_formats::Chain,
    raw_path: &str,
) -> Option<(Vec<benilla_formats::RenderSubmesh>, Vec<u16>)> {
    let root_path = raw_path.to_ascii_lowercase();
    let bytes = chain.read_file(&root_path).ok()?;
    let root = benilla_formats::parse_wmo_root(&bytes).ok()?;
    let stem = root_path.strip_suffix(".wmo").unwrap_or(&root_path);
    let mut subs: Vec<benilla_formats::RenderSubmesh> = Vec::new();
    let mut groups: Vec<u16> = Vec::new();
    for gi in 0..root.group_count() {
        let Ok(gbytes) = chain.read_file(&format!("{stem}_{gi:03}.wmo")) else {
            continue;
        };
        let gs = benilla_formats::wmo_group_submeshes(&gbytes, &root).ok()?;
        groups.resize(subs.len() + gs.len(), gi as u16);
        subs.extend(gs);
    }
    Some((subs, groups))
}

fn wmo_pending(key: &str) -> bool {
    wmo_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(key).map(|e| !e.ready))
        .unwrap_or(false)
}

struct WmoFlatEntry {
    ready: bool,

    flat: Option<Arc<WmoFlat>>,
}

/// Baked placements of one tile path. Keyed by path, not by the tile serial:
/// a streaming burst loads the neighbour tiles right after the focus tile, and
/// the old serial-keyed table was wiped by every one of those loads, so the
/// focus tile's prefetch was always thrown away and its city WMO re-baked
/// synchronously on the game thread (~340 ms for Stormwind).
struct WmoFlatTile {
    entries: HashMap<u32, WmoFlatEntry>,

    bytes: u64,
}

struct WmoFlatCache {
    tiles: HashMap<String, WmoFlatTile>,

    /// Least-recently prefetched first; trimmed past the byte budget.
    order: VecDeque<String>,

    bytes: u64,
}

/// A city tile bakes ~36 MB (Stormwind: 815k verts + 2.5M index slots), so the
/// pooled bakes need a cap; the 3x3 residency window fits well inside it.
const WMO_FLAT_CACHE_BYTES: u64 = 384 * 1024 * 1024;

fn wmo_flat_cache() -> &'static Mutex<WmoFlatCache> {
    static CACHE: OnceLock<Mutex<WmoFlatCache>> = OnceLock::new();

    CACHE.get_or_init(|| {
        Mutex::new(WmoFlatCache {
            tiles: HashMap::new(),

            order: VecDeque::new(),

            bytes: 0,
        })
    })
}

fn wmo_flat_size(flat: &WmoFlat) -> u64 {
    (flat.verts.len() * std::mem::size_of::<BnVertex>() + flat.idx.len() * 4) as u64
}

fn wmo_flat_touch(cache: &mut WmoFlatCache, path: &str) {
    if let Some(pos) = cache.order.iter().position(|p| p == path) {
        cache.order.remove(pos);
    }

    cache.order.push_back(path.to_string());
}

/// Drop the least recently prefetched tiles past the byte budget. `keep` is
/// the tile being filled right now and is never dropped.
fn wmo_flat_trim(cache: &mut WmoFlatCache, keep: &str) {
    while cache.bytes > WMO_FLAT_CACHE_BYTES && cache.order.len() > 1 {
        let Some(victim) = cache.order.front().cloned() else {
            break;
        };

        if victim == keep {
            break;
        }

        cache.order.pop_front();

        if let Some(t) = cache.tiles.remove(&victim) {
            cache.bytes = cache.bytes.saturating_sub(t.bytes);
        }
    }
}

/// Some(Some(flat)) = baked, Some(None) = failed, None = not requested.
fn wmo_flat_lookup(path: &str, i: u32) -> Option<Option<Arc<WmoFlat>>> {
    let cache = wmo_flat_cache().lock().ok()?;

    cache
        .tiles
        .get(path)
        .and_then(|t| t.entries.get(&i))
        .map(|e| e.flat.clone())
}

fn wmo_flat_pending(path: &str, i: u32) -> bool {
    wmo_flat_cache()
        .lock()
        .ok()
        .and_then(|c| {
            c.tiles
                .get(path)
                .and_then(|t| t.entries.get(&i))
                .map(|e| !e.ready)
        })
        .unwrap_or(false)
}

/// World bounding spheres of every WMO placement baked so far, keyed by MODF
/// `unique_id`. The flat cache is released once the host copies a tile's
/// geometry, but the far-clip cull queries long after, by uid alone.
fn wmo_spheres() -> &'static Mutex<HashMap<u32, ([f32; 3], f32)>> {
    static SPHERES: OnceLock<Mutex<HashMap<u32, ([f32; 3], f32)>>> = OnceLock::new();
    SPHERES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn wmo_sphere_store(uid: u32, flat: Option<&WmoFlat>) {
    if let (Ok(mut m), Some(f)) = (wmo_spheres().lock(), flat) {
        m.insert(uid, f.sphere);
    }
}

/// Doodad placement bounding spheres by MDDF/WMO-doodad uid, surviving the
/// per-tile `doodad_flats` release the same way the WMO table does.
fn doodad_spheres() -> &'static Mutex<HashMap<u32, ([f32; 3], f32)>> {
    static SPHERES: OnceLock<Mutex<HashMap<u32, ([f32; 3], f32)>>> = OnceLock::new();
    SPHERES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn wmo_flat_store(path: &str, i: u32, flat: Option<Arc<WmoFlat>>) {
    if let Ok(mut cache) = wmo_flat_cache().lock() {
        let add = flat.as_ref().map_or(0, |f| wmo_flat_size(f));

        // Only fill a slot the host still wants: trimming drops a tile's
        // entries, so a job that finishes after its tile was evicted finds
        // nothing here and its result is dropped.
        if let Some(t) = cache.tiles.get_mut(path) {
            if let Some(e) = t.entries.get_mut(&i) {
                if !e.ready {
                    let old = e.flat.as_ref().map_or(0, |f| wmo_flat_size(f));

                    *e = WmoFlatEntry { ready: true, flat };

                    t.bytes = t.bytes.saturating_sub(old) + add;

                    cache.bytes = cache.bytes.saturating_sub(old) + add;
                }
            }
        }
    }
}

/// Drop every baked placement of the active tile. The host calls this once its
/// gather has copied the geometry: a city tile's flats are ~36 MB and the
/// residency window holds several of them, so keeping them past their use just
/// forces the byte budget to evict tiles that are still being assembled.
fn wmo_flat_release(path: &str) {
    if path.is_empty() {
        return;
    }

    if let Ok(mut cache) = wmo_flat_cache().lock() {
        if let Some(t) = cache.tiles.remove(path) {
            cache.bytes = cache.bytes.saturating_sub(t.bytes);
        }

        if let Some(pos) = cache.order.iter().position(|p| p == path) {
            cache.order.remove(pos);
        }
    }
}

fn wmo_worker() {
    static STARTED: OnceLock<()> = OnceLock::new();

    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("bn-wmo".to_string())
            .spawn(|| {
                let mut chain: Option<(String, benilla_formats::Chain)> = None;

                loop {
                    let job = {
                        let jobs = wmo_jobs();

                        let mut q = match jobs.queue.lock() {
                            Ok(q) => q,

                            Err(_) => return,
                        };

                        while q.is_empty() {
                            q = match jobs.ready.wait(q) {
                                Ok(q) => q,

                                Err(_) => return,
                            };
                        }

                        q.pop_front()
                    };

                    let Some(job) = job else { continue };

                    let dir = match &job {
                        WmoJob::Parse { dir, .. } | WmoJob::Flat { dir, .. } => dir.clone(),
                    };

                    if chain.as_ref().map_or(true, |(d, _)| d != &dir) {
                        chain = benilla_formats::Chain::open(Path::new(&dir))
                            .ok()
                            .map(|c| (dir, c));
                    }

                    match job {
                        WmoJob::Parse { key, model, .. } => {
                            let parsed = chain
                                .as_mut()
                                .and_then(|(_, c)| load_wmo_grouped(c, &model));
                            let (subs, groups) = match parsed {
                                Some((s, g)) => (Some(Arc::new(s)), Some(g)),
                                None => (None, None),
                            };

                            if let Some(g) = groups {
                                wmo_groups_store(&key, g);
                            }
                            if let Ok(mut cache) = wmo_cache().lock() {
                                cache.insert(key, WmoEntry { ready: true, subs });
                            }
                        }

                        WmoJob::Flat {
                            path,
                            i,
                            uid,
                            model,
                            position,
                            rotation,
                            center,
                            ..
                        } => {
                            if chain.is_none() {
                                wmo_flat_store(&path, i, None);

                                continue;
                            }

                            let key = wmo_key(&model);

                            let cached = wmo_cache()
                                .lock()
                                .ok()
                                .and_then(|c| c.get(&key).and_then(|e| e.subs.clone()));

                            let subs = match cached {
                                Some(s) => Some(s),

                                None => {
                                    let parsed = chain
                                        .as_mut()
                                        .and_then(|(_, c)| load_wmo_grouped(c, &model))
                                        .map(|(s, g)| {
                                            wmo_groups_store(&key, g);
                                            Arc::new(s)
                                        });

                                    if let (Ok(mut cache), Some(s)) = (wmo_cache().lock(), &parsed)
                                    {
                                        cache.insert(
                                            key.clone(),
                                            WmoEntry {
                                                ready: true,
                                                subs: Some(s.clone()),
                                            },
                                        );
                                    }

                                    parsed
                                }
                            };

                            let flat = match (subs, chain.as_ref()) {
                                (Some(s), Some((_, c))) => Some(Arc::new(build_wmo_flat(
                                    &s,
                                    position,
                                    rotation,
                                    center,
                                    1.0,
                                    &model,
                                    Some(c),
                                ))),

                                _ => None,
                            };

                            // The placement's MLIQ surfaces are read here too:
                            // a city model's 3042 group files cost ~320 ms of
                            // MPQ reads, which used to land on the game thread
                            // inside the gather. bn_wmo_instance_load then finds
                            // them cached and only transforms them.
                            if !wmo_liquid_has(&key) {
                                if let Some((_, c)) = chain.as_mut() {
                                    let meshes = read_wmo_group_liquids(Some(c), &model);

                                    wmo_liquid_store(&key, Arc::new(meshes));
                                }
                            }

                            if !exterior::nav_known(&key) {
                                if let Some((_, c)) = chain.as_mut() {
                                    let nav = exterior::load_nav(c, &key);

                                    exterior::store_nav(&key, nav);
                                }
                            }

                            wmo_sphere_store(uid, flat.as_deref());

                            if let Some(f) = flat.as_deref() {
                                exterior::register_wmo(uid, &key, position, rotation, f.aabb);
                            }

                            wmo_flat_store(&path, i, flat);
                        }
                    }
                }
            });
    });
}

/// Hand the just-loaded tile's WMO placements to the background baker.
/// Returns the number of jobs queued. The host calls this right after
/// `bn_load_tile`; every instance then bakes (parse + transform) off the game
/// thread while the tile's remaining stages run.

#[no_mangle]
pub extern "C" fn bn_wmo_prefetch_tile(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }

    // SAFETY: caller owns a valid const handle.

    let w = unsafe { &*world };

    let Some(tile) = w.tile.as_ref() else {
        return 0;
    };

    let dir = w.data_dir.clone();

    let path = w.tile_path.clone();

    if path.is_empty() {
        return 0;
    }

    let mut jobs = Vec::new();

    {
        let mut cache = match wmo_flat_cache().lock() {
            Ok(c) => c,

            Err(_) => return 0,
        };

        wmo_flat_touch(&mut cache, &path);

        let entries = cache
            .tiles
            .entry(path.clone())
            .or_insert_with(|| WmoFlatTile {
                entries: HashMap::new(),

                bytes: 0,
            });

        for (i, inst) in tile.wmos.iter().enumerate() {
            let i = i as u32;

            if entries.entries.contains_key(&i) {
                continue;
            }

            entries.entries.insert(
                i,
                WmoFlatEntry {
                    ready: false,
                    flat: None,
                },
            );

            jobs.push(WmoJob::Flat {
                dir: dir.clone(),
                path: path.clone(),
                i,
                uid: inst.unique_id,
                model: inst.model.clone(),
                position: inst.position,
                rotation: inst.rotation,
                center: tile.center,
            });
        }

        wmo_flat_trim(&mut cache, &path);
    }

    let n = jobs.len() as u32;

    wmo_worker();

    if let Ok(mut q) = wmo_jobs().queue.lock() {
        for job in jobs {
            q.push_back(job);
        }

        wmo_jobs().ready.notify_one();
    }

    n
}

/// Release the active tile's baked placements once the host has copied them.
/// Pairs with `bn_wmo_prefetch_tile`; a later prefetch of the same tile bakes
/// it again.

#[no_mangle]
pub extern "C" fn bn_wmo_release_tile(world: *const BnWorld) {
    if world.is_null() {
        return;
    }

    // SAFETY: caller owns a valid const handle.

    let w = unsafe { &*world };

    wmo_flat_release(&w.tile_path);
}

/// 1 while instance `i` of the loaded tile is still being baked by the worker.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_pending(world: *const BnWorld, i: u32) -> u32 {
    if world.is_null() {
        return 0;
    }

    // SAFETY: caller owns a valid const handle.

    let w = unsafe { &*world };

    if w.wmo_flats.contains_key(&i) {
        return 0;
    }

    wmo_flat_pending(&w.tile_path, i) as u32
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
/// uid; the client dedups by it, and so does the host.
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
    let Some((model_path, pos, rot, center, uid)) = w.tile.as_ref().and_then(|t| {
        t.wmos.get(i as usize).map(|inst| {
            (
                inst.model.clone(),
                inst.position,
                inst.rotation,
                t.center,
                inst.unique_id,
            )
        })
    }) else {
        return 0;
    };

    // A background bake normally waits for us: consume it instead of building
    // ~840k Stormwind vertices on the game thread. A pending bake answers 0
    // (the host retries next frame); only an unprefetched tile bakes here.
    match wmo_flat_lookup(&w.tile_path, i) {
        Some(Some(flat)) => {
            let n = flat.subs.len() as u32;

            w.wmo_flats.insert(i, flat);

            append_wmo_liquids(w, &model_path, pos, rot, center);

            return n;
        }

        Some(None) => return 0,

        None => {}
    }

    let key = wmo_key(&model_path);
    if !w.wmo_models.contains_key(&key) {
        if let Some(subs) = wmo_lookup(&key) {
            w.wmo_models.insert(key.clone(), subs);
        } else if wmo_pending(&key) {
            return 0;
        } else {
            let Some(chain) = w.chain.as_mut() else {
                return 0;
            };
            let Some((subs, groups)) = load_wmo_grouped(chain, &model_path) else {
                return 0;
            };
            let subs = Arc::new(subs);
            wmo_groups_store(&key, groups);
            w.wmo_models.insert(key.clone(), subs.clone());
            if let Ok(mut cache) = wmo_cache().lock() {
                cache.insert(
                    key.clone(),
                    WmoEntry {
                        ready: true,
                        subs: Some(subs),
                    },
                );
            }
        }
    }
    let subs = &w.wmo_models[&key];
    let flat = build_wmo_flat(subs, pos, rot, center, 1.0, &model_path, w.chain.as_ref());
    let n = flat.subs.len() as u32;
    wmo_sphere_store(uid, Some(&flat));
    if !exterior::nav_known(&key) {
        let nav = w.chain.as_mut().and_then(|c| exterior::load_nav(c, &key));
        exterior::store_nav(&key, nav);
    }
    exterior::register_wmo(uid, &key, pos, rot, flat.aabb);
    w.wmo_flats.insert(i, Arc::new(flat));
    // MLIQ (Stormwind canals): place the model's group liquids beside the ADT
    // MCLQ so the host's liquid pass picks them up unchanged. The cache above
    // early-returns, so each instance appends exactly once per tile rebuild
    // (`bn_load_tile` clears wmo_flats together with the old tile buffer).
    append_wmo_liquids(w, &model_path, pos, rot, center);
    n
}

/// World-space placement (WoW yards, x/y/z) of WMO instance `i` of the
/// resident tile. A city-scale WMO is listed in every ADT it overlaps; the
/// host derives the placement's home ADT from this so exactly one resident
/// tile draws it, deterministically, instead of racing for the claim.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_position(world: *const BnWorld, i: u32, out: *mut f32) -> u32 {
    if world.is_null() || out.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    let Some(p) = w
        .tile
        .as_ref()
        .and_then(|t| t.wmos.get(i as usize))
        .map(|inst| inst.position)
    else {
        return 0;
    };
    unsafe {
        *out = p[0];
        *out.add(1) = p[1];
        *out.add(2) = p[2];
    }
    3
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
    let Some(f) = wmo_flat(world, i) else {
        return 0;
    };
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
    let Some(f) = wmo_flat(world, i) else {
        return 0;
    };
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

/// Copy `count` vertices starting at `first` of instance `i` into `out`
/// (0-based, capacity `cap`). Returns the number copied. The host walks a
/// city WMO's ~815k vertices in chunks so no single frame copies them all.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_fill_vertices_range(
    world: *const BnWorld,
    i: u32,
    first: u32,
    count: u32,
    out: *mut BnVertex,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = wmo_flat(world, i) else {
        return 0;
    };
    let start = (first as usize).min(f.verts.len());
    let n = (f.verts.len() - start)
        .min(count as usize)
        .min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(f.verts.as_ptr().add(start), out, n) };
    n as u32
}

/// Fill index slots `[first, first + count)` of instance `i` into `out`
/// (0-based, capacity `cap`), one BnTriangle per slot as
/// `bn_wmo_instance_fill_triangles` does. Returns the number of slots walked
/// (the destination advance), which is 0 past the end.
#[no_mangle]
pub extern "C" fn bn_wmo_instance_fill_triangles_range(
    world: *const BnWorld,
    i: u32,
    first: u32,
    count: u32,
    out: *mut BnTriangle,
    cap: u32,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    let Some(f) = wmo_flat(world, i) else {
        return 0;
    };
    let begin = (first as usize).min(f.idx.len());
    let n = (f.idx.len() - begin).min(count as usize).min(cap as usize);
    let end = begin + n;
    for (j, sm) in f.subs.iter().enumerate() {
        let sub0 = sm.idx0 as usize;
        if sub0 >= end {
            break;
        }
        let sub_last = sub0 + sm.idx_n as usize;
        // First triangle start at or after `begin`; a triangle is written when
        // its START slot lies in the range, even if its last two index slots
        // run past `end`. The caller's chunk boundary otherwise dropped the
        // straddling triangle (one per chunk per instance).
        let mut k = sub0 + begin.saturating_sub(sub0).div_ceil(3) * 3;
        while k + 2 < sub_last {
            if k >= end {
                break;
            }
            unsafe {
                *out.add(k - begin) = BnTriangle {
                    a: f.idx[k],
                    b: f.idx[k + 1],
                    c: f.idx[k + 2],
                    material: j.min(255) as u8,
                    flags: if sm.two_sided { 1 } else { 0 },
                };
            }
            k += 3;
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
        *out.add(4) = u32::from(sm.group);
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

/// Whether the WMO placement `uid` sits inside the far-clip wall, the one CPU
/// test benilla's draw gate runs on it: planar
/// depth along camera-forward to the nearest point of the placement's world
/// bounding sphere, compared against `farclip`. `eye` and `fwd` are three
/// floats each in WoW world coordinates (yards, x north, y west, z up), `fwd`
/// unit. Writes 1 (drawable) or 0 (behind the wall) to `out`; returns 1 when
/// the placement's sphere is known, 0 when it was never baked (fail-open:
/// `out` is left at 1, like the portal gate on an out-of-range room).
#[no_mangle]
pub extern "C" fn bn_wmo_drawable(
    uid: u32,
    eye: *const f32,
    fwd: *const f32,
    farclip: f32,
    out: *mut u32,
) -> u32 {
    if eye.is_null() || fwd.is_null() || out.is_null() {
        return 0;
    }
    unsafe { *out = 1 };
    let Some((c, r)) = wmo_spheres().lock().ok().and_then(|m| m.get(&uid).copied()) else {
        return 0;
    };
    // SAFETY: caller guarantees two valid three-float arrays.
    let (e, f) = unsafe {
        (
            [*eye, *eye.add(1), *eye.add(2)],
            [*fwd, *fwd.add(1), *fwd.add(2)],
        )
    };
    let depth = (c[0] - e[0]) * f[0] + (c[1] - e[1]) * f[1] + (c[2] - e[2]) * f[2] - r;
    // The client's `farclip` range: 177 to 777.
    let wall = farclip.clamp(177.0, 777.0);
    unsafe { *out = (depth <= wall) as u32 };
    1
}

/// Whether the doodad placement `uid` (MDDF, or a WMO-internal MODD's
/// synthetic uid) sits inside the far-clip wall — the same
/// test as `bn_wmo_drawable`, which the client runs on doodads too
/// (doodads and WMOs share the distance gate). Same
/// fail-open contract on an unknown sphere.
#[no_mangle]
pub extern "C" fn bn_doodad_drawable(
    uid: u32,
    eye: *const f32,
    fwd: *const f32,
    farclip: f32,
    out: *mut u32,
) -> u32 {
    if eye.is_null() || fwd.is_null() || out.is_null() {
        return 0;
    }
    unsafe { *out = 1 };
    let Some((c, r)) = doodad_spheres()
        .lock()
        .ok()
        .and_then(|m| m.get(&uid).copied())
    else {
        return 0;
    };
    // SAFETY: caller guarantees two valid three-float arrays.
    let (e, f) = unsafe {
        (
            [*eye, *eye.add(1), *eye.add(2)],
            [*fwd, *fwd.add(1), *fwd.add(2)],
        )
    };
    let depth = (c[0] - e[0]) * f[0] + (c[1] - e[1]) * f[1] + (c[2] - e[2]) * f[2] - r;
    let wall = farclip.clamp(177.0, 777.0);
    unsafe { *out = (depth <= wall) as u32 };
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

// Process-wide parse cache for doodad models, keyed by lowercase model path.
//
// Every tile carries its own props, so the first placement of each model used
// to read and parse the M2 on the game thread while the player walked into the
// tile (30-80 ms per model, a handful of models per tile). The tile prefetch
// worker fills this cache ahead of the build, so `bn_doodad_load` finds the
// parse instead of paying for it; a tile whose prefetch lost the race still
// parses on the game thread and shares the result with every later tile.

struct DoodadMeshEntry {
    subs: Arc<Vec<benilla_formats::RenderSubmesh>>,
    weight: usize,
}

struct DoodadMeshCache {
    entries: HashMap<String, DoodadMeshEntry>,
    /// Least-recently-used order (front is oldest).
    order: VecDeque<String>,
    weight: usize,
}

/// Rough bytes kept parsed (`positions.len * 4 * 12` covers position, normal
/// and UV; indices are u16). A long tour visits every prop model in the world.
const DOODAD_MESH_CACHE_BYTES: usize = 96 << 20;

fn doodad_mesh_cache() -> &'static Mutex<DoodadMeshCache> {
    static CACHE: OnceLock<Mutex<DoodadMeshCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(DoodadMeshCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            weight: 0,
        })
    })
}

fn doodad_mesh_weight(subs: &[benilla_formats::RenderSubmesh]) -> usize {
    subs.iter()
        .map(|s| s.positions.len() * 48 + s.indices.len() * 2)
        .sum()
}

/// The cached parse of `path`; touches its LRU position.
fn doodad_mesh_cached(path: &str) -> Option<Arc<Vec<benilla_formats::RenderSubmesh>>> {
    let key = chain_model_path(path);
    let mut cache = doodad_mesh_cache().lock().ok()?;
    let subs = cache.entries.get(&key)?.subs.clone();
    if let Some(pos) = cache.order.iter().position(|k| k == &key) {
        cache.order.remove(pos);
    }
    cache.order.push_back(key);
    Some(subs)
}

/// Keep a parsed model for every placement and tile that follows.
fn doodad_mesh_insert(
    path: &str,
    subs: Vec<benilla_formats::RenderSubmesh>,
) -> Arc<Vec<benilla_formats::RenderSubmesh>> {
    let weight = doodad_mesh_weight(&subs);
    let subs = Arc::new(subs);
    let key = chain_model_path(path);
    if let Ok(mut cache) = doodad_mesh_cache().lock() {
        if let Some(old) = cache.entries.insert(
            key.clone(),
            DoodadMeshEntry {
                subs: subs.clone(),
                weight,
            },
        ) {
            cache.weight = cache.weight.saturating_sub(old.weight);
        }
        if let Some(pos) = cache.order.iter().position(|k| k == &key) {
            cache.order.remove(pos);
        }
        cache.order.push_back(key);
        cache.weight += weight;
        while cache.weight > DOODAD_MESH_CACHE_BYTES && cache.order.len() > 1 {
            let Some(victim) = cache.order.pop_front() else {
                break;
            };
            if let Some(old) = cache.entries.remove(&victim) {
                cache.weight = cache.weight.saturating_sub(old.weight);
            }
        }
    }
    subs
}

/// The model path as the chain stores it: lowercase, `.mdx`/`.mdl` as `.m2`
/// (mirrors `benilla_formats::load_m2_mesh`'s normalization).
fn chain_model_path(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    match lower
        .strip_suffix(".mdx")
        .or_else(|| lower.strip_suffix(".mdl"))
    {
        Some(stem) => format!("{stem}.m2"),
        None => lower,
    }
}

/// Read `path`'s bytes through the worker's own chain (never the game thread's).
fn read_m2_bytes_chain(chain: &mut benilla_formats::Chain, raw: &str) -> Option<Vec<u8>> {
    chain.read_file(&chain_model_path(raw)).ok()
}

/// A WMO's own doodad (MODD) composed into world WoW coords: model path, world
/// position, world quaternion, uniform scale, and a synthetic straddle uid.
/// benilla spawns exactly these per placement — doodad set 0 plus the MODF
/// `doodad_set`; the
/// per-group MODR split is draw-time culling this host does not do, so every
/// prop of the two sets draws.
#[derive(Clone)]
struct WmoDoodadInst {
    model: String,
    position: [f32; 3],
    quat: [f32; 4],
    scale: f32,
    uid: u32,
    /// The MODF placement that owns this prop and its MODD index: the prop
    /// draws with the MODR groups that name it, so the cull needs both.
    wmo_uid: u32,
    di: u32,
}

/// A root's MODD/MODS tables, WMO model space, cached by root path: the
/// compose is per placement, the MPQ read and parse once per model.
struct WmoRootDoodads {
    insts: Vec<(String, [f32; 3], [f32; 4], f32)>,
    sets: Vec<(u32, u32)>,
}

fn wmo_root_doodads(
    chain: Option<&mut benilla_formats::Chain>,
    root_path: &str,
) -> Option<Arc<WmoRootDoodads>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<WmoRootDoodads>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = root_path.to_ascii_lowercase();
    if let Ok(c) = cache.lock() {
        if let Some(hit) = c.get(&key) {
            return hit.clone();
        }
    }
    let parsed = chain.and_then(|c| {
        let bytes = c.read_file(&key).ok()?;
        let root = benilla_formats::parse_wmo_root(&bytes).ok()?;
        Some(Arc::new(WmoRootDoodads {
            insts: root
                .doodads()
                .iter()
                .map(|d| (d.model.clone(), d.position, d.orientation, d.scale))
                .collect(),
            sets: root
                .doodad_sets()
                .iter()
                .map(|s| (s.start, s.count))
                .collect(),
        }))
    });
    if let Ok(mut c) = cache.lock() {
        if c.len() >= 256 {
            c.clear();
        }
        c.insert(key, parsed.clone());
    }
    parsed
}

/// fnv-1a of root path, placement uid and doodad index: stable across the
/// tiles a straddling building appears in, so the host's per-uid claim dedups
/// a building's props exactly like an MDDF uid.
fn wmo_doodad_uid(root: &str, placement_uid: u32, index: u32) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root
        .bytes()
        .chain(placement_uid.to_le_bytes())
        .chain(index.to_le_bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let v = (h ^ (h >> 32)) as u32;
    if v == 0 {
        1
    } else {
        v
    }
}

fn resolve_wmo_doodads(
    mut chain: Option<&mut benilla_formats::Chain>,
    wmos: &[benilla_formats::WmoInstance],
) -> Vec<WmoDoodadInst> {
    let mut out = Vec::new();
    for inst in wmos {
        let Some(root) = wmo_root_doodads(chain.as_deref_mut(), &inst.model) else {
            continue;
        };
        let q_place = placement_quat(inst.rotation);
        let mut ranges: Vec<(u32, u32)> = root.sets.first().copied().into_iter().collect();
        if inst.doodad_set != 0 {
            if let Some(s) = root.sets.get(inst.doodad_set as usize) {
                ranges.push(*s);
            }
        }
        for (start, count) in ranges {
            for (di, (model, pos, q_local, scale)) in root
                .insts
                .iter()
                .enumerate()
                .skip(start as usize)
                .take(count as usize)
            {
                if model.is_empty() {
                    continue; // the MODN name did not resolve
                }
                let p = q_rot(q_place, *pos);
                out.push(WmoDoodadInst {
                    model: model.clone(),
                    position: [
                        p[0] + inst.position[0],
                        p[1] + inst.position[1],
                        p[2] + inst.position[2],
                    ],
                    quat: q_mul(q_place, *q_local),
                    scale: *scale,
                    uid: wmo_doodad_uid(&inst.model, inst.unique_id, di as u32),
                    wmo_uid: inst.unique_id,
                    di: di as u32,
                });
            }
        }
    }
    out
}

/// Parse a prepared tile's prop models on the worker: the mesh (shared with
/// `bn_doodad_load`), its rig (shared with `bn_doodad_rig_info`) and the
/// collision hull the body walks against. `models` pairs the raw ADT path (the
/// hull's key) with its normalized form (the cache key).
fn prefetch_doodad_models(chain: &mut benilla_formats::Chain, models: &[(String, String)]) {
    for (raw, path) in models {
        let Some(bytes) = read_m2_bytes_chain(chain, path) else {
            continue;
        };
        let subs = match doodad_mesh_cached(path) {
            Some(s) => s,
            None => {
                match benilla_formats::parse_m2_render_submeshes(&bytes, &parent_dir_of(path), &[])
                {
                    Ok(s) => doodad_mesh_insert(path, s),
                    Err(_) => continue,
                }
            }
        };
        player::doodad_hull_park(
            raw,
            benilla_formats::parse_m2_collision_hull(&bytes)
                .ok()
                .filter(|h| !h.is_empty()),
        );
        doodad_rig::rig_ensure_from_bytes(path, &bytes, subs);
    }
}

/// The directory part of a model path (the `load_m2_mesh` skin/texture base).
fn parent_dir_of(path: &str) -> String {
    match path.rfind(['\\', '/']) {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Doodad placements in the loaded tile: the MDDF props plus the tile's
/// WMO-internal doodads (0 when no tile).
#[no_mangle]
pub extern "C" fn bn_doodad_count(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    let w = unsafe { &*world };
    w.tile.as_ref().map_or(0, |t| t.doodads.len() as u32) + w.wmo_doodads.len() as u32
}

/// A merged doodad index past the MDDF placements, or `None` in the MDDF
/// range (the caller handles those) / out of range.
fn wmo_doodad_at(w: &BnWorld, i: u32) -> Option<&WmoDoodadInst> {
    let mddf_n = w.tile.as_ref().map_or(0, |t| t.doodads.len()) as u32;
    w.wmo_doodads.get(i.checked_sub(mddf_n)? as usize)
}

/// The baked flat for doodad `i`, or `None` before `bn_doodad_load`.
fn doodad_flat<'a>(world: *const BnWorld, i: u32) -> Option<&'a WmoFlat> {
    if world.is_null() {
        return None;
    }
    // SAFETY: caller owns a valid const handle.
    unsafe { &*world }.doodad_flats.get(&i)
}

/// Bake doodad `i` (load its M2 through the official `load_m2_mesh`, apply
/// the placement, cache). The merged index space: an MDDF placement applies
/// its Euler rotation; a WMO-internal doodad applies its world-composed MODD
/// quaternion. Returns the submesh count on success (UE iterates `[0, n)`),
/// 0 on a miss or load failure.
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
    let center = w.tile.as_ref().map(|t| t.center);
    let mddf = w
        .tile
        .as_ref()
        .and_then(|t| t.doodads.get(i as usize).map(|d| (d, t.center)))
        .map(|(d, center)| (d.model.clone(), d.position, d.rotation, d.scale, center));
    let placement = match mddf {
        Some((model, pos, rot, scale, center)) => (model, pos, rot, scale, center, None),
        None => {
            let Some(d) = wmo_doodad_at(w, i) else {
                return 0;
            };
            let Some(center) = center else {
                return 0;
            };
            (
                d.model.clone(),
                d.position,
                [0.0; 3],
                d.scale,
                center,
                Some(d.quat),
            )
        }
    };
    let (model_path, pos, rot, scale, center, quat) = placement;
    let subs = match doodad_mesh_cached(&model_path) {
        Some(s) => s,
        None => {
            let Some(chain) = w.chain.as_mut() else {
                return 0;
            };
            // `.mdx` -> `.m2` normalization happens inside load_m2_mesh.
            let Ok(subs) = benilla_formats::load_m2_mesh(chain, &model_path) else {
                return 0;
            };
            doodad_mesh_insert(&model_path, subs)
        }
    };
    let flat = match quat {
        Some(q) => build_wmo_flat_q(&subs, pos, q, center, scale, &model_path, w.chain.as_ref()),
        None => build_wmo_flat(
            &subs,
            pos,
            rot,
            center,
            scale,
            &model_path,
            w.chain.as_ref(),
        ),
    };
    // The far-clip cull addresses the placement by uid after the tile drops
    // its flats, so the baked sphere rides the uid-keyed table.
    let owner = wmo_doodad_at(w, i).map(|d| (d.wmo_uid, d.di));
    if let Some(uid) = w
        .tile
        .as_ref()
        .and_then(|t| t.doodads.get(i as usize).map(|d| d.unique_id))
        .or_else(|| wmo_doodad_at(w, i).map(|d| d.uid))
    {
        if let Ok(mut m) = doodad_spheres().lock() {
            m.insert(uid, flat.sphere);
        }
        exterior::register_doodad(
            uid,
            pos,
            quat.unwrap_or_else(|| placement_quat(rot)),
            flat.aabb,
            owner,
        );
    }
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
    let Some(f) = doodad_flat(world, i) else {
        return 0;
    };
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
    let Some(f) = doodad_flat(world, i) else {
        return 0;
    };
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
pub extern "C" fn bn_doodad_submesh(world: *const BnWorld, i: u32, j: u32, out: *mut u32) -> u32 {
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
        .or_else(|| wmo_doodad_at(w, i).map(|d| d.model.clone()))
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

/// `unique_id` of doodad `i` into `out`: the MDDF uid, or a WMO doodad's
/// synthetic one (root path + placement uid + index, stable across tiles).
/// A straddling doodad appears in every overlapping tile's ADT with the same
/// uid; the client dedups by it, and so does the host.
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
        .or_else(|| wmo_doodad_at(w, i).map(|d| d.uid))
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
fn doodad_submesh(
    w: &BnWorld,
    i: u32,
    j: u32,
) -> Option<(Arc<Vec<benilla_formats::RenderSubmesh>>, usize)> {
    if !w.doodad_flats.contains_key(&i) {
        return None;
    }
    let path = match w.tile.as_ref().and_then(|t| t.doodads.get(i as usize)) {
        Some(d) => d.model.clone(),
        None => wmo_doodad_at(w, i)?.model.clone(),
    };
    let subs = doodad_mesh_cached(&path)?;
    ((j as usize) < subs.len()).then_some((subs, j as usize))
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
    let Some((subs, sm)) = doodad_submesh(w, i, j) else {
        return 0;
    };
    let sm = &subs[sm];
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
    let Some((subs, sm)) = doodad_submesh(w, i, j) else {
        return 0;
    };
    let sm = &subs[sm];
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
    let Some((subs, sm)) = doodad_submesh(w, i, j) else {
        return 0;
    };
    let sm = &subs[sm];
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
            (
                [30.0, 45.0, 60.0],
                [0.360423, -0.822363, -0.391904, 0.200562],
            ),
        ];
        for (deg, expected) in cases {
            let q = placement_quat(deg);
            let bevy = [-q[1], q[2], -q[0], q[3]]; // B·q·B⁻¹, B = wow_to_bevy
            let dot: f32 = (0..4).map(|k| bevy[k] * expected[k]).sum();
            assert!(dot.abs() > 0.9999, "placement_quat({deg:?}) = {q:?} → bevy {bevy:?}, expected ~{expected:?} (dot {dot})");
        }
    }
}

#[cfg(test)]
mod m2_batch_parity_tests {
    use super::*;
    use std::collections::HashMap;

    /// Canonical blend class from benilla-formats' `ModelBlend` + additive bit,
    /// in the FFI's numbering (3 and 4 are both additive to the host, so the
    /// two collapse there).
    fn canonical_class(blend: benilla_formats::ModelBlend, additive: bool) -> u8 {
        use benilla_formats::ModelBlend::*;
        match blend {
            Opaque => 0,
            AlphaTest => 1,
            Blend if additive => 3,
            Blend => 2,
            Mod => 5,
            Mod2x => 6,
        }
    }

    fn host_class(blend: u8) -> u8 {
        if blend == 4 {
            3
        } else {
            blend
        }
    }

    /// A vertex's identity by authored attributes; both paths copy the same
    /// `model.vertices` values, so the bit patterns match exactly.
    fn vertex_key(p: [f32; 3], n: [f32; 3], uv: [f32; 2]) -> [u32; 8] {
        [
            p[0].to_bits(),
            p[1].to_bits(),
            p[2].to_bits(),
            n[0].to_bits(),
            n[1].to_bits(),
            n[2].to_bits(),
            uv[0].to_bits(),
            uv[1].to_bits(),
        ]
    }

    /// Triangle (three vertex keys) plus blend class and two-sided flag.
    type TriKey = ([u32; 8], [u32; 8], [u32; 8], u8, u8);

    /// `m2_emit_tris` must reproduce `parse_m2_render_submeshes`, the canonical
    /// per-batch draw list: the same triangles with the same multiplicity (the
    /// glue plaza's ground draws once under its shadow decal, a creature's body
    /// once under a glow overlay), each with its batch's own blend class and
    /// two-sided flag. These models carry the cases that were wrong: overlapping
    /// batches (UI_Human, UI_MainMenu), a character body with hair/object
    /// batches, tauren fur (SkinExtra), and a mount mixing opaque and masked
    /// layers.
    #[test]
    fn emit_matches_canonical_batches() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let paths = [
            "Interface\\Glues\\Models\\UI_Human\\UI_Human.m2",
            "Interface\\Glues\\Models\\UI_MainMenu\\UI_MainMenu.m2",
            "Character\\Human\\Male\\HumanMale.m2",
            "Character\\Tauren\\Male\\TaurenMale.m2",
            "Creature\\RidingHorse\\RidingHorse.m2",
        ];
        let mut checked = 0;
        for path in paths {
            let Ok(bytes) = chain.read_file(path) else {
                continue;
            };
            let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) else {
                continue;
            };
            let model = fmt.model();
            let Ok(skin) = model.parse_embedded_skin(bytes.as_slice(), 0) else {
                continue;
            };

            let mut ours: HashMap<TriKey, i64> = HashMap::new();
            for t in m2_emit_tris(model, &skin) {
                let b = &skin.batches()[t.batch as usize];
                let flags = u8::from(
                    model
                        .materials
                        .get(b.material_index as usize)
                        .is_some_and(|m| m.flags.bits() & 0x04 != 0),
                );
                let key = |i: u32| {
                    let v = &model.vertices[i as usize];
                    vertex_key(
                        [v.position.x, v.position.y, v.position.z],
                        [v.normal.x, v.normal.y, v.normal.z],
                        [v.tex_coords.x, v.tex_coords.y],
                    )
                };
                *ours
                    .entry((
                        key(t.a),
                        key(t.b),
                        key(t.c),
                        host_class(m2_blend_class(model, b)),
                        flags,
                    ))
                    .or_default() += 1;
            }

            let subs = benilla_formats::parse_m2_render_submeshes(&bytes, "", &[])
                .unwrap_or_else(|e| panic!("{path}: canonical parse: {e}"));
            let mut theirs: HashMap<TriKey, i64> = HashMap::new();
            for s in &subs {
                let blend = canonical_class(s.blend, s.additive);
                for tri in s.indices.as_chunks::<3>().0 {
                    let key = |i: u32| {
                        let i = i as usize;
                        vertex_key(s.positions[i], s.normals[i], s.uvs[i])
                    };
                    *theirs
                        .entry((
                            key(tri[0]),
                            key(tri[1]),
                            key(tri[2]),
                            blend,
                            u8::from(s.two_sided),
                        ))
                        .or_default() += 1;
                }
            }
            assert_eq!(ours, theirs, "{path}: emit list differs from canonical");
            checked += 1;
        }
        assert_eq!(
            checked,
            paths.len(),
            "a parity model is missing from the install"
        );
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
    let mut s = [0u8; 4];
    s.copy_from_slice(b.get(o..o + 4)?);

    Some(u32::from_le_bytes(s))
}

/// Read a u16 at byte offset (0 on OOB).

fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    let mut s = [0u8; 2];
    s.copy_from_slice(b.get(o..o + 2)?);

    Some(u16::from_le_bytes(s))
}

/// Read an i16 at byte offset.

fn rd_i16(b: &[u8], o: usize) -> Option<i16> {
    let mut s = [0u8; 2];
    s.copy_from_slice(b.get(o..o + 2)?);

    Some(i16::from_le_bytes(s))
}

/// Read an f32 at byte offset (0 on OOB).

fn rd_f32(b: &[u8], o: usize) -> Option<f32> {
    let mut s = [0u8; 4];
    s.copy_from_slice(b.get(o..o + 4)?);

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

/// `model.bones.len` (in WoW Y-up coords, same space as `bone.pivot`).

pub fn m2_bone_rest_positions(bytes: &[u8]) -> Option<Vec<[f32; 3]>> {
    let (count, ofs) = m2_bones_array(bytes)?;

    let capped = std::cmp::min(
        count as usize,
        bytes.len().saturating_sub(ofs as usize) / 108,
    );

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

/// One M2 triangle to draw, with the skin batch it belongs to.
pub(crate) struct M2EmitTri {
    pub a: u32,
    pub b: u32,
    pub c: u32,
    /// Index into [`benilla_m2::Skin::batches`], for texture/blend/two-sided.
    pub batch: u16,
}

/// The M2's static draw list, in batch file order: one entry per triangle a
/// batch actually draws. Canonical `parse_m2_render_submeshes` makes one submesh
/// per batch, so a section covered by several batches (the glue plaza's ground
/// plus its shadow decal, a creature's body plus a glow overlay) is drawn once
/// per batch, and a triangle no batch covers is never drawn. Constant-invisible
/// batches drop here too, as the client's static cull does.
pub(crate) fn m2_emit_tris(model: &benilla_m2::M2Model, skin: &benilla_m2::Skin) -> Vec<M2EmitTri> {
    let lookup = skin.indices();
    let tris = skin.triangles();
    let sections = skin.submeshes();
    let mut out = Vec::new();
    for (bi, b) in skin.batches().iter().enumerate() {
        let Some(sec) = sections.get(b.skin_section_index as usize) else {
            continue;
        };
        // `triangle_start`/`triangle_count` are flat index slots, not triangles.
        let start = sec.triangle_start as usize;
        let end = start
            .saturating_add(sec.triangle_count as usize)
            .min(tris.len());
        if start >= end || m2_batch_static_alpha(model, b) == Some(0.0) {
            continue;
        }
        for t in tris[start..end].as_chunks::<3>().0 {
            let (Some(&a), Some(&b2), Some(&c)) = (
                lookup.get(t[0] as usize),
                lookup.get(t[1] as usize),
                lookup.get(t[2] as usize),
            ) else {
                continue;
            };
            let nv = model.vertices.len();
            if a as usize >= nv || b2 as usize >= nv || c as usize >= nv {
                continue;
            }
            out.push(M2EmitTri {
                a: u32::from(a),
                b: u32::from(b2),
                c: u32::from(c),
                batch: bi as u16,
            });
        }
    }
    out
}

/// A batch's static alpha factor `colorAlpha · transparencyWeight`. `Some(0.0)`
/// when constant tracks hide it; `None` when a track varies, leaving the
/// decision to the frame the client samples.
fn m2_batch_static_alpha(model: &benilla_m2::M2Model, b: &benilla_m2::SkinBatch) -> Option<f32> {
    let scalar = |t: Option<&benilla_m2::M2ScalarTrack>| match t {
        Some(t) if t.keys.is_empty() => Some(1.0),
        Some(t) => t.constant(),
        None => Some(1.0),
    };
    let color = scalar(model.color_alpha_tracks.get(b.color_index as usize));
    let weight = if b.texture_count != 0 {
        scalar(
            model
                .transparency_lookup
                .get(b.weight_combo_index as usize)
                .and_then(|&t| model.transparency_tracks.get(t as usize)),
        )
    } else {
        Some(1.0)
    };
    match (color, weight) {
        (Some(c), Some(w)) => Some(c * w),
        _ => None,
    }
}

/// The UE material class for a batch's M2 blend mode, through the same remap as
/// benilla-formats `m2_batches`: 0 opaque, 1 alpha-key, 2 alpha, 3/4 additive
/// (NoAlphaAdd/Add), 5 Mod, 6 Mod2x.
pub(crate) fn m2_blend_class(model: &benilla_m2::M2Model, b: &benilla_m2::SkinBatch) -> u8 {
    match model
        .materials
        .get(b.material_index as usize)
        .map(|m| m.blend_mode.bits())
    {
        Some(0) | None => 0,
        Some(1) => 1,
        Some(3) => 3,
        Some(4) => 4,
        Some(5) => 5,
        Some(6) => 6,
        Some(_) => 2,
    }
}

// ---- Parsed M2 cache ----
//
// One spawn drives six to eight model entry points (counts, fill, character
// slots, texture names, animation lookup) and every one used to re-read the
// MD20 from the MPQ: the read alone measures ~20 ms for a character model, so
// a single NPC cost 100-300 ms on the game thread. The bytes, the parsed model
// and the embedded skin are path-keyed and read once per install.

struct M2Cached {
    bytes: Vec<u8>,
    format: benilla_m2::M2Format,
    /// Embedded skin profile 0; `None` for a skin-less model (the dense-triple
    /// fallback applies).
    skin: Option<benilla_m2::Skin>,
}

/// Bound on the raw M2 bytes kept parsed: a long tour visits every creature
/// model in the world, and character M2s run 2-4 MB each.
const M2_CACHE_BYTES: usize = 128 << 20;

struct M2Cache {
    entries: HashMap<String, Arc<M2Cached>>,
    /// Least-recently-used order (front is oldest).
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

fn m2_cache() -> &'static Mutex<M2Cache> {
    static CACHE: OnceLock<Mutex<M2Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(M2Cache {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
        })
    })
}

/// The parsed `path` from the chain, reading it only once per install.
fn m2_cached(archives: &[Archive], path: &str) -> Option<Arc<M2Cached>> {
    let key = path.to_ascii_lowercase();
    if let Ok(mut cache) = m2_cache().lock() {
        if let Some(e) = cache.entries.get(&key).cloned() {
            if let Some(pos) = cache.order.iter().position(|k| k == &key) {
                cache.order.remove(pos);
                cache.order.push_back(key);
            }
            return Some(e);
        }
    }

    let mut bytes = None;
    for a in archives.iter().rev() {
        if !a.contains(path) {
            continue;
        }
        if let Ok(b) = a.read_file(path) {
            bytes = Some(b);
        }
        break;
    }
    let bytes = bytes?;
    let format = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())).ok()?;
    let skin = format.model().parse_embedded_skin(bytes.as_slice(), 0).ok();
    let entry = Arc::new(M2Cached {
        bytes,
        format,
        skin,
    });

    if let Ok(mut cache) = m2_cache().lock() {
        if let Some(old) = cache.entries.insert(key.clone(), entry.clone()) {
            cache.bytes = cache.bytes.saturating_sub(old.bytes.len());
        }
        cache.order.push_back(key);
        cache.bytes += entry.bytes.len();
        while cache.bytes > M2_CACHE_BYTES && cache.order.len() > 1 {
            let Some(victim) = cache.order.pop_front() else {
                break;
            };
            if let Some(old) = cache.entries.remove(&victim) {
                cache.bytes = cache.bytes.saturating_sub(old.bytes.len());
            }
        }
    }
    Some(entry)
}

/// One M2 vertex: `position` (C3, @0), weights (@0x0c), 4 bone indices

/// (@0x10), normal (@0x14), UV (@0x20). Rest pose applies the dominant

/// bone's pivot, then the weighted skin blend:

///

/// world_pos = Σ (bone_world_matrix[k] * (position - pivot[k] + pivot[k].world)) * (w_k / 255)

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
    if world.is_null() || model_path.is_null() || out_vertices.is_null() || out_triangles.is_null()
    {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };
    let model = cached.format.model();

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

    // skin.indices = indexLookup (vertex remap table)

    // skin.triangles = triangle index array (indices into indexLookup)

    let ntri = match &cached.skin {
        Some(skin) => {
            let batches = skin.batches();
            let mut ti = 0usize;
            for t in m2_emit_tris(model, skin) {
                if ti >= cap as usize {
                    break;
                }
                let b = &batches[t.batch as usize];
                let flags = u8::from(
                    model
                        .materials
                        .get(b.material_index as usize)
                        .is_some_and(|m| m.flags.bits() & 0x04 != 0),
                );
                // SAFETY: out_triangles has room for cap BnTriangle.
                unsafe {
                    *out_triangles.add(ti) = BnTriangle {
                        a: t.a,
                        b: t.b,
                        c: t.c,
                        material: m2_blend_class(model, b),
                        flags,
                    };
                }
                ti += 1;
            }
            ti
        }
        None => {
            // No embedded skin: dense sequential triples, so the mesh
            // is always closed (the old fallback).
            let mut ti = 0usize;
            while ti * 3 + 2 < nv && ti < cap as usize {
                let n = ti * 3;
                // SAFETY: out_triangles has room for cap BnTriangle.
                unsafe {
                    *out_triangles.add(ti) = BnTriangle {
                        a: n as u32,
                        b: (n + 1) as u32,
                        c: (n + 2) as u32,
                        material: 0,
                        flags: 0,
                    };
                }
                ti += 1;
            }
            ti
        }
    };

    ntri as u32
}

/// Per-vertex tint of an M2, in RGBA8, indexed like `bn_model_fill`'s vertices.
///
/// The reference draws a batch as `texel · M2Color · opacity`
/// (`0x70c507`; benilla's `model_render` carries the tint in the vertex
/// colour and the alpha in the fragment's). The UE material graphs multiply
/// the texel by the vertex colour (`M_M2*` in `gen_bn_materials.py`), so a
/// model served without one draws its additive shells at full strength - the
/// quest marker's WHITE8X8 glow read as a solid white `!`.
///
/// rgb is the batch's `M2Color` first key (white when the batch names no
/// colour, and dropped on Mod/Mod2x, which zero the tint term as benilla's
/// `m2_batches` does). a is `colorAlpha · transparencyWeight`
/// ([`m2_batch_static_alpha`]); a varying track is left at 1.0 here, the
/// static case the seated marker and every untinted model need.
///
/// Returns the vertices written.
#[no_mangle]
pub extern "C" fn bn_model_vertex_tint(
    world: *const BnWorld,
    model_path: *const c_char,
    out: *mut u8,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    // SAFETY: valid handle.
    let w = unsafe { &*world };
    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };
    let model = cached.format.model();
    let nv = model.vertices.len().min(cap as usize);
    if nv == 0 {
        return 0;
    }
    // Untinted: the texel times 1.
    for i in 0..nv {
        // SAFETY: out has room for cap RGBA8 entries.
        unsafe { std::ptr::write_bytes(out.add(i * 4), 0xFF, 4) };
    }
    let Some(skin) = cached.skin.as_ref() else {
        return nv as u32;
    };
    let batches = skin.batches();
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut assigned = vec![false; nv];
    for t in m2_emit_tris(model, skin) {
        let Some(b) = batches.get(t.batch as usize) else {
            continue;
        };
        let blend = m2_blend_class(model, b);
        let rgb = if matches!(blend, 5 | 6) {
            [1.0f32; 3]
        } else {
            model
                .color_rgb_tracks
                .get(b.color_index as usize)
                .and_then(|tr| tr.keys.first())
                .map_or([1.0f32; 3], |&(_, c)| c)
        };
        let a = m2_batch_static_alpha(model, b).unwrap_or(1.0);
        for v in [t.a, t.b, t.c] {
            let vi = v as usize;
            if vi >= nv || assigned[vi] {
                continue;
            }
            assigned[vi] = true;
            // SAFETY: vi < nv <= cap.
            unsafe {
                *out.add(vi * 4) = byte(rgb[0]);
                *out.add(vi * 4 + 1) = byte(rgb[1]);
                *out.add(vi * 4 + 2) = byte(rgb[2]);
                *out.add(vi * 4 + 3) = byte(a);
            }
        }
    }
    nv as u32
}

/// Count of M2 vertices. The UE renderer sizes its vertex buffer from this

/// before calling `bn_model_fill`. Returns 0 on a non-M2 miss.

#[no_mangle]

pub extern "C" fn bn_model_vertex_count(world: *const BnWorld, model_path: *const c_char) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    m2_cached(&w.archives, p).map_or(0, |c| c.format.model().vertices.len() as u32)
}

/// Count of M2 triangles. The UE renderer sizes its index buffer from this.

/// Returns 0 on a non-M2 miss.

#[no_mangle]

pub extern "C" fn bn_model_triangle_count(world: *const BnWorld, model_path: *const c_char) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };
    let model = cached.format.model();

    // Match bn_model_fill exactly: one triangle per (batch, section
    // triangle) pair, so a section covered by several batches counts
    // once per batch.
    if let Some(skin) = &cached.skin {
        return m2_emit_tris(model, skin).len() as u32;
    }

    // No embedded skin: fall back to a dense sequential triangle
    // strip over the vertex list (same fallback bn_model_fill
    // uses), which is floor(verts/3).
    (model.vertices.len() / 3) as u32
}

/// Count of embedded M2 textures (the `textures` array, one BLP each).

/// `0` for a non-M2 miss. The UE side iterates `[0, n)` and pulls each via

/// `bn_model_texture_name` + `bn_texture`.

#[no_mangle]

pub extern "C" fn bn_model_texture_count(world: *const BnWorld, model_path: *const c_char) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    m2_texture_count(cached.bytes.as_slice())
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let bytes = &cached.bytes;

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

    let (name_len, name_ofs) = match (
        rd_u32(bytes.as_slice(), rec + 8),
        rd_u32(bytes.as_slice(), rec + 12),
    ) {
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

    (n + 1) as u32
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    m2_attachment_count(cached.bytes.as_slice())
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
    if world.is_null()
        || model_path.is_null()
        || out_id.is_null()
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let bytes = &cached.bytes;

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

    1
}

/// The model-local matrix of attachment `attach_id` at a pose: 16 floats,
/// row-major, the three axes then the translation, in UE axes.
///
/// The reference hangs a held item off its hand attachment (`0x47a0c0`): the
/// item's root is the attachment's bone matrix carried to the attachment's
/// authored point. A bone's global matrix maps a model-space rest point to its
/// animated place (bones rotate about their pivots, so the rest global is the
/// identity), which makes the transform simply `bone_global · T(attach_pos)`.
/// `weight > 0` with `over_anim != u32::MAX` composes the masked two-clip rig
/// the way [`bn_model_skinned_positions_masked`] does, so a weapon follows the
/// torso layer its hand rides.
#[no_mangle]

pub extern "C" fn bn_model_attach_pose(
    world: *const BnWorld,

    model_path: *const c_char,

    base_anim: u32,

    base_ms: u32,

    over_anim: u32,

    over_ms: u32,

    weight: f32,

    attach_id: u32,

    out: *mut f32,
) -> u32 {
    attach_pose_impl(
        world,
        model_path,
        (base_anim, base_ms),
        (over_anim, over_ms),
        weight,
        attach_id,
        (0.0, 0.0),
        out,
    )
}

/// [`bn_model_attach_pose`] with the strafe counter-twist applied, so a held
/// item's hand follows the twisted torso.
#[no_mangle]

pub extern "C" fn bn_model_attach_pose_twist(
    world: *const BnWorld,

    model_path: *const c_char,

    base_anim: u32,

    base_ms: u32,

    over_anim: u32,

    over_ms: u32,

    weight: f32,

    attach_id: u32,

    twist_spine: f32,

    twist_head: f32,

    out: *mut f32,
) -> u32 {
    attach_pose_impl(
        world,
        model_path,
        (base_anim, base_ms),
        (over_anim, over_ms),
        weight,
        attach_id,
        (twist_spine, twist_head),
        out,
    )
}

fn attach_pose_impl(
    world: *const BnWorld,

    model_path: *const c_char,

    base: (u32, u32),

    over: (u32, u32),

    weight: f32,

    attach_id: u32,

    twist: (f32, f32),

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

    let Some(rig) = m2_rig_cached(&w.archives, p) else {
        return 0;
    };

    let bytes = rig.cached.bytes.as_slice();

    let Some((count, ofs)) = m2_attachment_array(bytes) else {
        return 0;
    };

    let mut found = None;

    for i in 0..count {
        let rec = ofs as usize + i as usize * 48;

        let id = rd_u32(bytes, rec).unwrap_or(0);

        if id == attach_id {
            found = Some((
                rd_u32(bytes, rec + 4).unwrap_or(0) as usize,
                rd_f32(bytes, rec + 8).unwrap_or(0.0),
                rd_f32(bytes, rec + 12).unwrap_or(0.0),
                rd_f32(bytes, rec + 16).unwrap_or(0.0),
            ));

            break;
        }
    }

    let Some((bone, px, py, pz)) = found else {
        return 0;
    };

    let mut mats = if weight > 0.0 && over.0 != u32::MAX {
        let upper = upper_subtree(&rig.bones, 4);

        masked_bone_matrices(
            &rig.bones,
            &rig.global_seqs,
            (base.0 as usize, base.1, base.1),
            (over.0 as usize, over.1, over.1),
            weight,
            &upper,
        )
    } else {
        (0..rig.bones.len())
            .map(|i| {
                bone_matrix_at(
                    &rig.bones,
                    base.0 as usize,
                    base.1,
                    base.1,
                    &rig.global_seqs,
                    i,
                )
            })
            .collect()
    };

    twist_pose(&rig.bones, &mut mats, twist.0, twist.1);

    let Some(m) = mats.get(bone) else {
        return 0;
    };

    // fixCoordSystem on the authored point, as every export does.
    let (ax, ay, az) = wow_vec_to_ue(px, py, pz);

    let tx = m[0] * ax + m[1] * ay + m[2] * az + m[3];

    let ty = m[4] * ax + m[5] * ay + m[6] * az + m[7];

    let tz = m[8] * ax + m[9] * ay + m[10] * az + m[11];

    // SAFETY: the caller passes room for 16 floats.
    unsafe {
        let o = std::slice::from_raw_parts_mut(out, 16);

        o[0] = m[0];
        o[1] = m[1];
        o[2] = m[2];
        o[3] = tx;

        o[4] = m[4];
        o[5] = m[5];
        o[6] = m[6];
        o[7] = ty;

        o[8] = m[8];
        o[9] = m[9];
        o[10] = m[10];
        o[11] = tz;

        o[12] = 0.0;
        o[13] = 0.0;
        o[14] = 0.0;
        o[15] = 1.0;
    }

    1
}

/// Number of skeleton bones in the model.

#[no_mangle]

pub extern "C" fn bn_model_bone_count(world: *const BnWorld, model_path: *const c_char) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    m2_bones_array(cached.bytes.as_slice())
        .map(|(c, _)| c)
        .unwrap_or(0)
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let Some(poses) = m2_bone_rest_positions(cached.bytes.as_slice()) else {
        return 0;
    };

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

    0
}

// ---- M2 skeletal animation (Phase 3) ----

/// A row-major 4x4 matrix, 16 floats.

pub(crate) type BnMat4 = [f32; 16];

/// A per-sequence / gseq M2Vec3 (translation or scale) track.

#[derive(Clone)]

struct F32Track {
    interp: u16,

    seq: i16, // -1 = per-sequence timeline, >=0 = gseq index

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

        F32Track {
            interp,
            seq,
            ranges,
            times,
            keys,
        }
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
                (self.ranges[anim].0 as usize, self.ranges[anim].1 as usize)
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

        // time %= times[times.size-1]; // wrap on the FULL absolute axis

        // then linear-scan [lo, hi) for the first bracket key.

        // `time_ms` arrives as the clip's absolute timeline time (timeStart + offset).

        let total_axis = self.times.last().copied().unwrap_or(0);

        let time_abs = if total_axis > 0 {
            time_ms % total_axis
        } else {
            time_ms
        };

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

        QuatTrack {
            interp,
            seq,
            ranges,
            times,
            keys,
        }
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

        // time %= times[times.size-1]; // wrap on the FULL absolute axis

        // then linear-scan [lo, hi) for the first bracket key.

        // `time_ms` arrives as the clip's absolute timeline time (timeStart + offset).

        let total_axis = self.times.last().copied().unwrap_or(0);

        let time_abs = if total_axis > 0 {
            time_ms % total_axis
        } else {
            time_ms
        };

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
    let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];

    let dot2 = dot.abs();

    if dot2 > 0.9995 {
        let x = a[0] * (1.0 - r) + b[0] * r;

        let y = a[1] * (1.0 - r) + b[1] * r;

        let z = a[2] * (1.0 - r) + b[2] * r;

        let w = a[3] * (1.0 - r) + b[3] * r;

        let m = (x * x + y * y + z * z + w * w).sqrt();

        if m < 1e-8 {
            return [0.0, 0.0, 0.0, 1.0];
        }

        return [x / m, y / m, z / m, w / m];
    }

    let theta = dot.acos();

    let sin_theta = theta.sin();

    if sin_theta < 1e-8 {
        return a;
    }

    let sa = ((1.0 - r) * theta).sin() / sin_theta;

    let sb = (r * theta).sin() / sin_theta;

    let x = a[0] * sa + b[0] * sb;

    let y = a[1] * sa + b[1] * sb;

    let z = a[2] * sa + b[2] * sb;

    let w = a[3] * sa + b[3] * sb;

    let m = (x * x + y * y + z * z + w * w).sqrt();

    if m < 1e-8 {
        return [0.0, 0.0, 0.0, 1.0];
    }

    [x / m, y / m, z / m, w / m]
}

fn quat_to_mat4(q: [f32; 4]) -> BnMat4 {
    // Reference newQuatRotate (matrix.h L102-110), ROW-MAJOR m[row][col].

    let x = q[0];
    let y = q[1];
    let z = q[2];
    let w = q[3];

    let xx = x * x;
    let yy = y * y;
    let zz = z * z;

    let xy = x * y;
    let xz = x * z;
    let yz = y * z;

    let wx = w * x;
    let wy = w * y;
    let wz = w * z;

    [
        1.0 - 2.0 * (yy + zz),
        2.0 * (xy + wz),
        2.0 * (xz - wy),
        0.0,
        2.0 * (xy - wz),
        1.0 - 2.0 * (xx + zz),
        2.0 * (yz + wx),
        0.0,
        2.0 * (xz + wy),
        2.0 * (yz - wx),
        1.0 - 2.0 * (xx + yy),
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
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

pub(crate) struct BnBone {
    /// `keyBoneId` (record +0): 4 is SpineLow, the root of the upper-body
    /// subtree the vanilla one-shots mask onto.
    key_bone: u16,

    parent: i16,

    pivot: [f32; 3],

    trans: F32Track,

    rot: QuatTrack,

    scale: F32Track,
}

/// Parse all M2 bones from raw M2 bytes.

pub(crate) fn parse_m2_bones(bytes: &[u8]) -> Option<Vec<BnBone>> {
    let (count, ofs) = m2_bones_array(bytes)?;

    let anim_count = m2_anim_array(bytes).map(|(c, _)| c).unwrap_or(0);

    let avail = bytes.len().saturating_sub(ofs as usize);

    let capped = std::cmp::min(count as usize, avail / 108);

    let mut out = Vec::with_capacity(capped);

    for i in 0..capped {
        let rec = ofs as usize + i * 108;

        let parent = rd_u16(bytes, rec + 8)? as i16;

        let key_bone = rd_u32(bytes, rec).unwrap_or(0) as u16;

        let mut px = rd_f32(bytes, rec + 96)?;

        let mut py = rd_f32(bytes, rec + 100)?;

        let mut pz = rd_f32(bytes, rec + 104)?;

        if px.is_nan() {
            px = 0.0;
        }

        if py.is_nan() {
            py = 0.0;
        }

        if pz.is_nan() {
            pz = 0.0;
        }

        // fixCoordSystem: (x, -y, z)

        let pivot = [px, -py, pz];

        let trans = F32Track::parse(bytes, rec + 12, anim_count);

        let rot = QuatTrack::parse(bytes, rec + 40);

        let scale = F32Track::parse(bytes, rec + 68, anim_count);

        out.push(BnBone {
            key_bone,
            parent,
            pivot,
            trans,
            rot,
            scale,
        });
    }

    Some(out)
}

/// The bone's parent-relative matrix at (anim, time_ms), the client's
/// Bone::calcMatrix without the hierarchy walk:

/// m = T(pivot) * T(trans) * R(rot) * S(scale) * T(-pivot)

/// All post-multiplies in row-major; the pivot sandwich makes a rotated bone

/// rotate about its pivot (not the model origin). Pivots are pre-flipped to

/// UE axes (x, -y, z) at parse time; trans/quat are flipped here to match.

pub(crate) fn bone_local_matrix(
    bones: &[BnBone],

    anim: usize,

    time_ms: u32,

    global_time_ms: u32,

    global_seqs: &[u32],

    i: usize,
) -> BnMat4 {
    let b = &bones[i];

    let mut m: BnMat4 = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
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
            s[0], 0.0, 0.0, 0.0, 0.0, s[1], 0.0, 0.0, 0.0, 0.0, s[2], 0.0, 0.0, 0.0, 0.0, 1.0,
        ];

        m = mat4_mul(&m, &sm);
    }

    // * T(-pivot) post-multiply, matching model.cpp:

    {
        let px = b.pivot[0];
        let py = b.pivot[1];
        let pz = b.pivot[2];

        let tneg: BnMat4 = [
            1.0, 0.0, 0.0, -px, 0.0, 1.0, 0.0, -py, 0.0, 0.0, 1.0, -pz, 0.0, 0.0, 0.0, 1.0,
        ];

        m = mat4_mul(&m, &tneg);
    }

    m
}

/// Compute the model-space 4x4 matrix for bone `i` at (anim_index, time_ms):
/// the local matrix composed up the parent chain.
pub(crate) fn bone_matrix_at(
    bones: &[BnBone],

    anim: usize,

    time_ms: u32,

    global_time_ms: u32,

    global_seqs: &[u32],

    i: usize,
) -> BnMat4 {
    let mut m = bone_local_matrix(bones, anim, time_ms, global_time_ms, global_seqs, i);

    let parent = bones[i].parent;

    if parent >= 0 && (parent as usize) < bones.len() {
        let pmat = bone_matrix_at(
            bones,
            anim,
            time_ms,
            global_time_ms,
            global_seqs,
            parent as usize,
        );

        m = mat4_mul(&pmat, &m);
    }

    m
}

/// The bones the upper-body mask covers: the bone whose `keyBoneId` is `key`
/// (4, SpineLow) — or 6 (Head) for a model without a 4 — and every descendant.
/// A one-shot posed on these alone is the vanilla `SpineLow` layer.
pub(crate) fn upper_subtree(bones: &[BnBone], key: u16) -> Vec<bool> {
    let mut mark = vec![false; bones.len()];

    let Some(root) = bones
        .iter()
        .position(|b| b.key_bone == key)
        .or_else(|| bones.iter().position(|b| b.key_bone == 6))
    else {
        return mark;
    };

    mark[root] = true;

    let mut changed = true;

    while changed {
        changed = false;

        for i in 0..bones.len() {
            let p = bones[i].parent;

            if p >= 0 && (p as usize) < bones.len() && mark[p as usize] && !mark[i] {
                mark[i] = true;

                changed = true;
            }
        }
    }

    mark
}

/// `root` and every bone under it, by parent chain. No key-bone fallback: the
/// twist channels arm on their own bone alone (the reference's `[+0xd58]` bits).
fn subtree_of(bones: &[BnBone], root: usize) -> Vec<bool> {
    let mut mark = vec![false; bones.len()];

    if root >= bones.len() {
        return mark;
    }

    mark[root] = true;

    let mut changed = true;

    while changed {
        changed = false;

        for i in 0..bones.len() {
            let p = bones[i].parent;

            if p >= 0 && (p as usize) < bones.len() && mark[p as usize] && !mark[i] {
                mark[i] = true;

                changed = true;
            }
        }
    }

    mark
}

/// The strafe counter-twist on a posed rig: SpineLow (key bone 4) turns its
/// subtree back toward the aim by `spine`, Head (key bone 6) by `head`, each
/// about the vertical through its own pivot — the reference's `0x711f10` bone
/// channels, armed by `0x607ed0`'s tail. The head runs after the spine, on the
/// already-twisted matrix, so it turns relative to the spine as the client
/// composes the residual gap. Angles are WoW yaws; the UE mirrors take the sign
/// flip every model export takes. A model lacking a key bone skips that share.
fn twist_pose(bones: &[BnBone], mats: &mut [BnMat4], spine: f32, head: f32) {
    for (key, yaw) in [(4u16, spine), (6u16, head)] {
        if yaw.abs() < 1.0e-6 {
            continue;
        }

        let Some(root) = bones.iter().position(|b| b.key_bone == key) else {
            continue;
        };

        let Some(m) = mats.get(root) else { continue };

        let pivot = [m[3], m[7], m[11]];

        let sub = subtree_of(bones, root);

        let (s, c) = (-yaw).sin_cos();

        for (i, &on) in sub.iter().enumerate() {
            if !on {
                continue;
            }

            let Some(m) = mats.get_mut(i) else { continue };

            // A yaw about world up: the three basis columns turn, and the bone's
            // offset turns about the pivot (`mats` are affine, row-major).
            for col in 0..3 {
                let (x, y) = (m[col], m[4 + col]);

                m[col] = c * x - s * y;

                m[4 + col] = s * x + c * y;
            }

            let (x, y) = (m[3] - pivot[0], m[7] - pivot[1]);

            m[3] = c * x - s * y + pivot[0];

            m[7] = s * x + c * y + pivot[1];
        }
    }
}

#[cfg(test)]
mod twist_tests {
    use super::*;

    fn bone(key: u16, parent: i16) -> BnBone {
        BnBone {
            key_bone: key,
            parent,
            pivot: [0.0; 3],
            trans: F32Track {
                interp: 0,
                seq: -1,
                ranges: Vec::new(),
                times: Vec::new(),
                keys: Vec::new(),
            },
            rot: QuatTrack {
                interp: 0,
                seq: -1,
                ranges: Vec::new(),
                times: Vec::new(),
                keys: Vec::new(),
            },
            scale: F32Track {
                interp: 0,
                seq: -1,
                ranges: Vec::new(),
                times: Vec::new(),
                keys: Vec::new(),
            },
        }
    }

    fn placed(at: [f32; 3]) -> BnMat4 {
        [
            1.0, 0.0, 0.0, at[0], 0.0, 1.0, 0.0, at[1], 0.0, 0.0, 1.0, at[2], 0.0, 0.0, 0.0, 1.0,
        ]
    }

    /// Root, SpineLow at z 1, Head above and to the side of it, a leg off the
    /// root. The spine's yaw turns the head and nothing below the hips; the
    /// head's rides on top and turns about its own, already-moved pivot.
    fn rig() -> (Vec<BnBone>, Vec<BnMat4>) {
        let bones = vec![bone(0, -1), bone(4, 0), bone(6, 1), bone(1, 0), bone(0, 2)];
        let mats = vec![
            identity(),
            placed([0.0, 0.0, 1.0]),
            placed([1.0, 0.0, 2.0]),
            placed([1.0, 0.0, 0.0]),
            placed([2.0, 0.0, 2.0]),
        ];
        (bones, mats)
    }

    fn identity() -> BnMat4 {
        placed([0.0; 3])
    }

    #[test]
    fn a_spine_twist_turns_its_subtree_and_leaves_the_legs() {
        use std::f32::consts::FRAC_PI_2;
        let (bones, mut mats) = rig();
        twist_pose(&bones, &mut mats, FRAC_PI_2, 0.0);
        let leg = mats[3];
        assert!((leg[3] - 1.0).abs() < 1.0e-6, "the leg never moves");
        assert!((mats[1][3]).abs() < 1.0e-6, "the pivot itself holds");
        // WoW +90° yaw on (1,0) lands on (0,1); UE's Y mirror makes it (0,-1).
        assert!((mats[2][3]).abs() < 1.0e-6, "head x: {}", mats[2][3]);
        assert!((mats[2][7] + 1.0).abs() < 1.0e-6, "head y: {}", mats[2][7]);
        assert!((mats[2][11] - 2.0).abs() < 1.0e-6, "head keeps its height");
    }

    #[test]
    fn the_head_turns_on_the_twisted_spine() {
        use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};
        let (bones, mut mats) = rig();
        twist_pose(&bones, &mut mats, FRAC_PI_2, FRAC_PI_4);
        // The spine's 90° takes the head's (1,0) offset to UE (0,-1), and the
        // helm — a child of the head at (2,0) — to (0,-2). The head's own
        // residual 45° turns about the head's moved pivot, so the head bone
        // stays put and the helm lands 45° on from it.
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((mats[2][3]).abs() < 1.0e-5, "head x: {}", mats[2][3]);
        assert!((mats[2][7] + 1.0).abs() < 1.0e-5, "head y: {}", mats[2][7]);
        assert!((mats[2][11] - 2.0).abs() < 1.0e-6, "head keeps its height");
        assert!((mats[4][3] + h).abs() < 1.0e-5, "helm x: {}", mats[4][3]);
        assert!(
            (mats[4][7] + 1.0 + h).abs() < 1.0e-5,
            "helm y: {}",
            mats[4][7]
        );
    }
}

/// Compose one global matrix per bone from local matrices, memoised per bone.
fn compose_globals(
    bones: &[BnBone],
    locals: &[BnMat4],
    i: usize,
    memo: &mut [Option<BnMat4>],
) -> BnMat4 {
    if let Some(m) = memo[i] {
        return m;
    }

    let parent = bones[i].parent;

    let m = if parent >= 0 && (parent as usize) < bones.len() {
        let pmat = compose_globals(bones, locals, parent as usize, memo);

        mat4_mul(&pmat, &locals[i])
    } else {
        locals[i]
    };

    memo[i] = Some(m);

    m
}

/// The masked rig: two clips' local matrices, the upper subtree taken from
/// `over` and every other bone from `base`, then each global matrix eased from
/// the base pose to that pose by `weight`.
pub(crate) fn masked_bone_matrices(
    bones: &[BnBone],

    global_seqs: &[u32],

    base: (usize, u32, u32),

    over: (usize, u32, u32),

    weight: f32,

    upper: &[bool],
) -> Vec<BnMat4> {
    let n = bones.len();

    let base_locals: Vec<BnMat4> = (0..n)
        .map(|i| bone_local_matrix(bones, base.0, base.1, base.2, global_seqs, i))
        .collect();

    let over_locals: Vec<BnMat4> = (0..n)
        .map(|i| bone_local_matrix(bones, over.0, over.1, over.2, global_seqs, i))
        .collect();

    let mut base_memo = vec![None; n];

    let mut base_globals = Vec::with_capacity(n);

    for i in 0..n {
        base_globals.push(compose_globals(bones, &base_locals, i, &mut base_memo));
    }

    let mut over_used = base_locals.clone();

    for i in 0..n {
        if upper.get(i).copied().unwrap_or(false) {
            over_used[i] = over_locals[i];
        }
    }

    let mut over_memo = vec![None; n];

    let w = weight.clamp(0.0, 1.0);

    (0..n)
        .map(|i| {
            // Outside the subtree the two poses are identical, so the easing is
            // a no-op there and exact inside it.
            let over = compose_globals(bones, &over_used, i, &mut over_memo);

            let mut m = base_globals[i];

            if w > 0.0 {
                for k in 0..16 {
                    m[k] += (over[k] - base_globals[i][k]) * w;
                }
            }

            m
        })
        .collect()
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

    Some(
        (0..bones.len())
            .map(|i| bone_matrix_at(&bones, anim, time_ms, global_time_ms, global_seqs, i))
            .collect(),
    )
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

pub fn m2_anim_time_range(bytes: &[u8], idx: u32) -> Option<(u32, u32, u32)> {
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

pub fn m2_skinned_positions(bytes: &[u8], anim: usize, time_ms: u32) -> Option<Vec<[f32; 3]>> {
    let fmt = benilla_m2::parse_m2(&mut std::io::Cursor::new(bytes)).ok()?;

    let global_seqs = parse_m2_global_sequences(bytes);

    let bones = parse_m2_bones(bytes)?;

    Some(m2_skinned_positions_rig(
        fmt.model(),
        &global_seqs,
        &bones,
        anim,
        time_ms,
        (0.0, 0.0),
    ))
}

/// Skinning core shared by the byte path and the cached-rig path: bone

/// matrices at one (anim, time) plus the 4-weight vertex blend.

pub fn m2_skinned_positions_rig(
    model: &benilla_m2::M2Model,

    global_seqs: &[u32],

    bones: &[BnBone],

    anim: usize,

    time_ms: u32,

    twist: (f32, f32),
) -> Vec<[f32; 3]> {
    // global_time_ms: for gseq tracks, use global clock.

    // The client's animate passes globalTime directly;

    // for gseq tracks the time is wrapped on the gseq duration inside sample.

    let global_time_ms = time_ms;

    let mut mats: Vec<BnMat4> = (0..bones.len())
        .map(|i| bone_matrix_at(bones, anim, time_ms, global_time_ms, global_seqs, i))
        .collect();

    twist_pose(bones, &mut mats, twist.0, twist.1);

    skin_vertices(model, &mats)
}

/// Skinned vertices for a masked two-clip pose: `base` animates everything,
/// `over` takes the upper-body subtree (key bone 4, SpineLow), and `weight`
/// eases between the two.
pub fn m2_skinned_positions_rig_masked(
    model: &benilla_m2::M2Model,

    global_seqs: &[u32],

    bones: &[BnBone],

    base: (usize, u32, u32),

    over: (usize, u32, u32),

    weight: f32,

    twist: (f32, f32),
) -> Vec<[f32; 3]> {
    let upper = upper_subtree(bones, 4);

    let mut mats = masked_bone_matrices(bones, global_seqs, base, over, weight, &upper);

    twist_pose(bones, &mut mats, twist.0, twist.1);

    skin_vertices(model, &mats)
}

/// Blend the vertices through their 4 bone matrices: the reference
/// model.cpp loop (`v += mat*pos * (w/255)`), in UE axes.
fn skin_vertices(model: &benilla_m2::M2Model, mats: &[BnMat4]) -> Vec<[f32; 3]> {
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

    out
}

/// A parsed M2 rig kept for the duration of the session: the animation tick
/// calls `bn_model_skinned_positions` every frame, and re-reading + re-parsing
/// the model (plus its bone and global-sequence tables) cost ~10 ms per call.

struct M2Rig {
    cached: Arc<M2Cached>,

    global_seqs: Vec<u32>,

    bones: Vec<BnBone>,
}

fn m2_rig_cache() -> &'static Mutex<HashMap<String, Arc<M2Rig>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<M2Rig>>>> = OnceLock::new();

    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached rig for `path`, parsed on first use. The M2 bytes and model come
/// from the shared parsed-model cache, so a rig costs only the two side tables.

fn m2_rig_cached(archives: &[Archive], path: &str) -> Option<Arc<M2Rig>> {
    let key = path.to_ascii_lowercase();

    {
        let cache = m2_rig_cache().lock().unwrap_or_else(|e| e.into_inner());

        if let Some(rig) = cache.get(&key) {
            return Some(rig.clone());
        }
    }

    let cached = m2_cached(archives, path)?;

    let global_seqs = parse_m2_global_sequences(&cached.bytes);

    let bones = parse_m2_bones(&cached.bytes)?;

    let rig = Arc::new(M2Rig {
        cached,

        global_seqs,

        bones,
    });

    let mut cache = m2_rig_cache().lock().unwrap_or_else(|e| e.into_inner());

    // Bound the cache: a long tour visits every creature model in the world,
    // and each rig pins its parsed M2 through the shared cache entry.

    if cache.len() >= 128 {
        cache.clear();
    }

    cache.insert(key, rig.clone());

    Some(rig)
}

/// Skinned vertex positions **in UE axes** (x, -y, z), matching the

/// existing `bn_model_fill` coordinate convention.

pub fn m2_skinned_positions_ue(bytes: &[u8], anim: usize, time_ms: u32) -> Option<Vec<[f32; 3]>> {
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    m2_bone_count(cached.bytes.as_slice())
}

/// Number of animation sequences in the model.

#[no_mangle]

pub extern "C" fn bn_model_anim_count(world: *const BnWorld, model_path: *const c_char) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }

    let p = match cstr_to_str(model_path) {
        Some(s) => s,

        None => return 0,
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    m2_anim_count(cached.bytes.as_slice())
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let Some((ts, te, dur)) = m2_anim_time_range(cached.bytes.as_slice(), anim_idx) else {
        return 0;
    };

    unsafe {
        *out = ts;

        *out.add(1) = te;

        *out.add(2) = dur;
    }

    1
}

/// Diagnostic: dump the rotation-track header for every bone of the

/// walk-clip sequence so we can see why the sampler folds it to a static pose.

/// out receives 6 u32 slots per bone: interp, seq, n_ranges, n_times, n_keys,

/// range_hi_for_anim.

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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let Some(bones) = parse_m2_bones(cached.bytes.as_slice()) else {
        return 0;
    };

    let mut n = 0u32;

    for b in bones.iter() {
        if n >= cap {
            break;
        }

        let r = &b.rot;

        let hi = if (anim_idx as usize) < r.ranges.len() {
            r.ranges[anim_idx as usize].1
        } else {
            0xFFFF_FFFF
        };

        unsafe {
            out.add((n as usize) * 6 + 0)
                .write_volatile(r.interp as u32);

            out.add((n as usize) * 6 + 1)
                .write_volatile((r.seq as i16) as u32);

            out.add((n as usize) * 6 + 2)
                .write_volatile(r.ranges.len() as u32);

            out.add((n as usize) * 6 + 3)
                .write_volatile(r.times.len() as u32);

            out.add((n as usize) * 6 + 4)
                .write_volatile(r.keys.len() as u32);

            out.add((n as usize) * 6 + 5).write_volatile(hi);
        }

        n += 1;
    }

    n
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

    let Some(cached) = m2_cached(&w.archives, p) else {
        return 0;
    };

    let Some(bones) = parse_m2_bones(cached.bytes.as_slice()) else {
        return 0;
    };

    let Some(b) = bones.get(bone_idx as usize) else {
        return 0;
    };

    let r = &b.rot;

    unsafe {
        out.add(0).write_volatile(r.ranges.len() as u32);

        out.add(1)
            .write_volatile(r.times.first().copied().unwrap_or(0));

        out.add(2)
            .write_volatile(r.times.last().copied().unwrap_or(0));
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

    n
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
    skinned_positions_impl(world, model_path, anim_idx, time_ms, (0.0, 0.0), out, cap)
}

/// [`bn_model_skinned_positions`] with the strafe counter-twist applied to the
/// posed rig before the blend (see [`twist_pose`]).
#[no_mangle]

pub extern "C" fn bn_model_skinned_positions_twist(
    world: *const BnWorld,

    model_path: *const c_char,

    anim_idx: u32,

    time_ms: u32,

    twist_spine: f32,

    twist_head: f32,

    out: *mut f32,

    cap: u32,
) -> u32 {
    skinned_positions_impl(
        world,
        model_path,
        anim_idx,
        time_ms,
        (twist_spine, twist_head),
        out,
        cap,
    )
}

fn skinned_positions_impl(
    world: *const BnWorld,

    model_path: *const c_char,

    anim_idx: u32,

    time_ms: u32,

    twist: (f32, f32),

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

    let rig = match m2_rig_cached(&w.archives, p) {
        Some(r) => r,

        None => return 0,
    };

    let verts = m2_skinned_positions_rig(
        rig.cached.format.model(),
        &rig.global_seqs,
        &rig.bones,
        anim_idx as usize,
        time_ms,
        twist,
    );

    let n = verts.len().min(cap as usize);

    for (i, v) in verts.iter().take(n).enumerate() {
        unsafe {
            *out.add(i * 3) = v[0];

            *out.add(i * 3 + 1) = v[1];

            *out.add(i * 3 + 2) = v[2];
        }
    }

    n as u32
}

/// Skinned vertex positions in UE axes for a masked two-clip pose: the base
/// anim drives the whole model, the overlay anim the upper-body subtree
/// (`keyBoneId` 4, SpineLow), and `weight` eases between them. `out` must hold
/// `vertex_count * 3` floats. Returns the number of vertices written.
#[no_mangle]

pub extern "C" fn bn_model_skinned_positions_masked(
    world: *const BnWorld,

    model_path: *const c_char,

    base_anim: u32,

    base_ms: u32,

    over_anim: u32,

    over_ms: u32,

    weight: f32,

    out: *mut f32,

    cap: u32,
) -> u32 {
    skinned_positions_masked_impl(
        world,
        model_path,
        (base_anim, base_ms),
        (over_anim, over_ms),
        weight,
        (0.0, 0.0),
        out,
        cap,
    )
}

/// [`bn_model_skinned_positions_masked`] with the strafe counter-twist applied
/// to the posed rig before the blend: `spine` turns the SpineLow subtree,
/// `head` the Head subtree, in WoW yaw radians (see [`twist_pose`]).
#[no_mangle]

pub extern "C" fn bn_model_skinned_positions_masked_twist(
    world: *const BnWorld,

    model_path: *const c_char,

    base_anim: u32,

    base_ms: u32,

    over_anim: u32,

    over_ms: u32,

    weight: f32,

    twist_spine: f32,

    twist_head: f32,

    out: *mut f32,

    cap: u32,
) -> u32 {
    skinned_positions_masked_impl(
        world,
        model_path,
        (base_anim, base_ms),
        (over_anim, over_ms),
        weight,
        (twist_spine, twist_head),
        out,
        cap,
    )
}

fn skinned_positions_masked_impl(
    world: *const BnWorld,

    model_path: *const c_char,

    base: (u32, u32),

    over: (u32, u32),

    weight: f32,

    twist: (f32, f32),

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

    let rig = match m2_rig_cached(&w.archives, p) {
        Some(r) => r,

        None => return 0,
    };

    let verts = m2_skinned_positions_rig_masked(
        rig.cached.format.model(),
        &rig.global_seqs,
        &rig.bones,
        (base.0 as usize, base.1, base.1),
        (over.0 as usize, over.1, over.1),
        weight,
        twist,
    );

    let n = verts.len().min(cap as usize);

    for (i, v) in verts.iter().take(n).enumerate() {
        unsafe {
            *out.add(i * 3) = v[0];

            *out.add(i * 3 + 1) = v[1];

            *out.add(i * 3 + 2) = v[2];
        }
    }

    n as u32
}

// ---- Background BLP decode ----
//
// A BLP costs 25-50 ms to decode and the tile build used to pay that on the
// game thread inside its material stage. The host asks for a texture as soon
// as the gather stages discover its path (`bn_texture_request`): the file
// bytes are read here, on the calling thread (cheap - the archives are
// already open), and the decode runs on a small worker pool. `bn_texture`
// then just copies the decoded entry; on a miss it still decodes
// synchronously, so a host that never requests stays correct.

use std::collections::VecDeque;

use std::sync::{Condvar, Mutex, OnceLock};

use std::sync::atomic::{AtomicU64, Ordering};

const TEX_PENDING: u8 = 0;

const TEX_READY: u8 = 1;

const TEX_FAILED: u8 = 2;

/// Decoded RGBA8 kept per texture path, shared by the worker pool and the
/// synchronous `bn_texture` fallback. The whole authored level chain is kept:
/// the 1.12 client uploads each stored BLP level and never regenerates one, so
/// the host needs them all to build a real mip chain.
struct TexEntry {
    status: u8,

    width: u32,

    height: u32,

    /// Level 0 first, as the BLP stores them (`level 0` is what `bn_texture` hands out).
    mips: Vec<(u32, u32, Vec<u8>)>,
}

impl TexEntry {
    /// Every level's bytes, what the cache budget counts.
    fn bytes(&self) -> u64 {
        self.mips.iter().map(|m| m.2.len() as u64).sum()
    }
}

fn tex_pending_entry() -> TexEntry {
    TexEntry {
        status: TEX_PENDING,
        width: 0,
        height: 0,
        mips: Vec::new(),
    }
}

fn tex_failed_entry() -> TexEntry {
    TexEntry {
        status: TEX_FAILED,
        width: 0,
        height: 0,
        mips: Vec::new(),
    }
}

fn tex_ready_entry(w: u32, h: u32, mips: Vec<(u32, u32, Vec<u8>)>) -> TexEntry {
    TexEntry {
        status: TEX_READY,
        width: w,
        height: h,
        mips,
    }
}

/// Decoded pixels kept process-wide: 1024x1024 sheets are 4 MB each, so cap
/// the pool and start over when it grows past the budget. The UE side owns its
/// own UTexture2D cache, so a reset never loses a texture already built.
const TEX_CACHE_BUDGET: u64 = 256 * 1024 * 1024;

static TEX_CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Diagnostic counters, read by the host's per-tile log.
static TEX_STAT_QUEUED: AtomicU64 = AtomicU64::new(0);

static TEX_STAT_WORKER: AtomicU64 = AtomicU64::new(0);

static TEX_STAT_SYNC: AtomicU64 = AtomicU64::new(0);

static TEX_STAT_HIT: AtomicU64 = AtomicU64::new(0);

/// [queued, decoded by workers, decoded synchronously, cache hits].
#[no_mangle]
pub extern "C" fn bn_texture_stats(out4: *mut u32) -> u32 {
    if out4.is_null() {
        return 0;
    }

    unsafe {
        *out4 = TEX_STAT_QUEUED.load(Ordering::Relaxed) as u32;

        *out4.add(1) = TEX_STAT_WORKER.load(Ordering::Relaxed) as u32;

        *out4.add(2) = TEX_STAT_SYNC.load(Ordering::Relaxed) as u32;

        *out4.add(3) = TEX_STAT_HIT.load(Ordering::Relaxed) as u32;
    }

    4
}

fn tex_insert(cache: &mut HashMap<String, TexEntry>, key: String, entry: TexEntry) {
    let old = cache.get(&key).map(|e| e.bytes()).unwrap_or(0);

    let add = entry.bytes();

    // Replacing a pending/failed slot frees nothing, replacing a decoded one
    // returns its bytes; either way the counter stays on the live total.
    let prev = TEX_CACHE_BYTES.fetch_add(add, Ordering::Relaxed);

    if prev.saturating_sub(old) + add > TEX_CACHE_BUDGET {
        cache.retain(|_, e| e.status == TEX_PENDING);

        TEX_CACHE_BYTES.store(add, Ordering::Relaxed);
    }

    cache.insert(key, entry);
}

fn tex_cache() -> &'static Mutex<HashMap<String, TexEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, TexEntry>>> = OnceLock::new();

    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct TexJobs {
    /// (cache key, data dir, internal path). The worker reads the file: an MPQ
    /// read on the calling thread used to cost a millisecond or two per WMO
    /// submesh (3042 of them in one city tile).
    queue: Mutex<VecDeque<(String, String, String)>>,

    ready: Condvar,
}

fn tex_jobs() -> &'static TexJobs {
    static JOBS: OnceLock<TexJobs> = OnceLock::new();

    JOBS.get_or_init(|| TexJobs {
        queue: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
    })
}

/// Cache key: case folded, ".blp" dropped. The host asks both with and
/// without the extension (UI paths carry none) and MPQ paths are
/// case-insensitive, so one key serves every spelling.
fn tex_key(path: &str) -> String {
    let lower = path.trim().to_ascii_lowercase();

    lower.strip_suffix(".blp").unwrap_or(&lower).to_string()
}

/// Read a BLP's bytes from the chain, trying the ".BLP" spelling when the
/// path carries no extension (the host's UI paths are extensionless).
fn read_texture_bytes(w: &BnWorld, path: &str) -> Option<Vec<u8>> {
    for a in w.archives.iter().rev() {
        if a.contains(path) {
            if let Ok(bytes) = a.read_file(path) {
                return Some(bytes);
            }

            break;
        }
    }

    if !path.to_ascii_lowercase().ends_with(".blp") {
        let alt = format!("{}.BLP", path);

        for a in w.archives.iter().rev() {
            if a.contains(&alt) {
                if let Ok(bytes) = a.read_file(&alt) {
                    return Some(bytes);
                }

                break;
            }
        }
    }

    None
}

/// Same read through the worker's own chain. The background decode must not
/// read (and decompress) MPQ files on the game thread: 3042 WMO submesh
/// requests used to cost 874 ms in one gather slice just doing this.
fn read_texture_bytes_chain(chain: &mut benilla_formats::Chain, path: &str) -> Option<Vec<u8>> {
    if chain.contains(path) {
        if let Ok(bytes) = chain.read_file(path) {
            return Some(bytes);
        }
    }

    if !path.to_ascii_lowercase().ends_with(".blp") {
        let alt = format!("{}.BLP", path);

        if chain.contains(&alt) {
            if let Ok(bytes) = chain.read_file(&alt) {
                return Some(bytes);
            }
        }
    }

    None
}

fn decode_blp_bytes(bytes: &[u8]) -> Option<(u32, u32, Vec<(u32, u32, Vec<u8>)>)> {
    let chain = benilla_formats::blp_bytes_to_mip_chain(bytes).ok()?;

    let (width, height) = (chain.width, chain.height);

    let dims: Vec<(u32, u32)> = (0..chain.mips.len() as u32)
        .map(|i| chain.mip_size(i))
        .collect();

    let mips: Vec<(u32, u32, Vec<u8>)> = chain
        .mips
        .into_iter()
        .zip(dims)
        .map(|(level, (w, h))| (w, h, level))
        .collect();

    mips.first()?;

    Some((width, height, mips))
}

/// Some(Some(tex)) = decoded, Some(None) = known miss, None = unknown or
/// still pending (the caller decodes synchronously then).
fn tex_lookup(key: &str) -> Option<Option<(u32, u32, Vec<u8>)>> {
    let cache = tex_cache().lock().ok()?;

    match cache.get(key) {
        Some(e) if e.status == TEX_READY => {
            Some(e.mips.first().map(|m| (e.width, e.height, m.2.clone())))
        }

        Some(e) if e.status == TEX_FAILED => Some(None),

        _ => None,
    }
}

/// One authored level of a cached texture: `Some(Some(_))` decoded, `Some(None)`
/// a known miss or no such level, `None` unknown or still pending.
fn tex_lookup_level(key: &str, level: u32) -> Option<Option<(u32, u32, Vec<u8>)>> {
    let cache = tex_cache().lock().ok()?;

    match cache.get(key) {
        Some(e) if e.status == TEX_READY => Some(e.mips.get(level as usize).cloned()),

        Some(e) if e.status == TEX_FAILED => Some(None),

        _ => None,
    }
}

fn tex_store(key: &str, value: Option<(u32, u32, Vec<(u32, u32, Vec<u8>)>)>) {
    if let Ok(mut cache) = tex_cache().lock() {
        let entry = match value {
            Some((w, h, mips)) => tex_ready_entry(w, h, mips),

            None => tex_failed_entry(),
        };

        tex_insert(&mut cache, key.to_string(), entry);
    }
}

fn decode_texture_sync(w: &BnWorld, path: &str) -> Option<(u32, u32, Vec<(u32, u32, Vec<u8>)>)> {
    let bytes = read_texture_bytes(w, path)?;

    decode_blp_bytes(bytes.as_slice())
}

fn tex_decode_job(key: String, decoded: Option<(u32, u32, Vec<(u32, u32, Vec<u8>)>)>) {
    if let Ok(mut cache) = tex_cache().lock() {
        // The synchronous path may have answered while this job waited.
        if cache.get(&key).is_some_and(|e| e.status != TEX_PENDING) {
            return;
        }

        let entry = match decoded {
            Some((w, h, mips)) => {
                TEX_STAT_WORKER.fetch_add(1, Ordering::Relaxed);

                tex_ready_entry(w, h, mips)
            }

            None => tex_failed_entry(),
        };

        tex_insert(&mut cache, key, entry);
    }
}

/// Start the decode workers once; they live as long as the process.
fn tex_workers() {
    static STARTED: OnceLock<()> = OnceLock::new();

    STARTED.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(2)
            .clamp(1, 2);

        for i in 0..n {
            let _ = std::thread::Builder::new()
                .name(format!("bn-blp-{i}"))
                .spawn(|| {
                    // Each worker reads from its own chain: the game thread
                    // only names the file.
                    let mut chain: Option<(String, benilla_formats::Chain)> = None;

                    loop {
                        let job = {
                            let jobs = tex_jobs();

                            let mut q = match jobs.queue.lock() {
                                Ok(q) => q,

                                Err(_) => return,
                            };

                            while q.is_empty() {
                                q = match jobs.ready.wait(q) {
                                    Ok(q) => q,

                                    Err(_) => return,
                                };
                            }

                            q.pop_front()
                        };

                        let Some((key, dir, path)) = job else {
                            continue;
                        };

                        if chain.as_ref().map_or(true, |(d, _)| d != &dir) {
                            chain = benilla_formats::Chain::open(Path::new(&dir))
                                .ok()
                                .map(|c| (dir, c));
                        }

                        let decoded = chain
                            .as_mut()
                            .and_then(|(_, c)| read_texture_bytes_chain(c, &path))
                            .and_then(|b| decode_blp_bytes(b.as_slice()));

                        tex_decode_job(key, decoded);
                    }
                });
        }
    });
}

/// Queue a background decode for `internal_path`.
///
/// 1 = ready already, 3 = pending already, 0 = queued, 2 = bad arguments.
/// The worker resolves and decodes the file, so this never touches the MPQ.

#[no_mangle]

pub extern "C" fn bn_texture_request(world: *const BnWorld, internal_path: *const c_char) -> u32 {
    if world.is_null() || internal_path.is_null() {
        return 2;
    }

    let Some(p) = cstr_to_str(internal_path) else {
        return 2;
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    tex_request(&w.data_dir, p)
}

/// Queue a background decode into the shared texture cache; 1 = ready
/// already, 3 = pending already, 0 = queued, 2 = known miss.
fn tex_request(dir: &str, path: &str) -> u32 {
    let p = path.trim();

    if p.is_empty() {
        return 2;
    }

    let key = tex_key(p);

    if let Ok(cache) = tex_cache().lock() {
        if let Some(e) = cache.get(&key) {
            return match e.status {
                TEX_READY => 1,

                TEX_PENDING => 3,

                _ => 2,
            };
        }
    }

    // Reserve the slot before queueing: a second request for the same path
    // then answers "pending" instead of queueing it twice.
    if let Ok(mut cache) = tex_cache().lock() {
        if cache.contains_key(&key) {
            return 3;
        }

        cache.insert(key.clone(), tex_pending_entry());
    }

    tex_workers();

    let jobs = tex_jobs();

    if let Ok(mut q) = jobs.queue.lock() {
        q.push_back((key, dir.to_string(), p.to_string()));

        TEX_STAT_QUEUED.fetch_add(1, Ordering::Relaxed);

        jobs.ready.notify_one();
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
    if world.is_null()
        || internal_path.is_null()
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

    // A background request normally filled this already. Pending entries fall
    // through to the synchronous decode below: correctness first, and the tile
    // build normally reaches its material stage long after the workers did.
    let key = tex_key(p);

    let decoded = match tex_lookup(&key) {
        Some(Some(v)) => {
            TEX_STAT_HIT.fetch_add(1, Ordering::Relaxed);

            v
        }

        Some(None) => return 0,

        None => {
            TEX_STAT_SYNC.fetch_add(1, Ordering::Relaxed);

            let v = decode_texture_sync(w, p);

            let level0 = v
                .as_ref()
                .and_then(|(_, _, mips)| mips.first().map(|m| (m.0, m.1, m.2.clone())));

            tex_store(&key, v);

            match level0 {
                Some(v) => v,

                None => return 0,
            }
        }
    };

    let (width, height, rgba) = decoded;

    let need = width.saturating_mul(height).saturating_mul(4);

    if need == 0 || need > cap {
        return 0;
    }

    unsafe {
        *out_width = width;

        *out_height = height;

        std::ptr::copy_nonoverlapping(rgba.as_ptr(), out_rgba, need as usize);
    }

    need
}

/// One authored mip level of a BLP, `level` 0 = full size, RGBA8. The 1.12
/// client uploads the levels the file stores (`glCompressedTexImage2DARB` on
/// each) and never regenerates them, so the host asks for the chain the BLP
/// carries instead of filtering its own. Returns the level's byte count; 0
/// when the texture, the level or the arguments are missing.
#[no_mangle]
pub extern "C" fn bn_texture_mip(
    world: *const BnWorld,

    internal_path: *const c_char,

    level: u32,

    out_width: *mut u32,

    out_height: *mut u32,

    out_rgba: *mut u8,

    cap: u32,
) -> u32 {
    if world.is_null()
        || internal_path.is_null()
        || out_rgba.is_null()
        || out_width.is_null()
        || out_height.is_null()
    {
        return 0;
    }

    let Some(p) = cstr_to_str(internal_path) else {
        return 0;
    };

    // SAFETY: valid handle.

    let w = unsafe { &*world };

    let key = tex_key(p);

    let decoded = match tex_lookup_level(&key, level) {
        Some(Some(level)) => level,

        Some(None) => return 0,

        None => {
            let Some((width, height, mips)) = decode_texture_sync(w, p) else {
                tex_store(&key, None);

                return 0;
            };

            let out = mips.get(level as usize).cloned();

            tex_store(&key, Some((width, height, mips)));

            match out {
                Some(level) => level,

                None => return 0,
            }
        }
    };

    let (width, height, rgba) = decoded;

    let need = width.saturating_mul(height).saturating_mul(4);

    if need == 0 || need > cap {
        return 0;
    }

    unsafe {
        *out_width = width;

        *out_height = height;

        std::ptr::copy_nonoverlapping(rgba.as_ptr(), out_rgba, need as usize);
    }

    need
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

/// The dump's row builders: one `F` line per frame, one `R` line per region,
/// tabs within any field squashed. Each frame is `pcall`ed so one bad widget
/// never sinks the whole dump. WoW 1.12 runs Lua 5.0: no `#` length operator —
/// `table.getn`/`table.insert` instead. `UI_DUMP_WALK_LUA` runs them over the
/// whole frame list in one eval; the sliced API keeps `dump_one` around in
/// `BN_DUMP_FRAME` and walks a few hundred frames per call.
const UI_DUMP_DEFS_LUA: &str = r#"
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
    -- FontString text keeps its hard breaks: the dump's rows split on LF, so a
    -- break rides as US (0x1f) and the host restores it before rasterizing.
    -- TAB flattens to a space and CR drops, as the plain clean does.
    local function text_clean(s)
        local t = tostring(s)
        t = string.gsub(t, "\t", " ")
        t = string.gsub(t, "\r", "")
        t = string.gsub(t, "\n", "\31")
        return t
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
    -- The region's blend (col 28, `GetBlendMode` `0x79a890`): the client draws
    -- an `alphaMode="ADD"` texture (a button's checked/highlight marks, a
    -- nameplate glow) by adding it, so its opaque black background shows as
    -- nothing. Drawn as plain alpha instead, that black covers what lies under
    -- it - the ActionButton checked mark hid the slot it marks. Textures only;
    -- a FontString has no blend and keeps the empty column.
    local function blend_str(r, ty)
        if ty ~= "Texture" or not r.GetBlendMode then return "" end
        local ok, b = pcall(function() return r:GetBlendMode() end)
        if not ok or type(b) ~= "string" then return "" end
        return clean(b)
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
        -- A StatusBar's fill is not its texture region's rect and colour: the
        -- client draws the value's fraction of the bar texture (`0x782dd0`
        -- sizes the quad from min/max/value) and tints it with the bar's own
        -- colour (`0x78fc20` writes the region's vertex colour, which
        -- GetVertexColor on the texture does not answer). The renderer needs
        -- both, so they ride their own row: min, max, value, orientation and
        -- the bar colour.
        if f.IsObjectType and f:IsObjectType("StatusBar") then
            local okv, v = pcall(function() return f:GetValue() end)
            local okm, mn, mx = pcall(function() return f:GetMinMaxValues() end)
            if okv and okm and type(v) == "number" and type(mx) == "number" then
                local orient = "HORIZONTAL"
                local oko, o = pcall(function() return f:GetOrientation() end)
                if oko and type(o) == "string" then orient = o end
                local colr = colour_str(function() return f:GetStatusBarColor() end)
                table.insert(mine, table.concat({ "B", clean(f:GetName()), num(mn),
                    num(mx), num(v), orient, colr }, "\t"))
            end
        end
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
                texcoord_str(r), layer_str(r), blend_str(r, ty) }, "\t"))
            end
        end
        return mine
    end
    BN_DUMP_FRAME = dump_one
"#;

/// One full dump over every frame: the caller evals `UI_DUMP_DEFS_LUA` first.
const UI_DUMP_WALK_LUA: &str = r#"
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

/// Load the stock interface into a fresh in-game VM: the chain's key sets first
/// (stock `ActionButton_OnLoad` paints its hotkey corner from `GetBindingKey`),
/// then every row of the chain's `Interface\FrameXML\FrameXML.toc`, in order
/// (`.lua` runs as a chunk, `.xml` through the official loader), then the core
/// `Interface\FrameXML\Bindings.xml` whose commands the key dispatch runs. File
/// and loader failures are tolerated and skipped. Returns the live frame count
/// at load end, or 0 on failure.
#[no_mangle]
pub extern "C" fn bn_ui_load(world: *mut BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    // The feed's catalogs need the chain mutably, so it binds the world first,
    // in its own borrow scope, before this call takes the chain to read from.
    ui_feed::attach(world);
    // Latches name commands of the VM that made them; a new load clears them.
    BN_LATCH.with(|l| l.borrow_mut().clear());
    // The engine clears the nameplate toggles with the VM it loads.
    nameplates::clear_bits();
    // SAFETY: caller guarantees a valid handle for the call's span.
    let w = unsafe { &*world };
    let Some(chain) = w.chain.as_ref() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The faces must be local before the first measure: the VM asks inside
        // the load, where no chain handle is in scope.
        ui_font::preload_fonts(chain);
        let Ok(mut script) = benilla_ui::script::UiScript::new() else {
            return 0u32;
        };
        // The reference answers `GetStringWidth` from its font engine inside the
        // call that asked (`0x79e510` -> `0x772890`), so a `<OnLoad>` or a
        // `GossipResize` that measures lays out against a real box. With no
        // measurer every wrapped string is 0 tall and the stock frames stack
        // their rows on top of each other.
        script.set_text_measurer(Box::new(ui_font::VmMeasure));
        // The reference registers its whole console-var set before `UI_Init`
        // (`0x490018`) walks the toc, and `GetCVar` raises on a name the table
        // lacks (`0x842310`): `TextStatusBar.lua:47` reads `statusBarText` on
        // every health-bar update, so one unseeded name aborts that tick's whole
        // dispatch. Same rows the app's registry seeds into its own VM.
        script.register_cvars(cvar_defaults::DEFAULTS.iter().copied());
        // `WTF\DefaultBindings.wtf`, set 0 (`0x4b62b0`), then set 1 live as a
        // first login's account set is.
        let defaults = chain
            .read(r"WTF\DefaultBindings.wtf")
            .map(|bytes| {
                benilla_ui::script::keybind::parse_bindings_wtf(&benilla_ui::source::decode(&bytes))
            })
            .unwrap_or_default();
        script.set_default_bindings(defaults);
        script.load_binding_set(1);
        let toc_bytes = chain
            .read(r"Interface\FrameXML\FrameXML.toc")
            .unwrap_or_default();
        let toc = benilla_ui::toc::Toc::parse(&benilla_ui::source::decode(&toc_bytes));
        // The loader resolves `<Script file=>`/`<Include file=>` against the
        // document's dir (`dir_of` splits on `/` only), so hand `load_in` the
        // entry with forward slashes; MPQ reads take backslashes.
        let read = |req: &str| -> Option<Vec<u8>> { chain.read(&req.replace('/', "\\")).ok() };
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
                report.errors.iter().map(|e| format!("{path}: {e}")).chain(
                    report
                        .missing_files
                        .iter()
                        .map(|m| format!("{path}: missing {m}")),
                ),
            );
        }
        // `UI_Init`'s next step after the toc walk (`0x490018`): the core's
        // `Bindings.xml`, whose commands are the bodies key dispatch runs.
        let mut bindings_loaded = 0;
        match chain.read(r"Interface\FrameXML\Bindings.xml") {
            Ok(bytes) => {
                match benilla_ui::bindings_xml::parse(&benilla_ui::source::decode(&bytes)) {
                    Ok(bindings) => {
                        bindings_loaded = bindings.len();
                        script.register_bindings(&bindings);
                    }
                    Err(e) => failures.push(format!("Interface\\FrameXML\\Bindings.xml: {e}")),
                }
            }
            Err(_) => failures.push("Interface\\FrameXML\\Bindings.xml: not found".into()),
        }
        let frames = script.eval::<f64>("return GetNumFrames()").unwrap_or(0.0) as u32;
        // The load's last step is arming `PLAYER_LOGIN`;
        // `bn_ui_world_enter` spends it once the player is in the world.
        script.arm_player_login();
        BN_UI.with(|slot| *slot.borrow_mut() = Some(script));
        eprintln!(
            "bn_ui_load: {} toc rows, {bindings_loaded} bindings, {} failures, {frames} frames",
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

/// Fire the UI's world-entry cascade after `bn_ui_load`, mirroring the app's
/// host feed: `FRAMES_LOADED`, `VARIABLES_LOADED`, the chat loader's
/// `UPDATE_CHAT_WINDOWS` + `UPDATE_CHAT_COLOR` rows, then the login cascade
/// (`PLAYER_LOGIN` once, `PLAYER_ENTERING_WORLD`). Without it the stock UI
/// stays half-initialised — e.g. the chat panel never gets its black window
/// tint and paints as an opaque white block. Returns 1 when a session ran,
/// 0 without one.
#[no_mangle]
pub extern "C" fn bn_ui_world_enter() -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else { return 0u32 };
            script.fire_event("FRAMES_LOADED", Vec::new());
            script.fire_event("VARIABLES_LOADED", Vec::new());
            // `FCF_OnUpdate` reads `UIOptionsFrame:IsShown` every frame; stand
            // in a closed one, as the client's chat loader does.
            let _ = script.run(
                "if not UIOptionsFrame then UIOptionsFrame = CreateFrame('Frame') UIOptionsFrame:Hide() end",
            );
            script.fire_event("UPDATE_CHAT_WINDOWS", Vec::new());
            for entry in script.chat_colors() {
                let norm = |b: u8| f64::from(b as f32 * (1.0f32 / 255.0f32));
                script.fire_event(
                    "UPDATE_CHAT_COLOR",
                    vec![
                        benilla_ui::script::ScriptValue::Str(entry.name),
                        benilla_ui::script::ScriptValue::Number(norm(entry.rgb[0])),
                        benilla_ui::script::ScriptValue::Number(norm(entry.rgb[1])),
                        benilla_ui::script::ScriptValue::Number(norm(entry.rgb[2])),
                    ],
                );
            }
            // The world's first snapshot ahead of the cascade, so the login handlers read
            // real values (the app's `ui_unit` push order); the XP pair's arrival fires
            // `PLAYER_XP_UPDATE`, which is the only thing that paints `MainMenuExpBar`.
            crate::ui_feed::sync(script, 0.0);
            script.fire_world_enter();
            1
        })
    }));
    out.unwrap_or(0)
}

/// The focused EditBox's text-region rect `(left, top, height)` from a dump's
/// own rows: the OVERLAY FontString under `frame` (the ctor's anonymous text
/// region; the static label sits on BACKGROUND). Script-space px, y up.
fn editbox_text_rect_from_dump(text: &str, frame: &str) -> Option<(f32, f32, f32)> {
    let want = format!("F\t{}\t", frame);
    let mut in_block = false;
    for line in text.lines() {
        if line.starts_with("F\t") {
            in_block = line.starts_with(&want);
            continue;
        }
        if !in_block || !line.starts_with("R\t") {
            continue;
        }
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() > 27 && c[3] == "FontString" && c[27] == "OVERLAY" {
            let left = c[4].parse().ok()?;
            let top = c[5].parse().ok()?;
            let height = c[7].parse().ok()?;
            return Some((left, top, height));
        }
    }
    None
}

/// Insert `row` at the end of `frame`'s row block (before the next F row), so
/// the synthetic region stays attached to its frame in the TSV.
fn insert_after_frame_block(text: &str, frame: &str, row: &str) -> String {
    let want = format!("F\t{}\t", frame);
    let mut out = String::with_capacity(text.len() + row.len() + 1);
    let mut in_block = false;
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with("F\t") {
            if in_block && !inserted {
                out.push_str(row);
                out.push('\n');
                inserted = true;
            }
            in_block = line.starts_with(&want);
        }
        out.push_str(line);
    }
    if in_block && !inserted {
        out.push_str(row);
        out.push('\n');
    }
    out
}

/// Dump the loaded UI tree as TSV text (see `UI_DUMP_LUA` for the format).
/// Writes at most `cap - 1` bytes plus a NUL; returns bytes written (0 = no
/// session or failure). Only `IsVisible` frames come out (invisible ones
/// can never paint), in creation order, each frame's regions directly beneath
/// it.
#[no_mangle]
pub extern "C" fn bn_ui_dump(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let mut text = {
        let chunk = format!("{UI_DUMP_DEFS_LUA}{UI_DUMP_WALK_LUA}");
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            BN_UI.with(|slot| match slot.borrow().as_ref() {
                Some(s) => match s.eval::<String>(&chunk) {
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
    ui_dump_apply_caret(&mut text);
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// The paint state of each texture quad that no Lua getter answers, as TSV lines:
/// `owner \t region \t left \t top \t width \t height \t portraitUnit \t circular \t fill r g b a`.
/// The fill columns are blank when the quad has no `SetTexture(r,g,b,a)` fill.
///
/// The client's quad record carries these and its child getters do not: `GetVertexColor` answers
/// the colour the fill *multiplies*, `GetTexture` the sentinel "Solid Texture" for a fill, and a
/// `SetPortraitTexture` unit nothing at all. A host that paints the interface itself reads them
/// here; `left/top/width/height` let it match an unnamed region too.
#[no_mangle]
pub extern "C" fn bn_ui_paint(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let held = slot.borrow();
            let s = held.as_ref()?;
            let mut out = String::new();
            for q in s.extract() {
                let benilla_ui::script::QuadContent::Texture {
                    ref color,
                    circular,
                    ref portrait_unit,
                    ..
                } = q.content
                else {
                    continue;
                };
                if color.is_none() && portrait_unit.is_none() {
                    continue;
                }
                let owner = s.target_owner_name(q.target).unwrap_or_default();
                let region = s.target_region_name(q.target).unwrap_or_default();
                let (l, t, w, h) = q.rect.map_or((0.0, 0.0, 0.0, 0.0), |r| {
                    (r.left, r.top, r.right - r.left, r.top - r.bottom)
                });
                // The row's own "\t{}\n" already separates this column, so the
                // fill starts at its first value - an extra leading tab would
                // push r into col 9 and leave col 8 empty, which is where the
                // host reads r.
                let fill = color.map_or_else(
                    || "\t\t\t".to_string(),
                    |c| format!("{:.2}\t{:.2}\t{:.2}\t{:.2}", c[0], c[1], c[2], c[3]),
                );
                out.push_str(&format!(
                    "{owner}\t{region}\t{l:.1}\t{t:.1}\t{w:.1}\t{h:.1}\t{}\t{}\t{}\n",
                    portrait_unit.as_deref().unwrap_or(""),
                    if circular { 1 } else { 0 },
                    fill,
                ));
            }
            Some(out)
        })
    })) else {
        return 0;
    };
    let Some(text) = text else {
        return 0;
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    // SAFETY: the caller passes a writable buffer of `cap` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// The bag buttons' item-push cards, posed at this frame's play head, as TSV: one row per batch,
/// `alpha \t texture \t triangles \t x y u v \t …` in root units (y up). Each triangle's three
/// corners follow the file's index order, so a host paints them as textured triangles with the
/// dump's seam and offsets. Empty when no card shows; the stock card is the `ITEM_PUSH`
/// animation `ItemAnimTemplate` plays (`Interface\ItemAnimations\ForcedBackpackItem.mdx`).
#[no_mangle]
pub extern "C" fn bn_ui_model_cards(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(ui_cards::rows)) else {
        return 0;
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    // SAFETY: the caller passes a writable buffer of `cap` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// Append the focused EditBox caret row (see the seam note below) to a whole
/// dump's text. Shared by the one-shot and sliced dumps.
fn ui_dump_apply_caret(text: &mut String) {
    // The EditBox caret seam: the host round trip (advances in, text UI out)
    // answered with the FFI's own font engine, then a synthetic caret region
    // row appended for the focused box. The VM builds the caret as a
    // textureless region the dump cannot express, and without the advance
    // table `focused_editbox_text_ui` reports no caret at all - both reasons
    // the host never saw one. Measured in script-space px (seam 1): the drawn
    // size is the em-capped font height times the box's scale, so no seam
    // division rides along.
    let caret = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| -> Option<(String, f32, f32)> {
            let mut held = slot.borrow_mut();
            let s = held.as_mut()?;
            if let Some(req) = s.editbox_advances_request() {
                let outline = (req.outline as u8) as u32;
                let em = req.height.map_or(12.0, |h| h.min(32.0));
                let size_px = em * req.scale;
                let font = req.font.as_deref();
                let cum = ui_font::advances_bytes(font, size_px, outline, &req.text);
                let (rows, cell_h) = match req.wrap_width {
                    Some(w) => {
                        let (r, ch) = ui_font::wrap_rows(
                            font,
                            &req.text,
                            size_px,
                            outline,
                            w * req.scale,
                            req.spacing * req.scale,
                        );
                        (r, ch)
                    }
                    None => (vec![0usize], size_px),
                };
                s.set_editbox_advances(req.id, req.key, cum, rows, cell_h);
            }
            let tui = s.focused_editbox_text_ui()?;
            let name = s.focused_editbox_name()?;
            Some((name, tui.caret_x, tui.cell_h))
        })
    }));
    if let Ok(Some((frame, caret_x, cell_h))) = caret {
        if let Some((left, top, height)) = editbox_text_rect_from_dump(text.as_str(), &frame) {
            // Middle-justified single line: the cell centres in the region.
            const FULL_UV: &str = "0.0000\t0.0000\t1.0000\t0.0000\t1.0000\t1.0000\t0.0000\t1.0000";
            let caret_top = top - (height - cell_h).max(0.0) * 0.5;
            let row = format!(
                "R\t{}\tbn_caret\tTexture\t{:.1}\t{:.1}\t1.5\t{:.1}\tSolid Texture\t\
                 1.00\t1.00\t1.00\t1.00\t1.00\t\t0.0\t\tLEFT\tMIDDLE\t{}\tOVERLAY",
                frame,
                left + caret_x,
                caret_top,
                cell_h.max(1.0),
                FULL_UV
            );
            *text = insert_after_frame_block(text, &frame, &row);
        }
    }
}

// ---- Sliced UI dump ----
//
// The one-shot dump walks ~3700 frames and builds the ~60 visible ones' rows
// in one Lua eval: 10-20 ms the host pays inside a single frame, which reads
// as a periodic hitch. The sliced API parks the frame cursor and the row text
// between calls, so the host walks a few hundred frames per tick and parses
// once the scan is done.

const UI_DUMP_STEP_LUA: &str = r#"
    local budget = __BUDGET__
    local out = {}
    local f = BN_DUMP_CURSOR
    local n = 0
    while f and n < budget do
        if f:IsVisible() then
            local ok, lines = pcall(BN_DUMP_FRAME, f)
            if ok and type(lines) == "table" then
                for i = 1, table.getn(lines) do
                    table.insert(out, lines[i])
                end
            end
        end
        f = EnumerateFrames(f)
        n = n + 1
    end
    BN_DUMP_CURSOR = f
    local done = 0
    if f == nil then done = 1 end
    return table.concat(out, "\n"), done
"#;

struct UiDumpScan {
    text: String,

    done: bool,

    failed: bool,
}

thread_local! {
    static UI_DUMP_SCAN: RefCell<Option<UiDumpScan>> = const { RefCell::new(None) };
}

/// Start a sliced dump: install the row builders in the VM and park the frame
/// cursor at the first frame. 1 = scan started, 0 = no VM.
#[no_mangle]
pub extern "C" fn bn_ui_dump_begin() -> u32 {
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| -> bool {
            let held = slot.borrow();

            let Some(s) = held.as_ref() else { return false };

            let chunk =
                format!("{UI_DUMP_DEFS_LUA}\n    BN_DUMP_CURSOR = EnumerateFrames()\n    return 1");

            match s.eval::<f64>(&chunk) {
                Ok(v) => v != 0.0,
                Err(e) => {
                    eprintln!("bn_ui_dump_begin: eval failed: {e}");
                    false
                }
            }
        })
    }));

    let started = matches!(started, Ok(true));

    UI_DUMP_SCAN.with(|scan| {
        *scan.borrow_mut() = started.then(|| UiDumpScan {
            text: String::new(),
            done: false,
            failed: false,
        });
    });

    u32::from(started)
}

/// Walk at most `frames` frames of the sliced dump, accumulating their rows
/// for `bn_ui_dump_finish`. `*out_done` is 1 once the frame list is exhausted.
/// Returns 1 on success, 0 when there is no scan or the VM failed.
#[no_mangle]
pub extern "C" fn bn_ui_dump_step(frames: u32, out_done: *mut u32) -> u32 {
    if out_done.is_null() {
        return 0;
    }

    let mut done = 0u32;

    let ok = UI_DUMP_SCAN.with(|slot| {
        let Ok(mut held) = slot.try_borrow_mut() else {
            return false;
        };

        let Some(scan) = held.as_mut() else {
            return false;
        };

        if scan.failed {
            return false;
        }

        if scan.done {
            done = 1;
            return true;
        }

        let chunk = UI_DUMP_STEP_LUA.replace("__BUDGET__", &frames.to_string());

        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            BN_UI.with(|ui| -> Option<(String, f64)> {
                let held = ui.borrow();

                let s = held.as_ref()?;

                s.eval::<(String, f64)>(&chunk).ok()
            })
        }));

        match out {
            Ok(Some((text, d))) => {
                if !text.is_empty() {
                    if !scan.text.is_empty() {
                        scan.text.push('\n');
                    }

                    scan.text.push_str(&text);
                }

                if d != 0.0 {
                    scan.done = true;

                    done = 1;
                }

                true
            }

            _ => {
                scan.failed = true;

                eprintln!("bn_ui_dump_step: eval failed");

                false
            }
        }
    });

    unsafe {
        *out_done = done;
    }

    u32::from(ok)
}

/// Finish the sliced dump: append the caret row, copy the TSV into `buf`
/// (NUL-terminated) and reset the scan. 0 bytes when the scan failed.
#[no_mangle]
pub extern "C" fn bn_ui_dump_finish(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }

    let scan = UI_DUMP_SCAN.with(|slot| slot.borrow_mut().take());

    let Some(scan) = scan else {
        return 0;
    };

    if scan.failed {
        return 0;
    }

    let mut text = scan.text;

    ui_dump_apply_caret(&mut text);

    let n = (text.len() as u64).min(cap as u64 - 1) as usize;

    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);

        *buf.add(n) = 0;
    }

    n as u32
}

/// Visible lines of one message frame (chat / error text), one TSV row each:
/// `left top right bottom r g b a font height text` (VM root units, y up).
/// These live in the widget's ring, which the region-walking dump cannot see;
/// the host paints them with `bn_ui_text_bitmap` at the given rect.
/// Returns bytes written (0 = no session, unknown frame or no lines).
#[no_mangle]
pub extern "C" fn bn_ui_message_lines(frame: *const c_char, buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let Some(frame_name) = cstr_to_str(frame) else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let held = slot.borrow();
            let s = held.as_ref()?;
            let mut text = String::new();
            for row in s.message_line_rows(frame_name) {
                // The text is the last column: strip the delimiters it could carry.
                let clean: String = row
                    .text
                    .chars()
                    .map(|c| {
                        if c == '\t' || c == '\n' || c == '\r' {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect();
                text.push_str(&format!(
                    "{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{:.1}\t{}\n",
                    row.left,
                    row.top,
                    row.right,
                    row.bottom,
                    row.color[0],
                    row.color[1],
                    row.color[2],
                    row.color[3],
                    row.font_path,
                    row.font_height,
                    clean
                ));
            }
            Some(text)
        })
    }));
    let Ok(Some(text)) = out else {
        return 0;
    };
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// The icon the cursor carries, as the extensionless BLP path the host draws at the pointer: the
/// app's `payload_icon` (`0x523840` composites it to 32×32 and hands it to the OS cursor). The
/// reference draws a drag in the engine, not in a frame, so it is in no dump row and no paint
/// record; a host that paints the interface itself reads it here. 0, with `buf` emptied, when the
/// cursor carries nothing or no icon resolves.
#[no_mangle]
pub extern "C" fn bn_ui_cursor_icon(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            use benilla_ui::script::CursorPayload;
            let held = slot.borrow();
            let s = held.as_ref()?;
            match s.cursor_payload()? {
                CursorPayload::Item(i) => i.texture,
                CursorPayload::Spell(s) => s.texture,
                CursorPayload::Action(a) => a.texture,
                CursorPayload::Macro(m) => m.texture,
                CursorPayload::PetAction(p) => p.texture,
                // Mode 10: the stabled pet's family icon; a non-empty path gates the grab.
                CursorPayload::StablePet(p) => Some(p.texture),
                // Mode 2: the coin bitmap by magnitude, `GetCoinIcon`'s table.
                CursorPayload::Money(m) => {
                    Some(benilla_ui::script::coin_icon(i64::from(m.copper)).to_string())
                }
                // Mode 5: the vendor row's icon, which the reference resolves from its
                // `ItemDisplayInfo` id (`0xb4d8ec`).
                CursorPayload::Merchant(m) => m.texture,
            }
        })
    }));
    let Ok(Some(path)) = out else {
        unsafe { *buf = 0 };
        return 0;
    };
    let n = (path.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// Shared body of [`bn_ui_text_bitmap`] / [`bn_ui_text_bitmap_wrap`]:
/// rasterize one FontString and hand the host its metrics + RGBA. Metrics are
/// written whenever the raster succeeds — including when `cap` is too small, so
/// the host can grow its buffer and retry. Returns the bytes written.
///
/// `offset` is `(wrap_px, spacing_px)`; honored only when `wrapped`.
#[allow(clippy::too_many_arguments)]
fn text_bitmap_common(
    world: *const BnWorld,
    font_path: *const c_char,
    size_px: f32,
    outline: u32,
    text: *const c_char,
    offset: (f32, f32),
    wrapped: bool,
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
        let face = font_str.unwrap_or(ui_font::DEFAULT_FONT);
        if wrapped {
            ui_font::rasterize_wrapped(chain, face, size_px, outline, text_str, offset.0, offset.1)
        } else {
            // The pre-wrap contract: hard lines only, rows advance by the em.
            ui_font::rasterize(chain, face, size_px, outline, text_str)
        }
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

/// Rasterize one FontString to RGBA8, the original single-line layout: rows
/// advance by the em and no width wrap. Kept byte-for-byte compatible for hosts
/// built before the wrap flag existed.
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
    text_bitmap_common(
        world,
        font_path,
        size_px,
        outline,
        text,
        (0.0, 0.0),
        false,
        out_rgba,
        cap,
        out_w,
        out_h,
        out_ox,
        out_oy,
        out_pw,
        out_ph,
    )
}

/// Same as [`bn_ui_text_bitmap`] but wraps to `wrap_px` drawn pixels
/// (`<= 1` = no wrap, hard `'\n'` still breaks) with `spacing_px` extra line
/// gap. This is what the current UE HUD calls.
#[no_mangle]
pub extern "C" fn bn_ui_text_bitmap_wrap(
    world: *const BnWorld,
    font_path: *const c_char,
    size_px: f32,
    outline: u32,
    wrap_px: f32,
    spacing_px: f32,
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
    text_bitmap_common(
        world,
        font_path,
        size_px,
        outline,
        text,
        (wrap_px, spacing_px),
        true,
        out_rgba,
        cap,
        out_w,
        out_h,
        out_ox,
        out_oy,
        out_pw,
        out_ph,
    )
}

// ---- UI glue (stage 7): the main menu (GlueXML) + pointer feedback ----
//
// The client's menus live in `Interface\GlueXML`, on a VM the client never
// shares with the in-game FrameXML state (so loading one replaces the other in
// `BN_UI`). `bn_ui_load_glue` builds that state from the stock toc with a stub
// `GlueHost` behind `CGlueMgr`: benilla-ffi stays offline (no networking), so
// login and the character roster answer in place, and world entry is queued for
// the render host to collect through `bn_ui_poll_event`. `bn_ui_tick` drives
// OnUpdate and applies the ops the stub queued (a call fan-out borrows the
// script, so the roster cannot load inside the call itself). Geometry arrives
// through the same `bn_ui_dump` as the in-game UI.

use benilla_ui::script::{
    GlueCall, GlueCharacter, GlueCreateCatalog, GlueCreateClass, GlueCreateCounts, GlueCreateRace,
    GlueHost, GlueNameRow, GlueNotice, GlueRealm, GlueRealmCategory, GlueRealmType, GlueSignal,
    GlueText, NoticeState, ScriptValue, UiScript,
};

/// The DBC catalogs the glue screens read: the create screen's races/classes/
/// customization counts and the roster's race/class/zone/model strings. Loaded
/// once with the glue VM.
#[derive(Default)]
struct GlueCatalogs {
    create: Option<benilla_formats::CharCreateCatalog>,
    classes: Option<benilla_formats::ChrClasses>,
    factions: Option<benilla_formats::FactionCatalog>,
    areas: Option<benilla_formats::AreaTableCatalog>,
    creatures: Option<benilla_formats::CreatureCatalog>,
}

/// The menu's bridge between the network worker and the glue VM: the worker
/// state already applied, the pending roster fetch, the realm the screen picked
/// and the status dialog the host raised.
struct MenuSync {
    last_status: u32,
    last_realms_epoch: u64,
    last_chars_epoch: u64,
    pending_roster: bool,
    roster_epoch_at_request: u64,
    realm_index: Option<u32>,
    dialog: Option<&'static str>,
    realms_refresh: bool,
    events: Vec<String>,
    realm_address: String,
    catalogs: GlueCatalogs,
}

impl Default for MenuSync {
    fn default() -> Self {
        MenuSync {
            last_status: u32::MAX,
            last_realms_epoch: 0,
            last_chars_epoch: 0,
            pending_roster: false,
            roster_epoch_at_request: 0,
            realm_index: None,
            dialog: None,
            realms_refresh: false,
            events: Vec::new(),
            realm_address: String::new(),
            catalogs: GlueCatalogs::default(),
        }
    }
}

thread_local! {
    static BN_MENU: RefCell<MenuSync> = RefCell::new(MenuSync::default());
}

fn open_key(which: &'static str, key: &'static str) -> GlueSignal {
    GlueSignal::Open {
        which,
        text: Some(GlueText::Key(key)),
        data: None,
    }
}

fn open_raw(which: &'static str, text: String) -> GlueSignal {
    GlueSignal::Open {
        which,
        text: Some(GlueText::Raw(text)),
        data: None,
    }
}

fn set_dialog(which: Option<&'static str>) {
    BN_MENU.with(|m| m.borrow_mut().dialog = which);
}

/// The `FactionGroup` rows whose bit the race template's mask holds.
fn faction_groups<'a>(
    factions: &'a benilla_formats::FactionCatalog,
    template: &'a benilla_formats::FactionTemplate,
) -> impl Iterator<Item = &'a benilla_formats::FactionGroupRow> {
    factions
        .faction_groups()
        .iter()
        .filter(|g| g.mask_id != 0 && template.group_mask & 1u32.wrapping_shl(g.mask_id) != 0)
}

/// The race's body display for a sex, then that display's model row.
fn display_resolves(cats: &GlueCatalogs, display: u32) -> bool {
    let Some(creatures) = cats.creatures.as_ref() else {
        return false;
    };
    let models: std::collections::HashSet<u32> =
        creatures.sized_models().map(|(id, _, _)| id).collect();
    creatures
        .display_models()
        .any(|(d, m)| d == display && models.contains(&m))
}

/// The create screen's catalog, assembled the way the glue build walks it
fn build_create_catalog(cats: &GlueCatalogs) -> GlueCreateCatalog {
    let mut catalog = GlueCreateCatalog::default();
    let Some(create) = cats.create.as_ref() else {
        return catalog;
    };
    let Some(factions) = cats.factions.as_ref() else {
        return catalog;
    };
    for side in ["Alliance", "Horde"] {
        for row in create.race_rows() {
            if row.flags & 0x1 != 0 {
                continue;
            }
            let Some(template) = factions.template(row.faction) else {
                continue;
            };
            for _ in
                faction_groups(factions, template).filter(|g| g.internal.eq_ignore_ascii_case(side))
            {
                let id = row.id;
                let token = |t: Option<&str>| t.unwrap_or_default().to_string();
                catalog.races.push(GlueCreateRace {
                    id,
                    name: token(create.race_name(id)),
                    file: token(create.race_file(id)),
                    faction: faction_groups(factions, template)
                        .next()
                        .map(|g| (g.name.clone(), g.internal.clone())),
                    tokens: [
                        token(create.facial_hair_customization(id, 0)),
                        token(create.facial_hair_customization(id, 1)),
                        token(create.hair_customization(id)),
                    ],
                });
            }
        }
    }
    for race in catalog.races.iter().map(|r| r.id).collect::<Vec<_>>() {
        let list = create.class_list(race);
        for &class in &list {
            if let Some(table) = cats
                .classes
                .as_ref()
                .filter(|t| t.has_row(u32::from(class)))
            {
                catalog.classes.insert(
                    class,
                    GlueCreateClass {
                        name: table.name(u32::from(class)).unwrap_or_default().into(),
                        file: table.file(u32::from(class)).unwrap_or_default().into(),
                    },
                );
            }
        }
        catalog.class_lists.insert(race, list);
        for sex in 0..=2u8 {
            if let Some(r) = create.ranges(race, sex) {
                catalog.counts.insert(
                    (race, sex),
                    GlueCreateCounts {
                        skin: i32::from(r.skin),
                        face: i32::from(r.face),
                        hair_styles: i32::from(r.hair_style),
                        hair_colors: (0..r.hair_style)
                            .map(|style| i32::from(create.hair_colors(race, sex, style)))
                            .collect(),
                        facial_hair: i32::from(r.facial_hair),
                    },
                );
            }
            if create
                .body_display(race, sex)
                .is_some_and(|d| display_resolves(cats, d))
            {
                catalog.models.insert((race, sex));
            }
        }
    }
    catalog.names = create
        .name_gen()
        .iter()
        .map(|r| GlueNameRow {
            race: r.race,
            sex: r.sex,
            name: r.name.clone(),
        })
        .collect();
    catalog
}

/// The menu's `CGlueMgr` host over the network worker: the verbs' dialogs and
/// state land in the call; the replies arrive on later ticks.
struct MenuHost;

impl MenuHost {
    /// The roster rows as the character screen's records take them.
    fn map_chars(sync: &MenuSync) -> Vec<GlueCharacter> {
        let text = |s: Option<&str>| s.unwrap_or_default().to_string();
        let mut out = Vec::new();
        for row in net::menu_chars() {
            let bytes: Vec<u8> = row
                .name
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            out.push(GlueCharacter {
                guid: row.guid,
                name: String::from_utf8_lossy(&bytes).into_owned(),
                race: text(
                    sync.catalogs
                        .create
                        .as_ref()
                        .and_then(|r| r.race_name(row.race)),
                ),
                class: text(
                    sync.catalogs
                        .classes
                        .as_ref()
                        .and_then(|c| c.name(u32::from(row.class))),
                ),
                level: row.level,
                zone: sync
                    .catalogs
                    .areas
                    .as_ref()
                    .and_then(|a| a.name(row.zone))
                    .map(str::to_string),
                file_string: text(
                    sync.catalogs
                        .create
                        .as_ref()
                        .and_then(|r| r.race_file(row.race)),
                ),
                gender: row.gender,
                flags: 0,
                model: sync
                    .catalogs
                    .create
                    .as_ref()
                    .and_then(|r| r.body_display(row.race, row.gender))
                    .is_some_and(|d| display_resolves(&sync.catalogs, d)),
            });
        }
        out
    }
}

impl GlueHost for MenuHost {
    fn call(&self, call: GlueCall) -> Vec<GlueSignal> {
        eprintln!("bn_menu: call {call:?}");
        match call {
            // `CGlueMgr::Login`'s own dialog half: empty boxes open an `OKAY`,
            // else state 1 with the `CANCEL` dialog and the logon.
            GlueCall::Login(c) => {
                if c.account.is_empty() {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "LOGIN_ENTER_NAME")];
                }
                if c.password.is_empty() {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "LOGIN_ENTER_PASSWORD")];
                }
                let addr = BN_MENU.with(|m| m.borrow().realm_address.clone());
                BN_MENU.with(|m| {
                    let mut s = m.borrow_mut();
                    s.last_status = u32::MAX;
                    s.last_realms_epoch = 0;
                    s.last_chars_epoch = 0;
                    s.pending_roster = false;
                    s.realm_index = None;
                    s.dialog = Some("CANCEL");
                });
                let _ = net::menu_login(&addr, &c.account, &c.password);
                vec![GlueSignal::Open {
                    which: "CANCEL",
                    text: None,
                    data: None,
                }]
            }
            // The dialog button at the logon cancels the link, then closes.
            GlueCall::StatusDialogClick => {
                if net::menu_snapshot().status == net::ST_CONNECTING {
                    let _ = net::menu_logout();
                }
                set_dialog(None);
                vec![GlueSignal::Close]
            }
            GlueCall::DisconnectFromServer => {
                let _ = net::menu_logout();
                set_dialog(None);
                BN_MENU.with(|m| m.borrow_mut().last_status = u32::MAX);
                vec![GlueSignal::Close, GlueSignal::Disconnected]
            }
            GlueCall::QuitGame => vec![],
            // State 4: the `REALM_LIST_IN_PROGRESS` dialog when asked, and the
            // rebuild of the list the worker already holds.
            GlueCall::RequestRealmList { dialog } => {
                BN_MENU.with(|m| m.borrow_mut().realms_refresh = true);
                if dialog {
                    set_dialog(Some("CANCEL"));
                    vec![open_key("CANCEL", "REALM_LIST_IN_PROGRESS")]
                } else {
                    vec![]
                }
            }
            GlueCall::CancelRealmListQuery => {
                set_dialog(None);
                vec![GlueSignal::Close]
            }
            GlueCall::CancelLogon => {
                let _ = net::menu_logout();
                set_dialog(None);
                vec![GlueSignal::Close]
            }
            // `ConnectToRealm`: the realm is picked by name, then the world link.
            GlueCall::ChangeRealm(name) => match net::menu_realm_index(&name) {
                Some(index) => {
                    BN_MENU.with(|m| {
                        let mut s = m.borrow_mut();
                        s.realm_index = Some(index);
                        s.dialog = Some("CANCEL");
                    });
                    let _ = net::menu_pick_realm(index);
                    vec![open_key("CANCEL", "GAME_SERVER_LOGIN")]
                }
                None => {
                    set_dialog(Some("OKAY"));
                    vec![open_key("OKAY", "REALM_LIST_REALM_NOT_FOUND")]
                }
            },
            // State 3: the dialog and the `CMSG_CHAR_ENUM`; the roster reply is
            // loaded by the tick, with the script free.
            GlueCall::GetCharacterList => {
                let now = net::menu_snapshot().chars_epoch;
                if !net::menu_refresh_roster() {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "CHAR_LIST_FAILED")];
                }
                BN_MENU.with(|m| {
                    let mut s = m.borrow_mut();
                    s.pending_roster = true;
                    s.roster_epoch_at_request = now;
                    s.dialog = Some("CANCEL");
                });
                vec![open_key("CANCEL", "CHAR_LIST_RETRIEVING")]
            }
            GlueCall::DeleteCharacter(guid) => {
                if !net::menu_delete_character(guid) {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "RESPONSE_FAILED_TO_CONNECT")];
                }
                set_dialog(Some("CANCEL"));
                vec![open_key("CANCEL", "CHAR_DELETE_IN_PROGRESS")]
            }
            GlueCall::RenameCharacter { guid, name } => {
                if !net::menu_rename_character(guid, name) {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "RESPONSE_FAILED_TO_CONNECT")];
                }
                set_dialog(Some("CANCEL"));
                vec![open_key("CANCEL", "CHAR_RENAME_IN_PROGRESS")]
            }
            GlueCall::CreateCharacter(request) => {
                let req = benilla_protocol::CharCreateReq {
                    name: request.name.clone(),
                    race: request.race,
                    class: request.class,
                    gender: request.sex,
                    skin: request.skin,
                    face: request.face,
                    hair_style: request.hair_style,
                    hair_color: request.hair_color,
                    facial_hair: request.facial_hair,
                };
                if !net::menu_create_character(req) {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "RESPONSE_FAILED_TO_CONNECT")];
                }
                set_dialog(Some("CANCEL"));
                vec![open_key("CANCEL", "CHAR_CREATE_IN_PROGRESS")]
            }
            // The world entry's guards and the pick. The render host sees the
            // world through `bn_net_status == ST_IN_WORLD`.
            GlueCall::EnterWorld { guid, flags, .. } => {
                if flags & 0x4 != 0 {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "CHAR_LOGIN_LOCKED_FOR_TRANSFER")];
                }
                if flags & 0x4000 != 0 {
                    return vec![GlueSignal::ForceRename("CHAR_RENAME_DESCRIPTION")];
                }
                if !net::menu_enter(guid) {
                    set_dialog(Some("OKAY"));
                    return vec![open_key("OKAY", "RESPONSE_FAILED_TO_CONNECT")];
                }
                vec![]
            }
        }
    }

    fn resume(&self, _step: u32) -> Vec<GlueSignal> {
        Vec::new()
    }

    /// The world link's flag: up from the character roster to the world.
    fn connected(&self) -> bool {
        matches!(
            net::menu_snapshot().status,
            net::ST_CHARS | net::ST_ENTERING | net::ST_IN_WORLD
        )
    }

    fn selected_realm_type(&self) -> Option<u32> {
        let index = BN_MENU.with(|m| m.borrow().realm_index)?;
        net::menu_realms().get(index as usize).map(|r| r.realm_type)
    }
}

/// Load the stock menus (`Interface\GlueXML\GlueXML.toc`) into a fresh glue VM
/// (`UiScript::new_glue`), seat the offline `MenuHost`, and put the login
/// screen up (`SET_GLUE_SCREEN "login"`), mirroring the client's glue build.
/// Returns the live frame count, or 0 on failure. Loading glue replaces any
/// in-game UI session in `BN_UI` and vice versa (the client keeps the two
/// states apart).
#[no_mangle]
pub extern "C" fn bn_ui_load_glue(world: *const BnWorld) -> u32 {
    if world.is_null() {
        return 0;
    }
    // SAFETY: caller guarantees a valid exclusive handle for the call's span.
    // The host owns the world exclusively while a load runs; the FFI keeps the
    // const pointer for the read-only entry points, so the chain's mutable
    // catalogs are reached through this single-threaded cast.
    let w = unsafe { &mut *(world as *mut BnWorld) };
    let Some(chain) = w.chain.as_mut() else {
        return 0;
    };
    // Warm the FONTS cache now: bn_ui_dump's EditBox advance measurement runs
    // through cached_font, which has no world handle to read the MPQs with.
    // (bn_ui_text_bitmap loads lazily, but the caret must measure before any
    // label has been rasterised.)
    ui_font::preload_fonts(chain);
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut script) = UiScript::new_glue() else {
            return 0u32;
        };
        script.set_text_measurer(Box::new(ui_font::VmMeasure));
        // The engine registers every console var before either interface loads;
        // the glue screens read `gxResolution`, `readEULA` and `movieSubtitle`.
        script.register_cvars(cvar_defaults::DEFAULTS.iter().copied());
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
        let read = |req: &str| -> Option<Vec<u8>> { chain.read(&req.replace('/', "\\")).ok() };
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
                report.errors.iter().map(|e| format!("{path}: {e}")).chain(
                    report
                        .missing_files
                        .iter()
                        .map(|m| format!("{path}: missing {m}")),
                ),
            );
        }
        // The glue screens' DBC catalogs: the create screen's races/classes and
        // the roster's race/class/zone/model strings, loaded once with the VM.
        let mut catalogs = GlueCatalogs::default();
        catalogs.create = benilla_formats::CharCreateCatalog::load(chain).ok();
        catalogs.classes = benilla_formats::load_chr_classes(chain).ok();
        catalogs.factions = benilla_formats::load_faction_catalog(chain).ok();
        catalogs.areas = benilla_formats::load_area_table_catalog(chain).ok();
        catalogs.creatures = benilla_formats::load_creature_catalog(chain).ok();
        let create_catalog = build_create_catalog(&catalogs);
        script.set_glue_create_catalog(std::rc::Rc::new(create_catalog));
        let categories = benilla_formats::load_realm_categories(chain, 1)
            .unwrap_or_default()
            .into_iter()
            .map(|c| GlueRealmCategory {
                id: c.id,
                name: c.name,
            })
            .collect();
        let types = benilla_formats::load_realm_configs(chain)
            .map(|configs| {
                configs
                    .rows()
                    .iter()
                    .map(|r| GlueRealmType {
                        realm_type: r.realm_type,
                        pvp: r.pvp,
                        rp: r.rp,
                    })
                    .collect()
            })
            .unwrap_or_default();
        script.set_glue_realm_setup(categories, types);
        let realm_address = std::env::var("BN_REALMD")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| script.cvar("realmlist").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "127.0.0.1:3724".to_string());
        BN_MENU.with(|m| {
            *m.borrow_mut() = MenuSync {
                realm_address,
                catalogs,
                ..MenuSync::default()
            };
        });
        // `GetBillingPlan` is one of benilla's NOT_YET glue verbs (upstream's own
        // list), but zhCN's `CharacterSelect_OnShow` calls it and aborts on the
        // nil, skipping its tail (`GlueFrameFadeIn`). Stand it in only while
        // benilla leaves it unset, so a real binding is never shadowed.
        let _ = script
            .run("if not GetBillingPlan then GetBillingPlan = function() return 0, 0, 0 end end");
        script.fire_event("FRAMES_LOADED", Vec::new());
        // Seat the host after the load (as the client does) and run the
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
    nameplates::set_root_units(w, h);
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

/// The camera's backbuffer, in device pixels. The nameplates project a world point through the cone
/// the host actually renders, so they need the window the scene draws into, which the UI root's own
/// rect ([`bn_ui_set_screen_size`]) is letterboxed inside. Returns 1.
#[no_mangle]
pub extern "C" fn bn_ui_set_viewport(w: f32, h: f32) -> u32 {
    nameplates::set_screen_px(w, h);
    1
}

/// Seat the host's pixel factor, `canvas height / 768`, for the VM's text
/// measure. The measure steps glyphs at the drawn size and divides the factor
/// back out, so a resize drops the stored metrics: a stale one truncates text
/// that fits. Returns 1 when the measures were dropped.
#[no_mangle]
pub extern "C" fn bn_ui_set_seam(seam: f32) -> u32 {
    let moved = seam > 0.0 && (ui_font::seam() - seam).abs() > 1e-4;
    ui_font::set_seam(seam);
    if !moved {
        return 0;
    }
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(script) => {
                    script.invalidate_text_measures();
                    1u32
                }
                None => 0u32,
            }
        })
    }));
    out.unwrap_or(0)
}

/// Fire a batch of host signals into the VM, in order.
fn fire(script: &mut UiScript, signals: Vec<GlueSignal>) {
    for signal in signals {
        script.fire_glue_signal(signal);
    }
}

/// Publish the worker's realm list and run the glue rebuild (whose tail fires
/// `OPEN_REALM_LIST` or `GET_PREFERRED_REALM_INFO`). The account's character
/// count comes from the realm records so a roster-bearing account gets the
/// list rather than the new-account wizard.
fn publish_realms(script: &mut UiScript) {
    let realms: Vec<GlueRealm> = net::menu_realms()
        .iter()
        .map(|r| GlueRealm {
            name: r.name.clone(),
            realm_type: r.realm_type,
            flags: r.flags,
            population: r.population,
            characters: r.characters,
            category: r.category,
        })
        .collect();
    let count: u16 = realms.iter().map(|r| u16::from(r.characters)).sum::<u16>();
    eprintln!(
        "bn_menu: publish realms={} chars={count} {:?}",
        realms.len(),
        realms
            .iter()
            .map(|r| (&r.name, r.category, r.flags, r.characters))
            .collect::<Vec<_>>()
    );
    // The client's realm-list reply frame: the
    // status dialog closed first, then the holder and the rebuild.
    fire(script, vec![GlueSignal::Close]);
    script.set_glue_realms(realms);
    script.set_glue_realm_characters(count);
    script.rebuild_glue_realms();
    let n = script.eval::<f64>("return GetNumRealms(1)").unwrap_or(-1.0);
    let cats = script
        .eval::<f64>("local t = GetRealmCategories(); return t.n or #t")
        .unwrap_or(-1.0);
    eprintln!(
        "bn_menu: after rebuild screen={:?} realms(1)={n} categories={cats}",
        script.glue_screen()
    );
}

/// The `GetCharacterListUpdate` tail, called from the tick for the screen's
/// `OnShow` and after a delete or rename.
fn begin_char_list(script: &mut UiScript) {
    for signal in MenuHost.call(GlueCall::GetCharacterList) {
        script.fire_glue_signal(signal);
    }
}

/// Apply the worker's replies to the glue VM: the realm list, screen
/// transitions, the roster and the status dialogs. Runs outside a verb's call
/// fan-out, which is why the replies wait for a tick.
fn sync_menu(script: &mut UiScript) {
    let snap = net::menu_snapshot();
    let last_status = BN_MENU.with(|m| m.borrow().last_status);
    let last_realms = BN_MENU.with(|m| m.borrow().last_realms_epoch);
    let refresh_realms = BN_MENU.with(|m| std::mem::take(&mut m.borrow_mut().realms_refresh));

    if snap.realms_epoch != last_realms {
        BN_MENU.with(|m| m.borrow_mut().last_realms_epoch = snap.realms_epoch);
        publish_realms(script);
    } else if refresh_realms {
        publish_realms(script);
    }

    if snap.status != last_status {
        BN_MENU.with(|m| m.borrow_mut().last_status = snap.status);
        eprintln!(
            "bn_menu: status {last_status} -> {} (screen={:?})",
            snap.status,
            script.glue_screen()
        );
        match snap.status {
            // Auth OK: the roster is up, and its OnShow asks for the list.
            net::ST_CHARS => {
                set_dialog(None);
                fire(
                    script,
                    vec![
                        GlueSignal::Close,
                        GlueSignal::Teardown,
                        GlueSignal::Screen("charselect"),
                    ],
                );
            }
            net::ST_FAILED => {
                set_dialog(None);
                let msg = net::menu_take_error().unwrap_or_else(|| "login failed".to_string());
                fire(script, vec![GlueSignal::Close, open_raw("OKAY", msg)]);
            }
            // The world is up; the render host polls this through
            // `bn_ui_poll_event` (it also reads `bn_net_status`).
            net::ST_IN_WORLD => {
                BN_MENU.with(|m| m.borrow_mut().events.push("enter_world".to_string()));
            }
            _ => {}
        }
    }

    // The `SMSG_CHAR_ENUM` reply the pending query waited for.
    let (pending, at) = BN_MENU.with(|m| {
        let s = m.borrow();
        (s.pending_roster, s.roster_epoch_at_request)
    });
    if pending && snap.chars_epoch != at {
        BN_MENU.with(|m| {
            let mut s = m.borrow_mut();
            s.pending_roster = false;
            s.last_chars_epoch = snap.chars_epoch;
            s.dialog = None;
        });
        fire(script, vec![GlueSignal::Close]);
        let chars = BN_MENU.with(|m| MenuHost::map_chars(&m.borrow()));
        script.load_glue_roster(chars);
    }

    if let Some((_name, code)) = net::menu_take_create_reply() {
        set_dialog(None);
        if code == 0x2E {
            fire(
                script,
                vec![
                    GlueSignal::Close,
                    GlueSignal::SelectLast,
                    GlueSignal::Screen("charselect"),
                ],
            );
        } else {
            fire(
                script,
                vec![
                    GlueSignal::Close,
                    open_key("OKAY", benilla_ui::script::glue_status_key(i32::from(code))),
                ],
            );
        }
    }

    if let Some((_guid, code)) = net::menu_take_delete_reply() {
        if code == 0x39 {
            fire(script, vec![GlueSignal::Close, GlueSignal::SelectFirst]);
            begin_char_list(script);
        } else {
            set_dialog(None);
            fire(
                script,
                vec![
                    GlueSignal::Close,
                    open_key("OKAY", benilla_ui::script::glue_status_key(i32::from(code))),
                ],
            );
        }
    }

    if let Some((_guid, _name, code)) = net::menu_take_rename_reply() {
        set_dialog(None);
        if code == 0x2E {
            fire(script, vec![GlueSignal::Close]);
            begin_char_list(script);
        } else if code == 0x31 {
            fire(
                script,
                vec![
                    GlueSignal::Close,
                    GlueSignal::ForceRename("CHAR_CREATE_NAME_IN_USE"),
                ],
            );
        } else {
            fire(
                script,
                vec![
                    GlueSignal::Close,
                    open_key("OKAY", benilla_ui::script::glue_status_key(i32::from(code))),
                ],
            );
        }
    }

    if let Some(msg) = net::menu_take_error() {
        set_dialog(None);
        fire(script, vec![GlueSignal::Close, open_raw("OKAY", msg)]);
    }
}

/// Surface one VM error where the host can read it. A dll has no console, so
/// `eprintln!` from inside it reaches nobody but a debugger; and a handler that
/// errors every tick would write the host log at frame rate, so each distinct
/// line is noted once.
fn note_vm_error(line: impl std::fmt::Display) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let line = line.to_string();
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|s| *s == line) || seen.len() >= 256 {
        return;
    }
    seen.push(line.clone());
    drop(seen);
    net::note(format!("ui: {line}"));
}

/// Advance the loaded UI (OnUpdate handlers, hover re-pick, queued events) and
/// apply the menu host's pending ops (realm list, screen transitions, roster).
/// `elapsed` is seconds since the last call. Returns 1 with a session, 0
/// without.
#[no_mangle]
pub extern "C" fn bn_ui_tick(elapsed: f32) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            if script.vm_kind() == benilla_ui::script::VmKind::Glue {
                sync_menu(script);
            } else {
                // Live session state in (unit tokens, money, actions, auras,
                // loot, party, death), before the frame's OnUpdates read it.
                ui_feed::sync(script, elapsed);
            }
            script.tick(elapsed);
            // This VM is the whole client: a Lua write lands in the table itself
            // and no host registry mirrors it, so the queues a write reports to
            // that registry are drained here rather than growing per write.
            let _ = script.take_cvar_changes();
            let _ = script.take_cvar_console_lines();
            if script.vm_kind() == benilla_ui::script::VmKind::InGame {
                // The frame's verbs back out: action uses, casts, selection,
                // loot, the death dialog, chat, and the movement bindings.
                ui_feed::intents(script);
            }
            for e in script.take_errors() {
                note_vm_error(format!("tick: {e}"));
            }
            1u32
        })
    }));
    out.unwrap_or(0)
}

/// Move the UI pointer to `(x, y)` in the root's screen space (`bn_ui_set_
/// screen_size`, y up). Fires hover enter/leave like the client's cursor
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

/// The binding key a mouse button's chord uses: 1.12 names the buttons `BUTTON1`
/// to `BUTTON5` (`IsValidBindingKeyString`'s `BUTTON` + digits arm, `0x846c04`).
fn mouse_binding_key(button: &str) -> Option<&'static str> {
    match button {
        "LeftButton" => Some("BUTTON1"),
        "RightButton" => Some("BUTTON2"),
        "MiddleButton" => Some("BUTTON3"),
        "Button4" => Some("BUTTON4"),
        "Button5" => Some("BUTTON5"),
        _ => None,
    }
}

/// Press or release a mouse button at `(x, y)`; `button` is the WoW name
/// ("LeftButton", ...), `mods` the chord bits (bit0 Shift, bit1 Ctrl, bit2 Alt),
/// `down` 1 = press, 0 = release.
///
/// Returns the arms that took the event: bit0 a frame's click, bit1 the world
/// frame's binding, bit2 a left world click and bit3 a right one. A press binds
/// only with the cursor over the world, since a frame owns its own clicks
/// (`bindings.rs`'s mouse arm); a release always runs the latch's up-half, whatever
/// was under the cursor when the press came. The two click bits are the reference's
/// up-edge predicate (`0x514ae0`, `player/camera.rs`'s `PressGesture`): a press settles
/// as a click only within 200 ms, or within 800 ms under 2.25° of yaw and 2.0° of pitch
/// travel, so a drag or a chord selects nothing.
#[no_mangle]
pub extern "C" fn bn_ui_mouse_button(
    x: f32,
    y: f32,
    button: *const c_char,
    down: u32,
    mods: u32,
) -> u32 {
    if button.is_null() {
        return 0;
    }
    let Ok(name) = (unsafe { std::ffi::CStr::from_ptr(button) }).to_str() else {
        return 0;
    };
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            let taken = u32::from(script.mouse_button(x, y, name, down != 0));
            // The world's own channel: a press a frame captured starts no look session
            // (`player/camera.rs`'s `WorldMouse`), so the drag turns nothing. The release
            // answers with the click that channel settled.
            let taken = taken | (player::ui_mouse_edge(name, down != 0, taken != 0) << 2);
            let Some(key) = mouse_binding_key(name) else {
                return taken;
            };
            if down != 0 {
                if taken != 0 {
                    return taken;
                }
                if BN_LATCH.with(|l| l.borrow().iter().any(|(k, _)| k == key)) {
                    return taken; // a repeat of a button already held
                }
                let Some(command) = resolve_binding(script, key, mods) else {
                    return taken;
                };
                let _ = script.execute_binding(&command, true);
                BN_LATCH.with(|l| l.borrow_mut().push((key.to_owned(), command)));
                taken | 2
            } else {
                let latched = BN_LATCH.with(|l| {
                    let mut l = l.borrow_mut();
                    l.iter().position(|(k, _)| k == key).map(|i| l.remove(i))
                });
                if let Some((_, command)) = latched {
                    let _ = script.execute_binding(&command, false);
                }
                taken
            }
        })
    }));
    out.unwrap_or(0)
}

// The whole notches a stream of wheel travel has left, carrying the fraction
// between events (`bindings.rs`'s `WheelNotches`): a reversal drops the carry,
// since it starts a new gesture. Stock `ScrollFrameTemplate_OnMouseWheel` moves
// half a pane per call, so it is fed the sign, never the raw delta.
thread_local! {
    static BN_WHEEL_CARRY: RefCell<f32> = const { RefCell::new(0.0) };
}

fn wheel_notches(lines: f32) -> i32 {
    if lines == 0.0 || !lines.is_finite() {
        return 0;
    }
    BN_WHEEL_CARRY.with(|c| {
        let mut carry = c.borrow_mut();
        if *carry != 0.0 && carry.signum() != lines.signum() {
            *carry = 0.0;
        }
        *carry += lines;
        let whole = carry.trunc();
        *carry -= whole;
        whole as i32
    })
}

/// One wheel sweep at the cursor `(x, y)` (UI units, y up): `lines` is the
/// platform's signed travel in mouse-wheel lines, `mods` the held chord bits.
///
/// Each whole notch walks the frames that took `EnableMouseWheel` (`0x7664f0`);
/// one that reaches the world frame runs its `MOUSEWHEELUP`/`MOUSEWHEELDOWN`
/// binding as a press and its release back to back, as `0x483d6f`/`0x483d82` do.
/// Returns the notches run, 0 when a session is missing or none completed.
#[no_mangle]
pub extern "C" fn bn_ui_mouse_wheel(x: f32, y: f32, lines: f32, mods: u32) -> u32 {
    let notches = wheel_notches(lines);
    if notches == 0 {
        return 0;
    }
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            let mut run = 0u32;
            for _ in 0..notches.abs() {
                let up = notches > 0;
                let token = if up { "MOUSEWHEELUP" } else { "MOUSEWHEELDOWN" };
                // The world slot only answers when its chord has a binding
                // (`0x483d87` sums the two `ExecuteBinding` results), so a chord
                // with none lets the sweep go on behind it.
                let command = resolve_binding(script, token, mods);
                match script.mouse_wheel_bound(x, y, if up { 1.0 } else { -1.0 }, command.is_some())
                {
                    benilla_ui::script::WheelWalk::World => {
                        if let Some(command) = command {
                            // One notch is a press and its release, back to back
                            // (`0x483d6f` then `0x483d82`); a `runOnUp` body runs
                            // both halves, a plain one's up leg is a no-op.
                            let _ = script.execute_binding(&command, true);
                            let _ = script.execute_binding(&command, false);
                            run += 1;
                        }
                    }
                    // A frame's `OnMouseWheel` took the notch; the walk stops there.
                    benilla_ui::script::WheelWalk::Frame => run += 1,
                    benilla_ui::script::WheelWalk::Declined => {}
                }
            }
            run
        })
    }));
    out.unwrap_or(0)
}

// ---- keyboard: key names in, one fixed Windows chord table applied here ----
// (the client leaves the per-OS keymap to the host; keeping it in the
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
        Some(EditAction::Move {
            unit,
            back,
            extend: shift,
        })
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
        ("BACKSPACE", false) => Some(EditAction::Delete {
            unit: EditUnit::Char,
            back: true,
        }),
        ("BACKSPACE", true) => Some(EditAction::Delete {
            unit: EditUnit::Word,
            back: true,
        }),
        ("DELETE", false) => Some(EditAction::Delete {
            unit: EditUnit::Char,
            back: false,
        }),
        ("DELETE", true) => Some(EditAction::Delete {
            unit: EditUnit::Word,
            back: false,
        }),
        ("A", true) => Some(EditAction::SelectAll),
        ("U", true) => Some(EditAction::DeleteToEdge { back: true }),
        ("K", true) => Some(EditAction::DeleteToEdge { back: false }),
        ("P", true) => Some(EditAction::History { older: true }),
        ("N", true) => Some(EditAction::History { older: false }),
        _ => None,
    }
}

// The keys a binding press latched, with the command each runs on release. The
// latch names a command of the VM that made it, so a reload clears it (the
// reference keeps a held key running through `ReloadUI`; here it re-latches on
// its next press).
thread_local! {
    static BN_LATCH: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// The chord string a press makes: 1.12's `[ALT-][CTRL-][SHIFT-]<TOKEN>` prefix
/// order (`Blizzard_BindingUI.lua:176-182`), the token upper-cased as the table
/// stores letters.
fn chord_of(name: &str, mods: u32) -> String {
    let mut out = String::new();
    if mods & 4 != 0 {
        out.push_str("ALT-");
    }
    if mods & 2 != 0 {
        out.push_str("CTRL-");
    }
    if mods & 1 != 0 {
        out.push_str("SHIFT-");
    }
    out.push_str(&name.to_ascii_uppercase());
    out
}

/// The one retry after an exact chord misses: the leftmost modifier dropped
/// (ALT, then CTRL, then SHIFT) (`0x4b7990`), never a second time.
fn chord_fallback(chord: &str) -> Option<&str> {
    ["ALT-", "CTRL-", "SHIFT-"]
        .iter()
        .find_map(|p| chord.strip_prefix(p))
}

/// The command a press runs: its chord's, else the fallback chord's.
fn resolve_binding(script: &benilla_ui::script::UiScript, name: &str, mods: u32) -> Option<String> {
    let chord = chord_of(name, mods);
    let keys = script.binding_keys();
    if let Some((_, command)) = keys.iter().find(|(key, _)| *key == chord) {
        return Some(command.clone());
    }
    let fallback = chord_fallback(&chord)?;
    keys.iter()
        .find(|(key, _)| key.as_str() == fallback)
        .map(|(_, command)| command.clone())
}

/// The client's ENTER hotkey, in FrameXML's own words: focus the chat box the
/// user last used, which is `DEFAULT_CHAT_FRAME.editBox` when no other chat box
/// holds a draft. A missing helper (an older or stripped chain) falls back to
/// that box directly.
fn focus_chat_box(script: &mut benilla_ui::script::UiScript) {
    use benilla_ui::mlua::Value;
    let _ = script.eval::<Value>(
        "if ChatEdit_FocusActiveWindow then \
             ChatEdit_FocusActiveWindow() \
         elseif DEFAULT_CHAT_FRAME and DEFAULT_CHAT_FRAME.editBox then \
             DEFAULT_CHAT_FRAME.editBox:SetFocus() \
         end",
    );
}

/// Press or release a key by WoW name ("A", "ENTER", "LEFT", ...). `mods` is
/// the chord state (bit0 Shift, bit1 Ctrl, bit2 Alt; for a modifier's own
/// event include its own bit — the state after the event), `down` 1 = press.
/// A press runs the key-down walk and, when it lands on an EditBox, our keymap
/// applies the key's editing action (the client's host job); a press the walk
/// declines runs the world frame's binding here (the middle layer keeps the
/// chord table, leaving the UE side a pure event forwarder). Returns 1 when a
/// press ran a binding, the KeyWalk of a press otherwise (0 declined / 1 frame
/// / 2 box), 1/0 taken for a release, 0 with no session.
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
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
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
                if walk != benilla_ui::script::KeyWalk::Declined {
                    return match walk {
                        benilla_ui::script::KeyWalk::Frame => 1,
                        _ => 2,
                    };
                }
                // A focused box owns the keyboard; an alt-arrow box leaves the
                // arrows through, and every box declines PRINTSCREEN (`0x77b17f`).
                let token = name.to_ascii_uppercase();
                let arrow = matches!(token.as_str(), "LEFT" | "RIGHT" | "UP" | "DOWN");
                let print = token == "PRINTSCREEN";
                if script.has_keyboard_focus()
                    && !print
                    && !(arrow && script.editbox_alt_arrow_mode())
                {
                    return 0;
                }
                // A press for a key already latched is a repeat.
                if BN_LATCH.with(|l| l.borrow().iter().any(|(k, _)| k == name)) {
                    return 0;
                }
                let Some(command) = resolve_binding(script, name, mods) else {
                    // ENTER carries no binding in 1.12's Bindings.xml: the client
                    // handles it in its own key arm, which focuses the active chat
                    // box. The stock box then owns typing and sends on its own
                    // Enter (`ChatEdit_SendText`), so chat needs no host-side line.
                    if token == "ENTER" && mods == 0 {
                        focus_chat_box(script);
                        return 1;
                    }
                    return 0;
                };
                // The world frame's key-down ran it, so its key-up and repeats
                // are the world frame's (`0x483b70`).
                script.note_world_key(name);
                let _ = script.execute_binding(&command, true);
                BN_LATCH.with(|l| l.borrow_mut().push((name.to_owned(), command)));
                1
            } else {
                let latched = BN_LATCH.with(|l| {
                    let mut l = l.borrow_mut();
                    l.iter().position(|(k, _)| k == name).map(|i| l.remove(i))
                });
                if let Some((_, command)) = latched {
                    // The up-half runs before the release reaches a frame, in
                    // key order, and a focused box never eats a latched release.
                    let _ = script.execute_binding(&command, false);
                    let _ = script.key_up(name);
                    return 1;
                }
                let up = script.key_up(name);
                // An up alone (PRINTSCREEN on Windows, `VK_SNAPSHOT`) whose walk
                // declines runs the chord as a press, with no latch since no
                // release follows (`0x76603c`-`0x76604d`).
                if up == benilla_ui::script::KeyUp::Walked(benilla_ui::script::KeyWalk::Declined)
                    && !script.has_keyboard_focus()
                {
                    if let Some(command) = resolve_binding(script, name, mods) {
                        script.note_world_key(name);
                        let _ = script.execute_binding(&command, true);
                        return 1;
                    }
                }
                u32::from(up.taken())
            }
        })
    }));
    out.unwrap_or(0)
}

/// The window lost focus (WM_ACTIVATE's deactivate): the reference's `0x514490` clears the
/// movement direction bits and sweeps the held-key latching, so a key held across an alt-tab
/// never keeps the body walking or turning. Each latched key runs its up-half, as its release
/// would have, and the stock UI's movement axes go back to the host. Returns 1 with a session.
#[no_mangle]
pub extern "C" fn bn_ui_focus_lost() -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Releases run in latch order, before the feed's own clear, so a
        // command's `runOnUp` body fires exactly once.
        let latched: Vec<(String, String)> =
            BN_LATCH.with(|l| std::mem::take(&mut *l.borrow_mut()));
        for (key, command) in latched {
            BN_UI.with(|slot| {
                if let Some(script) = slot.borrow_mut().as_mut() {
                    let _ = script.execute_binding(&command, false);
                    let _ = script.key_up(&key);
                }
            });
        }
        ui_feed::focus_lost();
        1u32
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
/// time as the client's paste is. Returns 1 if consumed, 0 otherwise.
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
                            Value::Integer(n) => format!("{n}"),
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
                    note_vm_error(format!("eval: {e}"));
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

// ---- VM state push: feed wire snapshots into benilla's own FrameXML dialogs ----

/// Push the open gossip menu into the VM's `UiScript` so benilla's stock
/// `GossipFrame` shows and populates (the host then paints it via `bn_ui_dump`
/// as it already does). `greeting` is the NPC's text; `quest_titles`/`quest_levels`
/// are parallel arrays of quest rows from `SMSG_GOSSIP_MESSAGE`; `opt_labels`/`opt_icons`
/// are the gossip option pairs. All NUL-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn bn_ui_push_gossip(
    greeting: *const c_char,
    quest_titles: *const *const c_char,
    quest_levels: *const u32,
    n_quests: u32,
    opt_labels: *const *const c_char,
    opt_icons: *const *const c_char,
    n_opts: u32,
) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            let cs = |p: *const c_char| {
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(p) }
                        .to_string_lossy()
                        .into_owned()
                }
            };
            let greeting_s = cs(greeting);
            // An all-empty push clears the open menu (the host's close signal).
            if greeting_s.is_empty() && n_quests == 0 && n_opts == 0 {
                script.set_gossip(None);
                return 1;
            }
            let mut quests = Vec::new();
            for i in 0..n_quests as usize {
                let title = unsafe { cs(*quest_titles.add(i)) };
                let level = unsafe { *quest_levels.add(i) };
                quests.push(GossipQuestRow {
                    title,
                    level,
                    active: false,
                });
            }
            let mut options = Vec::new();
            for i in 0..n_opts as usize {
                let label = unsafe { cs(*opt_labels.add(i)) };
                let icon = unsafe { cs(*opt_icons.add(i)) };
                options.push(GossipOptionView {
                    label,
                    icon_type: icon,
                });
            }
            let menu = GossipMenu {
                greeting: greeting_s,
                quests,
                options,
            };
            script.set_gossip(Some(menu));
            script.fire_event("GOSSIP_SHOW", Vec::new());
            1
        })
    }));
    out.unwrap_or(0)
}

/// Push the open questgiver panel into the VM's `UiScript` so benilla's stock
/// `QuestFrame` shows. `panel` is 0=greeting, 1=detail, 2=progress, 3=reward.
/// All text fields are NUL-terminated UTF-8.
#[no_mangle]
pub extern "C" fn bn_ui_push_quest(
    panel: u32,
    title: *const c_char,
    body: *const c_char,
    objectives: *const c_char,
    money: i32,
    can_complete: u32,
) -> u32 {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BN_UI.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(script) = slot.as_mut() else {
                return 0u32;
            };
            let cs = |p: *const c_char| {
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(p) }
                        .to_string_lossy()
                        .into_owned()
                }
            };
            let p = match panel {
                0 => QuestPanel::Greeting,
                1 => QuestPanel::Detail,
                2 => QuestPanel::Progress,
                _ => QuestPanel::Reward,
            };
            let title_s = cs(title);
            let body_s = cs(body);
            let objectives_s = cs(objectives);
            // An all-empty push clears the open panel.
            if title_s.is_empty() && body_s.is_empty() && objectives_s.is_empty() {
                script.set_quest(None);
                return 1;
            }
            let state = QuestState {
                panel: p,
                title: title_s,
                body: body_s,
                objectives: objectives_s,
                reward_money: money as u32,
                completable: can_complete != 0,
                ..QuestState::default()
            };
            script.set_quest(Some(state));
            let event = match p {
                QuestPanel::Greeting => "QUEST_GREETING",
                QuestPanel::Detail => "QUEST_DETAIL",
                QuestPanel::Progress => "QUEST_PROGRESS",
                QuestPanel::Reward => "QUEST_COMPLETE",
            };
            script.fire_event(event, Vec::new());
            1
        })
    }));
    out.unwrap_or(0)
}

/// Close the open vendor list: the host's Escape, and `CloseMerchant()` (`0x4fadf0`) for
/// anything that asks the VM. The middle layer's stock window goes, so the feed's next pass
/// clears the VM's and fires `MERCHANT_CLOSED` - and a bag click stops reading as a sale.
/// Returns 1 when a list was open.
#[no_mangle]
pub extern "C" fn bn_net_vendor_close() -> u32 {
    let opened = net::ui_vendor().guid != 0;
    net::ui_vendor_closed();
    u32::from(opened)
}

// ---- glue booth ----

/// The login / character-screen backdrop booth (the glue booth):
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
    /// `GetFacing` yaw (radians), `GetModelScale`, `GetPosition`.
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
    /// The scene's stock fog, mirroring glue_booth `stock_fog`
    /// (AccountLogin.xml `<FogColor>`/`fogFar`, GlueParent.lua `CharModelFogInfo`).
    pub fog_rgb: [f32; 3],
    pub fog_near: f32,
    pub fog_far: f32,
    /// The glue stage's standing character: 0 none, 1 the select screen's
    /// selected roster row, 2 the create screen's dress-up look.
    pub stand_src: u8,
    /// Appearance bytes (select: the roster row's CharEnum fields; create: the
    /// create dials).
    pub stand_race: u8,
    pub stand_sex: u8,
    pub stand_skin: u8,
    pub stand_face: u8,
    pub stand_hair: u8,
    pub stand_hair_color: u8,
    pub stand_facial: u8,
    /// Body displays in the CharEnum order: shirt, chest, waist, legs, feet,
    /// wrist, hands, tabard (0 = bare).
    pub stand_gear: [u32; 8],
    /// The stage spot (attachment 0) in raw WoW model space.
    pub stand_pos: [f32; 3],
    /// The body's in-scene yaw (radians, WoW +Z): the stage bone chain's parked
    /// yaw plus the screen's facing.
    pub stand_yaw: f32,
    /// The stage bone chain's parked uniform scale.
    pub stand_scale: f32,
}

/// The glue stage a body stands on: `attachment 0`'s point plus its bone
/// chain's parked yaw and uniform scale (`glue_booth`'s `attachment_point` +
/// `stage_frame`). `attachment_point` reduces to the raw record position
/// because the pivot chain telescopes.
fn stage_seat(m2: &[u8]) -> Option<([f32; 3], f32, f32)> {
    let att = benilla_formats::parse_m2_attachments(m2).ok()?;
    let a = att.iter().find(|a| a.id == 0)?;
    let skel = benilla_formats::parse_m2_skeleton(m2).ok()?;
    let gseq = benilla_formats::parse_m2_global_sequence_bones(m2);
    let mut yaw = 0.0f32;
    let mut scale = 1.0f32;
    let mut at = Some(usize::from(a.bone));
    for _ in 0..=skel.bones.len() {
        let Some(i) = at else { break };
        if let Some(g) = gseq.iter().find(|g| usize::from(g.bone) == i) {
            if let Some(q) = g.rotation.as_ref().and_then(|c| parked(&c.keys)) {
                yaw += 2.0 * q[2].atan2(q[3]);
            }
            if let Some(s) = g.scale.as_ref().and_then(|c| parked(&c.keys)) {
                scale *= s[0];
            }
        }
        at = usize::try_from(skel.bones.get(i)?.parent).ok();
    }
    Some((a.position, yaw, scale))
}

/// One M2's authored camera record, the round portrait's view: benilla-app
/// draws `cameraLookup[0]` verbatim (`portrait::framing::frame`, `0x713540`).
/// Raw **WoW model space**, like `BnBooth::cam_eye` - the host maps
/// (x, -y, z) × 100 exactly as it maps the model's own vertices.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BnModelCamera {
    pub eye: [f32; 3],
    pub target: [f32; 3],
    pub roll: f32,
    /// Diagonal radians; 0 = the model carries no such record.
    pub fov: f32,
    pub near: f32,
    pub far: f32,
}

/// A model's authored portrait camera: `cameraLookup[0]` (`0x713540`), the
/// record benilla-app's `portrait::framing::frame` draws a round portrait
/// through (`0x524f60`). Returns 1 when a record was written, 0 for a missing
/// model or a model with no camera. Cached per path: the M2 is up to a
/// megabyte decompressed and archives are immutable.
#[no_mangle]
pub extern "C" fn bn_model_portrait_camera(
    world: *mut BnWorld,
    path: *const c_char,
    out: *mut BnModelCamera,
) -> u32 {
    if world.is_null() || path.is_null() || out.is_null() {
        return 0;
    }
    let Some(path) = cstr_to_str(path) else {
        return 0;
    };
    // SAFETY: the caller passes the handle `bn_init` returned; `m2_bytes`
    // only reads the archives behind it.
    let w = unsafe { &*world };
    thread_local! {
        static CAM_CACHE: std::cell::RefCell<HashMap<String, Option<[f32; 10]>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let key = path.to_string();
    let cam = CAM_CACHE.with(|cell| {
        let mut cell = cell.borrow_mut();
        if let Some(hit) = cell.get(&key) {
            return *hit;
        }
        let v = m2_bytes(w, path)
            .and_then(|b| benilla_formats::parse_m2_portrait_camera(&b))
            .map(|c| {
                [
                    c.position[0],
                    c.position[1],
                    c.position[2],
                    c.target[0],
                    c.target[1],
                    c.target[2],
                    c.roll,
                    c.fov,
                    c.near_clip,
                    c.far_clip,
                ]
            });
        cell.insert(key, v);
        v
    });
    let Some(c) = cam else {
        return 0;
    };
    // SAFETY: `out` was checked non-null and points at a caller-owned struct.
    unsafe {
        *out = BnModelCamera {
            eye: [c[0], c[1], c[2]],
            target: [c[3], c[4], c[5]],
            roll: c[6],
            fov: c[7],
            near: c[8],
            far: c[9],
        };
    }
    1
}

/// A global-sequence channel's parked key: the stage bones sit at t = 0 and
/// never animate (the client samples t = 0).
fn parked<T: Copy>(keys: &[(u32, T)]) -> Option<T> {
    keys.iter()
        .find(|k| k.0 == 0)
        .or_else(|| keys.first())
        .map(|k| k.1)
}

/// `CharStartOutfit` InventoryType -> the `stand_gear` slot. Weapons, shields
/// and slots with no body model (head/shoulders/neck/back/fingers) are dropped.
fn start_outfit_slot(inv: u8) -> Option<usize> {
    Some(match inv {
        4 => 0,      // shirt
        5 | 20 => 1, // chest / robe
        6 => 2,      // waist
        7 => 3,      // legs
        8 => 4,      // feet
        9 => 5,      // wrist
        10 => 6,     // hands
        19 => 7,     // tabard
        _ => return None,
    })
}

/// What stands on the glue stage this poll.
#[derive(Default, Clone, Copy)]
struct StandLook {
    src: u8,
    race: u8,
    sex: u8,
    skin: u8,
    face: u8,
    hair: u8,
    hair_color: u8,
    facial: u8,
    gear: [u32; 8],
    facing: f32,
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

    // GlueXML names a.mdx stem, the archives hold.m2 only (booth_probe).
    let mut path = f[1].trim().to_string();
    if path
        .get(path.len().saturating_sub(4)..)
        .is_some_and(|e| e.eq_ignore_ascii_case(".mdx"))
    {
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

    // camera0 and the stage seat by model path: the booth M2 is ~866KB
    // decompressed, far too heavy to re-read from the MPQ on every poll
    // (files are immutable).
    #[derive(Clone, Copy)]
    struct SceneInfo {
        cam: Option<[f32; 10]>,
        seat: Option<([f32; 3], f32, f32)>,
    }
    thread_local! {
        static SCENE_CACHE: std::cell::RefCell<HashMap<String, Option<SceneInfo>>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let info = SCENE_CACHE.with(|cell| {
        let mut cell = cell.borrow_mut();
        if !cell.contains_key(&path) {
            let v = m2_bytes(w, &path).map(|b| SceneInfo {
                cam: benilla_formats::parse_m2_camera(&b, 0).map(|c| {
                    [
                        c.position[0],
                        c.position[1],
                        c.position[2],
                        c.target[0],
                        c.target[1],
                        c.target[2],
                        c.roll,
                        c.fov,
                        c.near_clip,
                        c.far_clip,
                    ]
                }),
                seat: stage_seat(&b),
            });
            cell.insert(path.clone(), v);
        }
        cell.get(&path).copied().flatten()
    });

    // What stands on the glue stage: the select screen's selected roster row or
    // the create screen's dress-up body. Login (ffx scene, no stage body) and
    // any other pane report `stand_src = 0`.
    let mut stand = StandLook::default();
    BN_UI.with(|slot| {
        let Ok(held) = slot.try_borrow() else { return };
        let Some(s) = held.as_ref() else { return };
        let Some(pane) = s.glue_scene() else { return };
        if pane.create {
            let Some(look) = s.glue_create_look() else {
                return;
            };
            stand = StandLook {
                src: 2,
                race: look.race,
                sex: look.sex,
                skin: look.skin,
                face: look.face,
                hair: look.hair_style,
                hair_color: look.hair_color,
                facial: look.facial_hair,
                gear: [0; 8],
                facing: s.glue_create_facing(),
            };
            // The create body wears its class's CharStartOutfit (glue_booth).
            let outfit = BN_MENU.with(|m| {
                m.borrow()
                    .catalogs
                    .create
                    .as_ref()
                    .map(|c| c.start_outfit(look.race, look.class, look.sex).to_vec())
                    .unwrap_or_default()
            });
            for it in outfit {
                if let Some(slot) = start_outfit_slot(it.inv_type) {
                    stand.gear[slot] = it.display_id;
                }
            }
        } else if pane.select {
            let Some(guid) = s.glue_standing_guid() else {
                return;
            };
            let row =
                net::with_chars(|rows| rows.iter().find(|r| r.guid == guid).copied()).flatten();
            let Some(row) = row else { return };
            stand = StandLook {
                src: 1,
                race: row.race,
                sex: row.gender,
                skin: row.skin,
                face: row.face,
                hair: row.hair_style,
                hair_color: row.hair_color,
                facial: row.facial_hair,
                // CharEnum body slots: shirt, chest, waist, legs, feet, wrist,
                // hands, tabard.
                gear: [
                    row.equip_display[3],
                    row.equip_display[4],
                    row.equip_display[5],
                    row.equip_display[6],
                    row.equip_display[7],
                    row.equip_display[8],
                    row.equip_display[9],
                    row.equip_display[18],
                ],
                facing: s.glue_select_cells().facing,
            };
        }
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
        if let Some(c) = info.and_then(|i| i.cam) {
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
        o.stand_src = if stand.src != 0 && stand.race != 0 && stand.sex <= 1 {
            stand.src
        } else {
            0
        };
        o.stand_race = stand.race;
        o.stand_sex = stand.sex;
        o.stand_skin = stand.skin;
        o.stand_face = stand.face;
        o.stand_hair = stand.hair;
        o.stand_hair_color = stand.hair_color;
        o.stand_facial = stand.facial;
        o.stand_gear = stand.gear;
        if let Some((pos, yaw, scale)) = info.and_then(|i| i.seat) {
            o.stand_pos = pos;
            o.stand_yaw = yaw + stand.facing;
            o.stand_scale = scale;
        } else {
            o.stand_src = 0;
        }
    }
    1
}

/// The zone atmosphere at a WoW-space position (`x`/`y`/`z` in yards) on
/// `map` (0 = Eastern Kingdoms, 1 = Kalimdor) at `time` half-minutes
/// (1440 = noon), exactly as `LightCatalog::sample` resolves it for the
/// client. Writes 26 floats:
/// [0] fog end (yd), [1] fog start as a fraction of the end,
/// [2..5] fog colour, [5..8] sun diffuse, [8..11] ambient,
/// [11..26] the five sky-gradient stops, zenith to horizon.
/// Returns the float count written, 0 when the tables or the row are missing.
#[no_mangle]
pub extern "C" fn bn_light_sample(
    world: *mut BnWorld,
    x: f32,
    y: f32,
    z: f32,
    map: u32,
    time: u32,
    out: *mut f32,
    cap: u32,
) -> u32 {
    const N: u32 = 26;
    if world.is_null() || out.is_null() || cap < N {
        return 0;
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    if w.lights.is_none() {
        let loaded = w
            .chain
            .as_mut()
            .and_then(|c| benilla_formats::LightCatalog::load(c).ok());
        w.lights = loaded;
    }
    let Some(cat) = w.lights.as_ref() else {
        return 0;
    };
    let a = cat.sample(map, [x, y, z], time, false);
    let mut vals = [0.0f32; N as usize];
    vals[0] = a.fog_end;
    vals[1] = a.fog_start_frac;
    vals[2..5].copy_from_slice(&a.fog_color);
    vals[5..8].copy_from_slice(&a.sun_diffuse);
    vals[8..11].copy_from_slice(&a.ambient);
    for (i, s) in a.sky.iter().enumerate() {
        vals[11 + i * 3..14 + i * 3].copy_from_slice(s);
    }
    // SAFETY: `out` holds at least `cap >= N` floats.
    unsafe { std::ptr::copy_nonoverlapping(vals.as_ptr(), out, N as usize) };
    N
}

/// One `Spell.dbc` display row as a TSV line:
/// `id|name|rank|icon|school|power_type|mana_cost|duration_index|range_index|`
/// `recovery_ms|category|category_recovery_ms|start_recovery_ms`
/// (`icon` is the BLP path without extension; empty rank for unranked rows).
/// Returns the byte count written, 0 when the row is missing.
#[no_mangle]
pub extern "C" fn bn_spell_info(
    world: *mut BnWorld,
    spell_id: u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || out.is_null() || cap == 0 || spell_id == 0 {
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
    let Some(cat) = w.spells.as_ref() else {
        return 0;
    };
    let Some(s) = cat.get(spell_id) else {
        return 0;
    };
    let line = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        s.id,
        s.name,
        s.rank.as_deref().unwrap_or(""),
        s.icon.as_deref().unwrap_or(""),
        s.school,
        s.power_type,
        s.mana_cost,
        s.duration_index,
        s.range_index,
        s.recovery_ms,
        s.category,
        s.category_recovery_ms,
        s.start_recovery_ms
    );
    // SAFETY: `out` holds at least `cap` bytes; the copy is NUL-terminated.
    let bytes = line.as_bytes();
    let room = (cap as usize).saturating_sub(1);
    let n = bytes.len().min(room);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, n);
        *out.add(n) = 0;
    }
    n as u32
}

/// The zone's water tint bands at a WoW-space position (`sRGB 0..1`): the same Light row
/// `bn_light_sample` resolves, but the swatch endpoints the ADT liquid pass uses
/// (`IB_RIVER_SHALLOW/DEEP` and `IB_OCEAN_SHALLOW/DEEP`, with their `LightParams` alphas).
/// Writes 16 floats: river shallow rgba, river deep rgba, ocean shallow rgba, ocean deep rgba.
/// Returns 16, 0 when the tables or the row are missing.
#[no_mangle]
pub extern "C" fn bn_light_water(
    world: *mut BnWorld,
    x: f32,
    y: f32,
    z: f32,
    map: u32,
    time: u32,
    out: *mut f32,
    cap: u32,
) -> u32 {
    const N: u32 = 16;
    if world.is_null() || out.is_null() || cap < N {
        return 0;
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    if w.lights.is_none() {
        let loaded = w
            .chain
            .as_mut()
            .and_then(|c| benilla_formats::LightCatalog::load(c).ok());
        w.lights = loaded;
    }
    let Some(cat) = w.lights.as_ref() else {
        return 0;
    };
    let a = cat.sample(map, [x, y, z], time, false);
    let mut vals = [0.0f32; N as usize];
    vals[0..3].copy_from_slice(&a.water_river[0]);
    vals[3] = a.water_river_alpha[0];
    vals[4..7].copy_from_slice(&a.water_river[1]);
    vals[7] = a.water_river_alpha[1];
    vals[8..11].copy_from_slice(&a.water_ocean[0]);
    vals[11] = a.water_ocean_alpha[0];
    vals[12..15].copy_from_slice(&a.water_ocean[1]);
    vals[15] = a.water_ocean_alpha[1];
    // SAFETY: `out` holds at least `cap >= N` floats.
    unsafe { std::ptr::copy_nonoverlapping(vals.as_ptr(), out, N as usize) };
    N
}

/// Resolve one minimap tile through `md5translate.trs`: writes the MPQ path stem
/// `textures\\Minimap\\<hash>` (NUL-terminated, the caller appends "`.BLP`") into
/// `buf` and returns its length; 0 when the tile has no authored art.
#[no_mangle]
pub extern "C" fn bn_minimap_tile_path(
    world: *mut BnWorld,
    dir: *const c_char,
    tx: u32,
    ty: u32,
    buf: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || buf.is_null() || cap == 0 {
        return 0;
    }
    let Some(dir) = cstr_to_str(dir) else {
        return 0;
    };
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    if w.minimap.is_none() {
        let loaded = w
            .chain
            .as_mut()
            .and_then(|c| benilla_formats::load_minimap_translate(c).ok());
        w.minimap = loaded;
    }
    let Some(translate) = w.minimap.as_ref() else {
        return 0;
    };
    let Some(hash) = translate.tile(dir, tx, ty) else {
        return 0;
    };
    let path = format!("textures\\Minimap\\{hash}");
    let bytes = path.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    // SAFETY: `buf` has `cap` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

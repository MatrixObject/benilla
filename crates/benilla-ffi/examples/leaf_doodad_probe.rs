// Which tile doodads could draw the "static leaf-like triangles" seen under a
// tree? Per tile, per unique model path: placement count, M2 rig header
// (verts / submesh / clips / bones / animatable), particle emitter count,
// submesh blend classes + textures, and the nearest placement's distance from
// the tile centre (where the host drops the pawn).
//
// Read-only probe: it touches the install and writes nothing.

extern "C" {
    fn bn_doodad_rig_info(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
        out: *mut u32,
        cap: u32,
    ) -> u32;
    fn bn_doodad_placement(world: *const benilla::BnWorld, i: u32, out: *mut f32) -> u32;
    fn bn_model_particle_group_count(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
    ) -> u32;
}

const KEYWORDS: [&str; 12] = [
    "tree", "canop", "leaf", "branch", "bush", "grass", "fern", "flower", "flora", "wisp",
    "particleemit", "trunk",
];

struct Entry {
    placements: u32,
    first: u32,
    /// Nearest placement to the tile centre (f32::MAX when none mapped).
    near: f32,
    wow: (f32, f32, f32),
}

fn cstr(buf: &[i8]) -> String {
    std::ffi::CStr::from_bytes_until_nul(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
    })
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default()
}

fn main() {
    // The whole probe runs inside one unsafe block.
    unsafe { run() }
}

unsafe fn run() {
    let args: Vec<String> = std::env::args().collect();
    let map = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "Azeroth".to_string());

    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let mapc = std::ffi::CString::new(map.clone()).unwrap();

    // The tiles to scan: the rest of the arguments, "tx tz" pairs, or a 3x3
    // neighbourhood around the map's default tile.
    let mut tiles: Vec<(i32, i32)> = Vec::new();
    let mut i = 2;
    while i + 1 < args.len() {
        let tx: i32 = args[i].parse().unwrap_or(30);
        let tz: i32 = args[i + 1].parse().unwrap_or(48);
        tiles.push((tx, tz));
        i += 2;
    }
    if tiles.is_empty() {
        let (cx, cz) = if map.eq_ignore_ascii_case("Kalimdor") {
            (27, 13)
        } else {
            (30, 48)
        };
        for tz in cz - 1..=cz + 1 {
            for tx in cx - 1..=cx + 1 {
                tiles.push((tx, tz));
            }
        }
    }

    let mut path_buf = vec![0i8; 512];
    let mut tex_buf = vec![0i8; 256];
    let mut m = [0f32; 16];
    for (tx, tz) in tiles {
        let chunks = benilla::bn_load_tile(w, mapc.as_ptr(), tx, tz);
        if chunks == 0 {
            println!("tile {tx}_{tz}: empty");
            continue;
        }
        let n = benilla::bn_doodad_count(w);
        let mut center = [0f32; 2];
        let has_center = benilla::bn_tile_center(w, center.as_mut_ptr()) == 2;

        let mut entries: std::collections::BTreeMap<String, Entry> = Default::default();
        for i in 0..n {
            if benilla::bn_doodad_model_path(w, i, path_buf.as_mut_ptr(), 512) == 0 {
                continue;
            }
            let path = cstr(&path_buf);
            let e = entries.entry(path).or_insert(Entry {
                placements: 0,
                first: i,
                near: f32::MAX,
                wow: (0.0, 0.0, 0.0),
            });
            e.placements += 1;
            if has_center && bn_doodad_placement(w, i, m.as_mut_ptr()) == 1 {
                let wx = center[0] + m[3];
                let wy = center[1] - m[7];
                let d = ((wx - center[0]).powi(2) + (wy - center[1]).powi(2)).sqrt();
                if d < e.near {
                    e.near = d;
                    e.wow = (wx, wy, m[11]);
                }
            }
        }

        println!("tile {tx}_{tz}: {chunks} chunks, {n} doodads, {} unique models", entries.len());
        for (path, e) in entries.iter() {
            let cp = std::ffi::CString::new(path.clone()).unwrap();
            let emitters = bn_model_particle_group_count(w, cp.as_ptr());
            let hit = KEYWORDS.iter().any(|k| path.contains(k));
            // Emitter-bearing models always print; the rest only when their
            // path smells like foliage / a tree.
            if emitters == 0 && !hit {
                continue;
            }

            let len = bn_doodad_rig_info(w, cp.as_ptr(), std::ptr::null_mut(), 0);
            let mut table = vec![0u32; len as usize];
            if len >= 12 {
                bn_doodad_rig_info(w, cp.as_ptr(), table.as_mut_ptr(), len);
            }
            let animated = len >= 12 && table[1] & 1 != 0;
            let subs = if len >= 12 { table[2] } else { 0 };
            let clips = if len >= 12 { table[3] } else { 0 };
            let verts = if len >= 12 { table[4] } else { 0 };
            let idx = if len >= 12 { table[5] } else { 0 };
            let bones = if len >= 12 { table[8] } else { 0 };

            // Bake the first instance so bn_doodad_blend/texture have flats.
            let loaded = benilla::bn_doodad_load(w, e.first);
            let mut blends: Vec<String> = Vec::new();
            for g in 0..loaded {
                let mut b = 0u32;
                if benilla::bn_doodad_blend(w, e.first, g, &mut b) == 0 {
                    continue;
                }
                let tn = benilla::bn_doodad_texture(w, e.first, g, tex_buf.as_mut_ptr(), 256);
                let tex = if tn == 0 { String::new() } else { cstr(&tex_buf) };
                if b != 0 || tex.contains("leaf") || tex.contains("tree") {
                    blends.push(format!("{g}:b{b}:{tex}"));
                }
            }

            let dist = if e.near == f32::MAX {
                "n/a".to_string()
            } else {
                format!("{:.1}", e.near)
            };
            let wow = if e.near == f32::MAX {
                "n/a".to_string()
            } else {
                format!("({:.1},{:.1},{:.1})", e.wow.0, e.wow.1, e.wow.2)
            };
            println!(
                "  model={path} placements={} verts={verts} idx={idx} subs={subs} clips={clips} anim={} bones={bones} emitters={emitters} near={dist}yd wow={wow} blend=[{}]",
                e.placements, animated as u8,
                blends.join("|")
            );
        }
    }

    benilla::bn_shutdown(w);
}

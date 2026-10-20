// Quantify the two u16 truncation risks before touching the render path:
//   1. doodad texture groups: UE's WMO/doodad builder remaps each texture
//      group into a u16 index space and `break`s at 65535 - a group summing
//      more verts across its placements silently loses geometry.
//   2. WMO instances: BnTriangle carries u16 vertex indices per instance, so
//      a single instance over 65535 verts truncates at the FFI itself.
// Part 1 walks EVERY .wmo in the MPQ chain (group vertex totals). Part 2
// finds doodad-bearing tiles near Stormwind (both filename orders, as
// doodad_probe does) and sums each tile's doodad groups by the UE key
// (texture + blend class + wrap; the anim split only makes real groups
// smaller, so these sums are upper bounds).
use benilla::*;
use benilla_mpq::Archive;
use benilla_wmo::ParsedWmo;
use std::collections::HashMap;
use std::io::Cursor;

fn cstr(buf: &[i8]) -> String {
    std::ffi::CStr::from_bytes_until_nul(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
    })
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default()
}

fn part1_wmo_files(data_dir: &str) {
    let mut files: Vec<String> = std::fs::read_dir(data_dir)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.to_lowercase().ends_with(".mpq"))
        .collect();
    files.sort();

    // The archives expose no file enumeration, so enumerate through the
    // in-archive (listfile). Root `X.wmo` holds no geometry; every
    // `X_NNN.wmo` group does, and the FFI bakes all groups of one root into
    // a single instance - so the u16 question is the per-root group sum.
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut roots: HashMap<String, u32> = HashMap::new();
    for mpq in &files {
        let Ok(a) = Archive::open(mpq) else { continue };
        let Ok(listfile) = a.read_file("(listfile)") else { continue };
        let listing = String::from_utf8_lossy(&listfile).into_owned();
        for name in listing.split(|c| c == '\n' || c == '\r') {
            let name = name.trim();
            let lower = name.to_ascii_lowercase();
            if !lower.ends_with(".wmo") || seen.contains_key(&lower) {
                continue;
            }
            seen.insert(lower.clone(), ());
            // `..._123.wmo` -> root key `..._123`; the plain root -> itself.
            let stem = &lower[..lower.len() - 4];
            let root = match stem.len().checked_sub(4) {
                Some(i)
                    if stem.as_bytes()[i] == b'_'
                        && stem[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
                {
                    stem[..i].to_string()
                }
                _ => stem.to_string(),
            };
            let Ok(bytes) = a.read_file(name) else { continue };
            let verts: u32 = match benilla_wmo::parse_wmo(&mut Cursor::new(bytes.as_slice())) {
                Ok(ParsedWmo::Group(g)) => g.vertex_positions.len() as u32,
                _ => 0,
            };
            *roots.entry(root).or_default() += verts;
        }
    }
    let mut over: Vec<(&String, &u32)> = roots.iter().filter(|(_, v)| **v >= 65535).collect();
    over.sort_by(|a, b| b.1.cmp(a.1));
    println!(
        "== wmo: {} files, {} roots, {} roots over u16 (>=65535 group verts)",
        seen.len(),
        roots.len(),
        over.len()
    );
    for (p, v) in over.iter().take(20) {
        println!("  {v} verts: {p}.wmo");
    }
}

fn part2_doodad_groups(w: *mut BnWorld) {
    let map = std::ffi::CString::new("Azeroth").unwrap();
    // Find doodad-bearing tiles: both filename orders around Stormwind,
    // then keep every tile that has any.
    let mut bearing: Vec<(i32, i32, u32)> = Vec::new();
    for order in 0..2 {
        for dx in -2..=2i32 {
            for dy in -2..=2i32 {
                let (a, b) = if order == 0 {
                    (48 + dx, 31 + dy)
                } else {
                    (31 + dx, 48 + dy)
                };
                if bearing.iter().any(|t| t.0 == a && t.1 == b) {
                    continue;
                }
                if bn_load_tile(w, map.as_ptr(), a, b) == 0 {
                    continue;
                }
                let n = bn_doodad_count(w);
                if n > 0 {
                    bearing.push((a, b, n));
                }
            }
        }
    }
    println!(
        "== doodad-bearing tiles: {} ({})",
        bearing.len(),
        bearing
            .iter()
            .map(|t| format!("{}_{}/{}", t.0, t.1, t.2))
            .collect::<Vec<_>>()
            .join(" ")
    );
    bearing.sort_by_key(|t| std::cmp::Reverse(t.2));

    for (a, b, n) in bearing.into_iter().take(4) {
        if bn_load_tile(w, map.as_ptr(), a, b) == 0 {
            continue;
        }
        let n2 = bn_doodad_count(w);
        let mut groups: HashMap<String, u64> = HashMap::new();
        let mut loaded = 0u32;
        for i in 0..n2 {
            let subs = bn_doodad_load(w, i);
            if subs == 0 {
                continue;
            }
            loaded += 1;
            for j in 0..subs {
                let mut r = [0u32; 4];
                if bn_doodad_submesh(w, i, j, r.as_mut_ptr()) == 0 {
                    continue;
                }
                let mut tbuf = [0i8; 512];
                let tex = if bn_doodad_texture(w, i, j, tbuf.as_mut_ptr(), tbuf.len() as u32) == 1
                {
                    cstr(&tbuf)
                } else {
                    String::new()
                };
                let mut blend = 0u32;
                bn_doodad_blend(w, i, j, &mut blend);
                let mut wrap = 0u32;
                bn_doodad_wrap(w, i, j, &mut wrap);
                let key = format!("{tex}#c{}#w{}", blend & 0xff, wrap);
                *groups.entry(key).or_default() += r[1] as u64;   // vtx_n
            }
        }
        let mut sorted: Vec<_> = groups.into_iter().collect();
        sorted.sort_by(|x, y| y.1.cmp(&x.1));
        let over: Vec<_> = sorted.iter().filter(|g| g.1 >= 65535).collect();
        let near: Vec<_> = sorted
            .iter()
            .filter(|g| g.1 >= 32768 && g.1 < 65535)
            .collect();
        println!(
            "Azeroth_{a}_{b}: doodads={n2} (loaded {loaded}), groups={} OVER={} near={}",
            sorted.len(),
            over.len(),
            near.len()
        );
        for (k, v) in sorted.iter().take(3) {
            println!("  top {v} verts: {k}");
        }
        for (k, v) in &over {
            println!("  OVER 65535: {v} verts: {k}");
        }
        for (k, v) in &near {
            println!("  near cap: {v} verts: {k}");
        }

        // Per-placement risk: a single model baked over 65535 verts would
        // also break BnTriangle's u16 indices.
        let mut worst = (0u32, 0u32);
        for i in 0..n2 {
            let nv = bn_doodad_vertex_count(w, i);
            if nv > worst.0 {
                worst = (nv, i);
            }
        }
        if worst.0 >= 65535 {
            println!("  PLACEMENT over u16: doodad {} verts={}", worst.1, worst.0);
        } else {
            println!("  max placement verts={} (< 65535 ok)", worst.0);
        }
    }

    // WMO instances per bearing/nearby tile (u16 per-instance risk).
    for (a, b, _) in [(29, 47, 0), (30, 47, 0), (31, 47, 0)] {
        if bn_load_tile(w, map.as_ptr(), a, b) == 0 {
            continue;
        }
        let wn = bn_wmo_instance_count(w);
        for wi in 0..wn {
            let nv = {
                bn_wmo_instance_load(w, wi);
                bn_wmo_instance_vertex_count(w, wi)
            };
            let mark = if nv >= 65535 { "  << OVER u16" } else { "" };
            println!("Azeroth_{a}_{b} wmo {wi}: verts={nv}{mark}");
        }
    }
}

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| {
        r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string()
    });
    part1_wmo_files(&data_dir);

    let dir = std::ffi::CString::new(data_dir).unwrap();
    let w = bn_init(dir.as_ptr());
    if w.is_null() {
        println!("bn_init failed");
        return;
    }
    part2_doodad_groups(w);
    bn_shutdown(w);
}

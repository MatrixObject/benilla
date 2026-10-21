// Which placed doodads of a tile have a bone rig, and how far are they from
// the tile's centre? (The tile centre is where the host drops the pawn.)
//
// Read-only probe: it touches the install and writes nothing.

// The rig exports are declared here (not reached through the crate root) so
// the probe also checks the exact C ABI the UE host binds.
extern "C" {
    fn bn_doodad_rig_info(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
        out: *mut u32,
        cap: u32,
    ) -> u32;
    fn bn_doodad_placement(world: *const benilla::BnWorld, i: u32, out: *mut f32) -> u32;
}

fn main() {
    // The whole probe runs inside one unsafe block.
    unsafe { run() }
}

unsafe fn run() {
    let args: Vec<String> = std::env::args().collect();
    let map = args.get(1).cloned().unwrap_or_else(|| "Azeroth".to_string());

    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let mapc = std::ffi::CString::new(map.clone()).unwrap();

    // The tiles to scan: the rest of the arguments, "tx tz" pairs, or the
    // default neighbourhood of the Stormwind tile.
    let mut tiles: Vec<(i32, i32)> = Vec::new();
    let mut i = 2;
    while i + 1 < args.len() {
        let tx: i32 = args[i].parse().unwrap_or(30);
        let tz: i32 = args[i + 1].parse().unwrap_or(48);
        tiles.push((tx, tz));
        i += 2;
    }
    if tiles.is_empty() {
        for tz in 28..=32 {
            for tx in 26..=32 {
                tiles.push((tx, tz));
            }
        }
    }

    let mut path_buf = vec![0i8; 512];
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
        // (distance from the tile centre, model, wow x, wow y, wow z)
        let mut found: Vec<(f32, String, f32, f32, f32)> = Vec::new();
        for i in 0..n {
            if benilla::bn_doodad_model_path(w, i, path_buf.as_mut_ptr(), 512) == 0 {
                continue;
            }
            let path = std::ffi::CStr::from_ptr(path_buf.as_ptr())
                .to_string_lossy()
                .to_string();
            let cpath = std::ffi::CString::new(path.clone()).unwrap();
            let len = bn_doodad_rig_info(w, cpath.as_ptr(), std::ptr::null_mut(), 0);
            if len < 12 {
                continue;
            }
            let mut table = vec![0u32; len as usize];
            bn_doodad_rig_info(w, cpath.as_ptr(), table.as_mut_ptr(), len);
            if table[1] & 1 == 0 {
                continue;
            }
            if bn_doodad_placement(w, i, m.as_mut_ptr()) != 1 {
                continue;
            }
            let wx = if has_center { center[0] + m[3] } else { m[3] };
            let wy = if has_center { center[1] - m[7] } else { -m[7] };
            let d = if has_center {
                ((wx - center[0]).powi(2) + (wy - center[1]).powi(2)).sqrt()
            } else {
                0.0
            };
            found.push((d, path, wx, wy, m[11]));
        }
        found.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        println!(
            "tile {tx}_{tz}: {chunks} chunks, {n} doodads, {} animated",
            found.len()
        );
        if has_center {
            let h = benilla::bn_terrain_height_at(w, center[0], center[1]);
            println!("   terrain height at the centre: {h:.2}");
        }
        for (d, path, wx, wy, wz) in found.iter().take(6) {
            let h = benilla::bn_terrain_height_at(w, *wx, *wy);
            println!(
                "   {d:6.1} yd from centre  {path}  wow=({wx:.1}, {wy:.1}, {wz:.1}) ground={h:.2}"
            );
        }
    }

    benilla::bn_shutdown(w);
}

// Verify the cross-tile straddle dedup premise: a doodad/WMO that spans a
// tile edge appears in BOTH adjacent tiles' MDDF/MODF under the SAME
// unique_id (the reference dedups by it at 0x694bc0). Prints the uid
// intersection of two adjacent tiles; non-empty means the duplicates the
// host-side claim registry suppresses are real.

use std::collections::HashSet;

fn main() {
    let dir = std::ffi::CString::new(
        "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data",
    )
    .unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let map = std::ffi::CString::new("Azeroth").unwrap();

    // Horizontally adjacent tiles (shared vertical edge) plus a diagonal
    // pair (shared corner) to cover both straddle shapes.
    let pairs = [((30i32, 48i32), (31, 48)), ((30, 48), (30, 47))];

    for ((ax, az), (bx, bz)) in pairs {
        let mut doodad_sets: Vec<HashSet<u32>> = Vec::new();
        let mut wmo_sets: Vec<HashSet<u32>> = Vec::new();
        for &(x, z) in &[(ax, az), (bx, bz)] {
            if benilla::bn_load_tile(w, map.as_ptr(), x, z) == 0 {
                eprintln!("tile {x},{z}: load failed");
                std::process::exit(1);
            }
            let mut du = HashSet::new();
            for i in 0..benilla::bn_doodad_count(w) {
                let mut uid = 0u32;
                if benilla::bn_doodad_unique_id(w, i, &mut uid) == 1 {
                    du.insert(uid);
                }
            }
            let mut wu = HashSet::new();
            for i in 0..benilla::bn_wmo_instance_count(w) {
                let mut uid = 0u32;
                if benilla::bn_wmo_unique_id(w, i, &mut uid) == 1 {
                    wu.insert(uid);
                }
            }
            println!(
                "tile ({x},{z}): {} doodad uids, {} wmo uids",
                du.len(),
                wu.len()
            );
            doodad_sets.push(du);
            wmo_sets.push(wu);
        }
        let dd: Vec<_> = doodad_sets[0].intersection(&doodad_sets[1]).collect();
        let ww: Vec<_> = wmo_sets[0].intersection(&wmo_sets[1]).collect();
        println!(
            "({ax},{az}) vs ({bx},{bz}): shared doodad uids = {}, shared wmo uids = {}",
            dd.len(),
            ww.len()
        );
        for u in dd.iter().take(5) {
            println!("  shared doodad uid {u}");
        }
        for u in ww.iter().take(5) {
            println!("  shared wmo uid {u}");
        }
        println!();
    }

    benilla::bn_shutdown(w);
}

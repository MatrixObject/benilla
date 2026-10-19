// Find the real 1.12.1 ADT paths by probing many map names + coords against
// terrain.MPQ (where 1.12.1 stores terrain). Use the WDT->ADT scheme
// `World\Maps\<dir>\<dir>_<x>_<y>.adt`.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";

    let dirs: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    // 1.12.1 outdoor zones (dir names, lowercase as WoW stores them).
    let map_dirs = [
        "Durotar",
        "Stranglethorn",
        "Stormwind",
        "Stonardel",
        "Ironwood",
        "Elfin",
        "Hills",
        "Alterac",
        "Locthane",
        "Deeppinewaters",
        "BootyBay",
        "Gnomeregan",
        "Scholomance",
        "Darkmoon",
        "Molgor",
        "Azshara",
        "Theramore",
        "Tanaris",
    ];

    let mut any = 0;
    for d in &dirs {
        let p = format!("{data}\\{d}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for m in &map_dirs {
            let low = m.to_ascii_lowercase();
            // WoW 1.12.1 ADT: <mapdir>_<x>_<y>.adt, 0-based, usually 0..9 grid.
            for x in 0..10 {
                for y in 0..10 {
                    let path = format!("World\\Maps\\{low}\\{low}_{x}_{y}.adt");
                    if a.contains(&path) {
                        any += 1;
                        let sz = a.file_size(&path).unwrap_or(0);
                        if any <= 20 {
                            println!("{d}: {path} ({sz} bytes)");
                        }
                    }
                }
            }
        }
    }
    println!("total adt hits: {any}");
}

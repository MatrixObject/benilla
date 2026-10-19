// Probe which WDT files exist (each WoW map has <dir>.wdt), to learn the real
// 1.12.1 map directory names. Try a known set of 1.12.1 maps.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    // 1.12.1 maps to probe. WoW 1.12.1 map dir names (case as stored in MPQ).
    let map_dirs = [
        "durotar",
        "stranglethorn",
        "stonedale",
        "stormwind",
        "ironwood",
        "dunmorowne",
        "eldorado",
    ];

    let mut any_wdt = false;
    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for d in &map_dirs {
            let wdt = format!("world\\maps\\{d}\\{d}.wdt");
            if a.contains(&wdt) {
                any_wdt = true;
                println!("FOUND wdt {wdt} in {f}");
            }
        }
    }
    if !any_wdt {
        println!("No wdt found under world\\maps\\<dir> — probing raw adt in terrain.MPQ...");
        // Fallback: list adt-ish paths in terrain.MPQ by trying coord ranges.
        let p = format!("{data}\\terrain.MPQ");
        if let Ok(a) = benilla_mpq::Archive::open(&p) {
            for d in &map_dirs {
                for x in 0..3 {
                    for z in 0..3 {
                        let adt = format!("world\\maps\\{d}\\{d}_{x}_{z}.adt");
                        if a.contains(&adt) {
                            println!("terrain.MPQ has {adt}");
                        }
                    }
                }
            }
        }
    }
}

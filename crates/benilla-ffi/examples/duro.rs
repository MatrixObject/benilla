// Probe Durotar ADT with correct 1.12.1 casing. WoW MPQ stores the dir
// name with the Map.dbc "Directory" value's casing (usually "Durotar").
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    // Try both casings of the dir, several coord ranges, across all MPQs.
    let candidates = [
        "Durotar",
        "durotar",
        "Stranglethorn",
        "stranglethorn",
        "Stormwind",
        "stormwind",
        "Alterac",
        "alterac",
    ];
    let mut hits = 0;
    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for c in &candidates {
            for x in 0..8 {
                for y in 0..8 {
                    // WoW 1.12.1 terrain: World\Maps\<Dir>\<Dir>_<x>_<y>.adt
                    let pth = format!("World\\Maps\\{c}\\{c}_{x}_{y}.adt");
                    if a.contains(&pth) {
                        hits += 1;
                        let sz = a.file_size(&pth).unwrap_or(0);
                        if hits <= 15 {
                            println!("{f}: {pth} ({sz} bytes)");
                        }
                    }
                }
            }
        }
    }
    println!("total hits: {hits}");
}

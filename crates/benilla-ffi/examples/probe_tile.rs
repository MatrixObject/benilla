// Find existing ADT tiles: vanilla tile coords are GLOBAL 0..63 (world_to_tile),
// so probe that range with Map.dbc casing ("Durotar", not "durotar").
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let maps = [
        "Durotar",
        "Stranglethorn",
        "Stormwind",
        "Alterac",
        "Azshara",
    ];
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();
    let mut hits = 0;
    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for m in maps {
            for x in 0..64u32 {
                for y in 0..64u32 {
                    let path = format!("World\\Maps\\{m}\\{m}_{x}_{y}.adt");
                    if a.contains(&path) {
                        hits += 1;
                        if hits <= 30 {
                            println!("{f}: {path} ({} bytes)", a.file_size(&path).unwrap_or(0));
                        }
                    }
                }
            }
        }
    }
    println!("total adt hits: {hits}");
}

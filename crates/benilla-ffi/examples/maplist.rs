// Enumerate real ADT/WDT filenames in the 1.12.1 chain.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect::<Vec<String>>();
    let maps = [
        "Durotar", "Stonardel", "Stranglethorn", "Westplains", "Deadwind",
        "Arathor", "Elwynn", "Lothetown", "Lakewater", "Alterac",
        "Barrens", "Tirisfal", "Mulgore",
    ];
    let mut found = 0u32;
    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for m in &maps {
            let lower = m.to_ascii_lowercase();
            for x in [0, 1, 4] {
                for y in [0, 1, 4] {
                    let adt = format!("World\\Maps\\{lower}\\{lower}_{x}_{y}.adt");
                    if a.contains(&adt) {
                        found += 1;
                        println!("{f}: {adt} ({} bytes)", a.file_size(&adt).unwrap_or(0));
                    }
                    let wdt = format!("World\\Maps\\{lower}\\{lower}.wdt");
                    if a.contains(&wdt) {
                        found += 1;
                        println!("{f}: {wdt}");
                    }
                }
            }
        }
    }
    println!("--- total hits: {found} ---");
    if found == 0 {
        println!("NONE. Map dir/casing differs from these guesses.");
    }
}

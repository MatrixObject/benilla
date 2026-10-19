// Diagnose why ADT probes find nothing: read each MPQ's (listfile) and grep
// .adt/.wdt, and probe WDT paths directly. Also list root MPQs that open.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();
    println!("root mpqs: {files:?}");
    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(e) => {
                println!("{f}: OPEN FAILED {e}");
                continue;
            }
        };
        match a.read_file("(listfile)") {
            Ok(lf) => {
                let txt = String::from_utf8_lossy(&lf);
                let adts: Vec<&str> = txt
                    .lines()
                    .filter(|l| l.to_lowercase().ends_with(".adt"))
                    .collect();
                println!("{f}: listfile {} lines, {} adt entries", txt.lines().count(), adts.len());
                for a in adts.iter().take(6) {
                    println!("    {a}");
                }
            }
            Err(e) => println!("{f}: no listfile ({e})"),
        }
    }
    // Direct WDT probes with several casings.
    let probes = [
        "World\\Maps\\Durotar\\Durotar.wdt",
        "World\\Maps\\durotar\\durotar.wdt",
        "World\\Maps\\Kalimdor\\Kalimdor.wdt",
        "World\\Maps\\Azeroth\\Azeroth.wdt",
    ];
    for f in &files {
        let p = format!("{data}\\{f}");
        let Ok(a) = benilla_mpq::Archive::open(&p) else { continue };
        for pr in probes {
            if a.contains(pr) {
                println!("{f}: FOUND {pr}");
            }
        }
    }
}

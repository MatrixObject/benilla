// Probe the 1.12.1 MPQ chain with *real* asset paths copied from benilla's
// own source (these are proven to exist in a 1.12.1 build). Confirms which
// MPQs actually hold what, so the FFI renderer can target a real file.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    // Real 1.12.1 paths (case + slashes as WoW stores them in MPQs).
    let probes = [
        "Interface\\Glues\\Common\\Glues-WoW-Logo.blp",
        "Interface\\Icons\\INV_Misc_QuestionMark.blp",
        "Interface\\Buttons\\TalkToMe.m2",
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Model\\M2\\Human\\Male\\M2Male_00.M2",
        "Model\\M2\\Human\\Male\\M2Male_00.m2",
        "World\\Maps\\Durotar\\Durotar.wdt",
        "World\\Maps\\durotar\\durotar.wdt",
        "World\\Maps\\Durotar\\Durotar_0_0.adt",
        "World\\Maps\\Durotar\\Durotar_1_1.adt",
        "World\\KhazModan\\Ironforge\\PassiveDoodads\\Trees\\IronForgeleaves01.blp",
        "WMO\\WmoTemplates\\Stone.blp",
        "WMO\\BLP\\Stone.blp",
        "WMO\\Wmo.blp",
    ];

    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(e) => {
                println!("{f}: open failed ({e})");
                continue;
            }
        };
        let hits: Vec<&str> = probes
            .iter()
            .filter(|pp| a.contains(*pp))
            .cloned()
            .collect();
        if !hits.is_empty() {
            println!("{f}:");
            for h in &hits {
                let sz = a.file_size(*h).unwrap_or(0);
                println!("    {h} ({sz} bytes)");
            }
        }
    }
}

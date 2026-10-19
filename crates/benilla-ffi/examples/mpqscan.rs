// Open each 1.12.1 MPQ in the Data dir and find which one holds Durotar terrain.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";

    // All MPQs in the dir (case-insensitive, the 1.12.1 chain).
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();
    println!("found {} MPQs: {files:#?}", files.len());

    // Probe paths for a Durotar terrain tile (lowercase, WoW convention).
    let probes = [
        "world\\maps\\durotar\\durotar_0_0.adt",
        "world\\maps\\durotar\\durotar_1_1.adt",
        "world\\maps\\durotar\\durotar_2_2.adt",
    ];

    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(e) => {
                println!("{f}: OPEN FAILED ({e})");
                continue;
            }
        };
        // Report which probe paths this archive contains.
        let hits: Vec<&str> = probes
            .iter()
            .filter(|pp| a.contains(pp))
            .cloned()
            .collect();
        if !hits.is_empty() {
            println!("{f}: CONTAINS {hits:?}");
        } else {
            println!("{f}: opened, none of the Durotar probes present");
        }
    }
}

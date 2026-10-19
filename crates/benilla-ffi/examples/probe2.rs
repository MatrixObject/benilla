// Confirm the MPQ reader actually finds known 1.12.1 files (proving the
// reader works), then learn the real ADT dir by trying every Durotar-ish
// casing and also listing what textures are present.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    // 1. First prove the reader finds a known texture (Texture.MPQ holds .blp).
    let tex = format!("{data}\\texture.MPQ");
    if let Ok(a) = benilla_mpq::Archive::open(&tex) {
        for probe in [
            "blp\\grass\\grass00.blp",
            "terrain\\blp\\grass\\grass00.blp",
            "blp\\ground\\grass00.blp",
            "wmo\\blp\\0600.blp",
            "model\\blp\\m2male_00.blp",
        ] {
            let c = a.contains(probe);
            println!("texture.MPQ contains {probe} = {c}");
        }
    }

    // 2. Now try Durotar ADT across casings in terrain.MPQ + patch.MPQ.
    for mpq in ["terrain.MPQ", "patch.MPQ", "patch-2.MPQ", "base.MPQ"] {
        let p = format!("{data}\\{mpq}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for dir in ["Durotar", "durotar", "0_Durotar", "0_durotar"] {
            for x in 0..8 {
                for y in 0..8 {
                    let q = format!("World\\Maps\\{dir}\\{dir}_{x}_{y}.adt");
                    if a.contains(&q) {
                        let sz = a.file_size(&q).unwrap_or(0);
                        println!("HIT {mpq}: {q} ({sz} bytes)");
                    }
                }
            }
        }
    }
    println!("probe done");
}

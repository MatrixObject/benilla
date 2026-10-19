// Find what file paths actually exist in base.MPQ by trying many casings of
// a known-good 1.12.1 file (terrain.BLP and a tile). Also probe the real map dir.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let base = format!("{data}\\base.MPQ");
    let a = match benilla_mpq::Archive::open(&base) {
        Ok(a) => a,
        Err(e) => {
            println!("open {base} failed: {e}");
            return;
        }
    };

    // 1.12.1 known paths (WoW stores MPQ names case-SENSITIVE, usually all-lowercase).
    let probes = [
        "world\\maps\\durotar\\durotar_0_0.adt",
        "world\\maps\\durotar\\durotar_1_1.adt",
        "world\\maps\\durotar\\durotar_1_0.adt",
        "world\\maps\\durotar\\durotar_0_1.adt",
        "durotar\\durotar_1_1.adt",
        "world\\maps\\stranglethorn\\stranglethorn_1_1.adt",
        "terrain\\blp\\ground\\grass.blp",
        "terrain\\blp\\ground\\grass00.blp",
        "blp\\ground\\grass.blp",
        "wmo\\0600.blp",
        "model\\m2\\human\\male\\m2male_00.mdl",
    ];
    for p in probes {
        let c = a.contains(p);
        let sz = a.file_size(p).map(|s| s).unwrap_or(0);
        println!("{p}: contains={c} size={sz}");
    }

    // Also try opening_file to get the real error for a likely path.
    for p in ["world\\maps\\durotar\\durotar_0_0.adt", "terrain\\blp\\ground\\grass.blp"] {
        match a.read_file(p) {
            Ok(b) => println!("READ OK {p}: {} bytes", b.len()),
            Err(e) => println!("READ ERR {p}: {e}"),
        }
    }
}

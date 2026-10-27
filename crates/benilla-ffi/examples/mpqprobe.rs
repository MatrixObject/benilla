// Probe the actual path casing inside base.MPQ (1.12.1 stores names in a hash
// table; contains is case-sensitive here, so find what the real paths are).
fn main() {
    let base = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data\\base.MPQ";
    let a = match benilla_mpq::Archive::open(base) {
        Ok(a) => a,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };

    // Try casings of a known 1.12.1 Durotar tile path.
    let probes = [
        "world\\maps\\durotar\\durotar_0_0.adt",
        "World\\Maps\\Durotar\\Durotar_0_0.adt",
        "world\\maps\\durotar\\durotar_1_1.adt",
        "World\\Maps\\Durotar\\Durotar_1_1.adt",
        "durotar\\durotar_0_0.adt",
        "Durotar\\Durotar_0_0.adt",
        "terrain\\blp\\ground\\grass.blp",
        "Terrain\\BLP\\Ground\\grass.blp",
    ];
    for p in probes {
        let c = a.contains(p);
        let sz = a
            .file_size(p)
            .map(|s| format!(" size={s}"))
            .unwrap_or_default();
        println!("{p}: contains={c}{sz}");
    }
}

// Find a M2 model with very few bones (static-looking) in the 1.12.1 chain.
use benilla_m2::parse_m2;
use benilla_mpq::Archive;
use std::io::Cursor;

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let mpqs: Vec<Archive> = std::fs::read_dir(data)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            p.extension()
                .is_some_and(|x| x.to_string_lossy().eq_ignore_ascii_case("mpq"))
                .then(|| Archive::open(&p).ok())
                .flatten()
        })
        .collect();

    // Known-real 1.12.1 M2 paths (from benilla source + probe3 hits).
    let candidates: Vec<&str> = vec![
        "Interface\\Buttons\\TalkToMe.m2",
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Creature\\Wolf\\Wolf.m2",
        "Creature\\Undead\\Skeleton\\Skeleton.m2",
        "Interface\\GFX\\WoWLogo.m2",
        "Creature\\Humanoid\\M2\\M2Male_00.m2",
        "Creature\\Humanoid\\M2\\M2Female_00.m2",
    ];

    for c in &candidates {
        for a in &mpqs {
            if !a.contains(c) {
                continue;
            }
            let bytes = match a.read_file(c) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let fmt = match parse_m2(&mut Cursor::new(bytes.as_slice())) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let m = fmt.model();
            let static_like = m.bones.len() <= 1;
            println!(
                "{:<45} verts={:>4} bones={:>3} static_like={}  ({:?} bytes)",
                c,
                m.vertices.len(),
                m.bones.len(),
                static_like,
                bytes.len()
            );
        }
    }
}

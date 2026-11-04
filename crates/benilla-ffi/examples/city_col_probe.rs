// The Stormwind city WMO (`...Stormwind\Stormwind.wmo`): which ADTs place it,
// what its MODD internal doodads are (trees/fences/barrels), and whether those
// models ship a collision hull. Also the host MDDF props on the same tile.
use std::collections::HashMap;
use std::path::Path;

fn hull_tris(chain: &mut benilla_formats::Chain, raw: &str) -> Option<usize> {
    if raw.is_empty() {
        return None;
    }
    let p = raw.to_ascii_lowercase().replace(".mdx", ".m2").replace(".mdl", ".m2");
    let b = chain.read_file(&p).ok()?;
    benilla_formats::parse_m2_collision_hull(&b)
        .ok()
        .map(|h| h.triangle_count())
}

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let mut chain = benilla_formats::Chain::open(Path::new(data)).expect("open chain");
    let adts: Vec<String> = chain
        .list()
        .expect("list")
        .iter()
        .map(|e| e.name.replace('/', "\\"))
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.starts_with("world\\maps\\azeroth\\") && l.ends_with(".adt")
        })
        .collect();
    let mut seen_root: HashMap<String, ()> = HashMap::new();
    for path in &adts {
        let Ok(bytes) = chain.read_file(path) else {
            continue;
        };
        let Ok(mesh) = benilla_formats::adt_to_tile_mesh(&bytes) else {
            continue;
        };
        let hits: Vec<String> = mesh
            .wmos
            .iter()
            .map(|w| w.model.to_ascii_lowercase())
            .filter(|m| m.contains("stormwind\\stormwind.wmo") || m.ends_with("stormwind.wmo"))
            .collect();
        if hits.is_empty() {
            continue;
        }
        println!("{} places {:?}", path, hits);
        for root in hits {
            if seen_root.insert(root.clone(), ()).is_some() {
                continue;
            }
            let wtris = match chain.read_file(&root) {
                Ok(_) => benilla_formats::load_wmo_collision_tris(&mut chain, &root)
                    .map(|m| m.triangle_count())
                    .unwrap_or(usize::MAX),
                Err(_) => usize::MAX - 1,
            };
            println!("  root walk_tris={wtris}");
            let b = match chain.read_file(&root) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let Ok(r) = benilla_formats::parse_wmo_root(&b) else {
                println!("  root parse fail");
                continue;
            };
            let mut tally: HashMap<String, (usize, usize)> = HashMap::new(); // name -> (count, hull or 0)
            for d in r.doodads() {
                let name = d.model.rsplit('\\').next().unwrap_or("").to_ascii_lowercase();
                let key = if name.contains("tree")
                    || name.contains("fence")
                    || name.contains("barrel")
                    || name.contains("brush")
                    || name.contains("bush")
                {
                    true
                } else {
                    false
                };
                if !key {
                    continue;
                }
                let t = hull_tris(&mut chain, &d.model).unwrap_or(0);
                let e = tally.entry(name).or_insert((0, t));
                e.0 += 1;
            }
            let mut rows: Vec<_> = tally.iter().collect();
            rows.sort();
            println!("  tree/fence/barrel MODD models: {}", rows.len());
            for (m, (n, t)) in rows.iter().take(40) {
                println!("    x{n} hull={t} {m}");
            }
        }
        let mut mhull = 0usize;
        let mut mno = 0usize;
        let mut no_names: Vec<String> = Vec::new();
        for d in &mesh.doodads {
            let name = d.model.to_ascii_lowercase();
            let is_tree = name.contains("tree") || name.contains("fence") || name.contains("barrel");
            let t = hull_tris(&mut chain, &d.model).unwrap_or(0);
            if t > 0 {
                mhull += 1;
            } else {
                mno += 1;
                if is_tree {
                    no_names.push(name.clone());
                }
            }
        }
        println!(
            "  tile MDDF: {} hulled, {} no-hull; no-hull tree/fence/barrel: {:?}",
            mhull,
            mno,
            {
                no_names.sort();
                no_names.dedup();
                no_names.iter().take(6).cloned().collect::<Vec<_>>()
            }
        );
    }
}

// Indoor-WMO collision census: for every WMO placed on the Azeroth tiles that
// carry an "inn", print groups / groups with walking triangles / total walking
// triangles, using the exact same face filter as `load_wmo_collision_tris`
// (skip MOPY DETAIL, add the reversed winding for interior or two-sided steep
// faces). Read-only: answers "is the collision data loaded, or is the mover
// missing hits?" with numbers instead of a guess.
use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::Path;

use benilla_formats::adt_to_tile_mesh;
use benilla_wmo::{parse_wmo, ParsedWmo};

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let mut chain = benilla_formats::Chain::open(Path::new(data)).expect("open chain");
    let entries = chain.list().expect("list");
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for e in &entries {
        let name = e.name.replace('/', "\\").to_ascii_lowercase();
        if !name.starts_with("world\\maps\\azeroth\\") || !name.ends_with(".adt") {
            continue;
        }
        let Ok(bytes) = chain.read_file(&e.name.replace('/', "\\")) else {
            continue;
        };
        let Ok(mesh) = benilla_formats::adt_to_tile_mesh(&bytes) else {
            continue;
        };
        for w in &mesh.wmos {
            let m = w.model.to_ascii_lowercase();
            if m.contains("inn") {
                roots.insert(m);
            }
        }
    }
    // Known fallback when the ADT scan finds nothing (the path is stable in 1.12).
    roots.insert("world\\generic\\human\\goldshireinn\\goldshireinn.wmo".to_string());
    for root_path in roots {
        let Ok(rb) = chain.read_file(&root_path) else {
            println!("{root_path}: MISSING (root not in archives)");
            continue;
        };
        let Ok(ParsedWmo::Root(root)) = parse_wmo(&mut Cursor::new(&rb)) else {
            println!("{root_path}: root parse failed");
            continue;
        };
        let stem = root_path.strip_suffix(".wmo").unwrap_or(&root_path);
        let two_sided: Vec<bool> = root.materials.iter().map(|m| m.flags & 0x04 != 0).collect();
        let mut with_tris = 0usize;
        let mut total = 0usize;
        // Floor-level census: walkable faces bucketed by their average local z,
        // so a second storey shows up as its own band instead of a guess.
        let mut bands: std::collections::BTreeMap<i32, usize> = Default::default();
        for gi in 0..root.n_groups {
            let gp = format!("{stem}_{gi:03}.wmo");
            let Ok(gb) = chain.read_file(&gp) else {
                println!("  group {:03}: MISSING {gp}", gi);
                continue;
            };
            let Ok(ParsedWmo::Group(g)) = parse_wmo(&mut Cursor::new(&gb)) else {
                println!("  group {:03}: parse failed", gi);
                continue;
            };
            let interior = g.flags & 0x48 == 0;
            let mut kept = 0usize;
            for t in 0..g.vertex_indices.len() / 3 {
                let Some(mopy) = g.material_info.get(t) else {
                    continue;
                };
                if mopy.flags & 0x04 != 0 {
                    continue; // MOPY_DETAIL, same skip as the loader
                }
                kept += 1;
                let ts = interior || two_sided.get(mopy.material_id as usize).copied().unwrap_or(false);
                if ts {
                    // Steepness gate, then the loader's reversed winding.
                    let i0 = g.vertex_indices[t * 3] as usize;
                    let i1 = g.vertex_indices[t * 3 + 1] as usize;
                    let i2 = g.vertex_indices[t * 3 + 2] as usize;
                    if let (Some(p0), Some(p1), Some(p2)) = (
                        g.vertex_positions.get(i0),
                        g.vertex_positions.get(i1),
                        g.vertex_positions.get(i2),
                    ) {
                        let a = [p1.x - p0.x, p1.y - p0.y, p1.z - p0.z];
                        let b = [p2.x - p0.x, p2.y - p0.y, p2.z - p0.z];
                        let n = [
                            a[1] * b[2] - a[2] * b[1],
                            a[2] * b[0] - a[0] * b[2],
                            a[0] * b[1] - a[1] * b[0],
                        ];
                        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                        if len > 0.0 {
                            let nz = (n[2] / len).abs();
                            if nz < 0.642_788 {
                                kept += 1;
                            } else {
                                // Walkable floor: bucket its height.
                                let z = (p0.z + p1.z + p2.z) as f32 / 3.0;
                                *bands.entry(z.round() as i32).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
            if kept > 0 {
                with_tris += 1;
            }
            total += kept;
            println!("  group {:03}: flags=0x{:04x} interior={} kept_tris={}", gi, g.flags, interior, kept);
        }
        println!("SUMMARY {root_path} groups={} groups_with_tris={} total_walk_tris={}", root.n_groups, with_tris, total);
        for (z, c) in bands.iter().filter(|(_, c)| **c >= 40) {
            println!("FLOORBAND z={z} faces={c}");
        }
    }
}

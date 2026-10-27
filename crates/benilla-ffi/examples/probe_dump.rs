// Decisive diagnostic: dump the actual vertex coords, triangle indices,
// index-range consistency, and bone pivots for a skinned M2 model, to find
// exactly why the mesh renders as "messy triangles" (wrong connectivity or
// bone-local rest-pose that needs pivots applied).
use benilla_m2::{parse_m2, M2Model};
use benilla_mpq::Archive;
use std::io::Cursor;

fn dist3(x: f32, y: f32, z: f32, a: f32, b: f32, c: f32) -> f32 {
    let dx = x - a;
    let dy = y - b;
    let dz = z - c;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn dump(name: &str, data: &str) {
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.to_lowercase().ends_with(".mpq"))
        .collect();
    let mut found = false;
    for mpq in &files {
        let a = match Archive::open(mpq) {
            Ok(x) => x,
            Err(_) => continue,
        };
        if !a.contains(name) {
            continue;
        }
        let bytes = match a.read_file(name) {
            Ok(b) => b,
            Err(_) => continue,
        };
        found = true;
        let fmt = match parse_m2(&mut Cursor::new(bytes.as_slice())) {
            Ok(f) => f,
            Err(e) => {
                println!("{name}: parse failed: {e}");
                return;
            }
        };
        let m: &M2Model = fmt.model();
        println!("\n=== {name} ({} bytes) ===", bytes.len());
        println!("verts={} bones={}", m.vertices.len(), m.bones.len());

        // Vertex coord range.
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for v in &m.vertices {
            min[0] = min[0].min(v.position.x);
            min[1] = min[1].min(v.position.y);
            min[2] = min[2].min(v.position.z);
            max[0] = max[0].max(v.position.x);
            max[1] = max[1].max(v.position.y);
            max[2] = max[2].max(v.position.z);
        }
        println!(
            "vertex range min=({:.2},{:.2},{:.2}) max=({:.2},{:.2},{:.2})",
            min[0], min[1], min[2], max[0], max[1], max[2]
        );

        println!("first 4 vertices:");
        for i in 0..4.min(m.vertices.len()) {
            let v = &m.vertices[i];
            println!(
                "  v{i} pos=({:.2},{:.2},{:.2}) bone0={} w0={}",
                v.position.x, v.position.y, v.position.z, v.bone_indices[0], v.bone_weights[0]
            );
        }

        if let Ok(skin) = m.parse_embedded_skin(&bytes, 0) {
            let idx = skin.indices();
            let maxv = idx.iter().copied().max().unwrap_or(0) as usize;
            println!(
                "skin0 indices: len={} max_idx={} verts={} -> {}",
                idx.len(),
                maxv,
                m.vertices.len(),
                if maxv < m.vertices.len() {
                    "OK in range"
                } else {
                    "OUT OF RANGE!!"
                }
            );
            let ntri = idx.len() / 3;
            println!("first 3 triangles (edge lengths):");
            for t in 0..3.min(ntri) {
                let a = idx[t * 3] as usize;
                let b = idx[t * 3 + 1] as usize;
                let c = idx[t * 3 + 2] as usize;
                let va = &m.vertices[a];
                let vb = &m.vertices[b];
                let vc = &m.vertices[c];
                let dab = dist3(
                    va.position.x,
                    va.position.y,
                    va.position.z,
                    vb.position.x,
                    vb.position.y,
                    vb.position.z,
                );
                let dbc = dist3(
                    vb.position.x,
                    vb.position.y,
                    vb.position.z,
                    vc.position.x,
                    vc.position.y,
                    vc.position.z,
                );
                let dca = dist3(
                    vc.position.x,
                    vc.position.y,
                    vc.position.z,
                    va.position.x,
                    va.position.y,
                    va.position.z,
                );
                println!(
                    "  tri{t} ({} {} {}) ab={:.2} bc={:.2} ca={:.2}",
                    a, b, c, dab, dbc, dca
                );
            }
        } else {
            println!("skin0 parse failed");
        }

        println!("first 3 bones:");
        for i in 0..3.min(m.bones.len()) {
            let b = &m.bones[i];
            println!(
                "  bone{i} pivot=({:.2},{:.2},{:.2}) parent={}",
                b.pivot.x, b.pivot.y, b.pivot.z, b.parent
            );
        }
        break;
    }
    if !found {
        println!("{name}: NOT FOUND in chain");
    }
}

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    dump("Interface\\Buttons\\TalkToMe.m2", data);
    dump("Creature\\RidingHorse\\RidingHorse.m2", data);
}

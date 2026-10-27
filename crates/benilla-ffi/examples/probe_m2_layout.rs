// Probe the M2 vertex position layout to diagnose "messy triangles": are the
// rest-pose vertex positions global model-space, or bone-relative offsets?
// Dumps the first few vertices (position + bound bone) plus the bone pivots.
use benilla_m2::{parse_m2, M2Model, C3};
use std::io::Cursor;

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    // RidingHorse is in model.MPQ per probe3.
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.to_lowercase().ends_with(".mpq"))
        .collect();
    let path = "Creature\\RidingHorse\\RidingHorse.m2";
    for mpq in &files {
        let a = match benilla_mpq::Archive::open(mpq) {
            Ok(a) => a,
            Err(_) => continue,
        };
        if !a.contains(path) {
            continue;
        }
        let bytes = a.read_file(path).expect("read RidingHorse");
        let fmt = parse_m2(&mut Cursor::new(bytes.as_slice())).expect("parse m2");
        let m: &M2Model = fmt.model();
        println!(
            "RidingHorse: {} vertices, {} bones",
            m.vertices.len(),
            m.bones.len()
        );
        // Authored bounds vs actual vertex range.
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for v in &m.vertices {
            for (i, c) in [v.position.x, v.position.y, v.position.z]
                .iter()
                .enumerate()
            {
                if *c < min[i] {
                    min[i] = *c;
                }
                if *c > max[i] {
                    max[i] = *c;
                }
            }
        }
        println!(
            "vertex range: ({:.1},{:.1},{:.1}) - ({:.1},{:.1},{:.1})",
            min[0], min[1], min[2], max[0], max[1], max[2]
        );
        println!(
            "authored bounds : ({:.1},{:.1},{:.1}) - ({:.1},{:.1},{:.1}) radius={:.1}",
            m.header.bounding_box_min[0],
            m.header.bounding_box_min[1],
            m.header.bounding_box_min[2],
            m.header.bounding_box_max[0],
            m.header.bounding_box_max[1],
            m.header.bounding_box_max[2],
            m.header.bounding_sphere_radius
        );
        // Dump first 5 vertices: position, bound bones, and that bone's pivot.
        for i in 0..5.min(m.vertices.len()) {
            let v = &m.vertices[i];
            let b0 = v.bone_indices[0] as usize;
            let w0 = v.bone_weights[0] as f32;
            let piv = if b0 < m.bones.len() {
                &m.bones[b0].pivot
            } else {
                &C3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                }
            };
            println!(
                "  v{i:3} pos=({:7.1},{:7.1},{:7.1}) w0={:3.0} bone0={:2} bone0pivot=({:7.1},{:7.1},{:7.1})",
                v.position.x, v.position.y, v.position.z, w0, b0, piv.x, piv.y, piv.z
            );
        }
        break;
    }
}

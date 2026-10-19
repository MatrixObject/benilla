// Find static (non-skinned) M2 models whose rest-pose IS the final shape,
// so the POC can render a clean model without doing bone deformation.
use benilla_m2::parse_m2;
use benilla_mpq::Archive;
use std::io::Cursor;

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.to_lowercase().ends_with(".mpq"))
        .collect();

    // Candidate paths known to exist in 1.12.1 (from probe3 + benilla sources).
    let candidates: Vec<&str> = vec![
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Item\\Sword\\Sword_01.m2",
        "Item\\Staff\\Staff_01.m2",
        "Creature\\Wolf\\Wolf.m2",
        "World\\Maps\\Durotar\\Durotar_M2.m2",
    ];

    for mpq in &files {
        let a = match Archive::open(mpq) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for c in &candidates {
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
            let bones = m.bones.len();
            let verts = m.vertices.len();
            // A model is "static-looking" if it has few/no bones or its vertex
            // span closely matches the authored bounds (rest == final).
            let mut span_x = 0f32;
            let mut span_y = 0f32;
            let mut span_z = 0f32;
            if !m.vertices.is_empty() {
                let mut minx = f32::MAX;
                let mut maxx = f32::MIN;
                let mut miny = f32::MAX;
                let mut maxy = f32::MIN;
                let mut minz = f32::MAX;
                let mut maxz = f32::MIN;
                for v in &m.vertices {
                    minx = minx.min(v.position.x);
                    maxx = maxx.max(v.position.x);
                    miny = miny.min(v.position.y);
                    maxy = maxy.max(v.position.y);
                    minz = minz.min(v.position.z);
                    maxz = maxz.max(v.position.z);
                }
                span_x = maxx - minx;
                span_y = maxy - miny;
                span_z = maxz - minz;
            }
            let bb_w = m.header.bounding_box_max[0] - m.header.bounding_box_min[0];
            let bb_h = m.header.bounding_box_max[1] - m.header.bounding_box_min[1];
            let bb_d = m.header.bounding_box_max[2] - m.header.bounding_box_min[2];
            println!(
                "{:<45} verts={:>4} bones={:>3}  vspan=({:.1},{:.1},{:.1})  bounds=({:.1},{:.1},{:.1}) [{}: {}]",
                c, verts, bones, span_x, span_y, span_z, bb_w, bb_h, bb_d, mpq.split('\\').last().unwrap(), bytes.len()
            );
        }
    }
}

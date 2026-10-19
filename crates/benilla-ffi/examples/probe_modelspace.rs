// Decisive test: compare M2 vertex position range against the header's authored
// bounding box. If they match, vertex positions ARE model-space and must be
// rendered directly (no bone pivot added). If vertex range is far smaller than
// the header box, they're bone-local and need skinning.
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

    let model = "Creature\\RidingHorse\\RidingHorse.m2";
    for mpq in &files {
        let a = match Archive::open(mpq) { Ok(x) => x, Err(_) => continue };
        if !a.contains(model) { continue; }
        let bytes = a.read_file(model).expect("read RidingHorse");
        let fmt = parse_m2(&mut Cursor::new(bytes.as_slice())).expect("parse");
        let m = fmt.model();

        let (mut vmin, mut vmax) = ([f32::MAX; 3], [f32::MIN; 3]);
        for v in &m.vertices {
            vmin[0] = vmin[0].min(v.position.x);
            vmin[1] = vmin[1].min(v.position.y);
            vmin[2] = vmin[2].min(v.position.z);
            vmax[0] = vmax[0].max(v.position.x);
            vmax[1] = vmax[1].max(v.position.y);
            vmax[2] = vmax[2].max(v.position.z);
        }
        let hmin = m.header.bounding_box_min;
        let hmax = m.header.bounding_box_max;
        let hr = m.header.bounding_sphere_radius;

        println!("RidingHorse raw M2 data:");
        println!("  vertex range  min=({:.2},{:.2},{:.2})  max=({:.2},{:.2},{:.2})",
            vmin[0], vmin[1], vmin[2], vmax[0], vmax[1], vmax[2]);
        println!("  vertex span   ({:.2},{:.2},{:.2})",
            vmax[0]-vmin[0], vmax[1]-vmin[1], vmax[2]-vmin[2]);
        println!("  header bbox   min=({:.2},{:.2},{:.2})  max=({:.2},{:.2},{:.2})  radius={:.2}",
            hmin[0], hmin[1], hmin[2], hmax[0], hmax[1], hmax[2], hr);
        println!("  header span   ({:.2},{:.2},{:.2})",
            hmax[0]-hmin[0], hmax[1]-hmin[1], hmax[2]-hmin[2]);

        let single_bone_full = m.vertices.iter()
            .filter(|v| v.bone_weights[0] == 255 && v.bone_weights[1] == 0).count();
        println!("  single-bone-full-weight verts: {}/{}", single_bone_full, m.vertices.len());

        let mut pivot_max = 0.0f32;
        for b in &m.bones {
            let mag = b.pivot.x.abs().max(b.pivot.y.abs()).max(b.pivot.z.abs());
            if mag > pivot_max { pivot_max = mag; }
        }
        println!("  max |bone pivot| magnitude = {pivot_max:.2}");
        break;
    }
}

// Scratch: the selection ring's radius inputs for one creature display id.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let mut chain = benilla_formats::Chain::open(std::path::Path::new(data)).expect("chain");
    let cat = benilla_formats::load_creature_catalog(&mut chain).expect("catalog");
    for arg in std::env::args().skip(1) {
        let id: u32 = arg.parse().expect("display id");
        let Some(model) = cat.model(id) else {
            println!("{id}: no row");
            continue;
        };
        let path = model
            .model_path
            .replace(".mdx", ".m2")
            .replace(".mdl", ".m2");
        println!("{id}: model={path} scale={}", model.scale);
        match chain.read_file(&path.to_ascii_lowercase()) {
            Ok(bytes) => match benilla_formats::parse_m2_bounds(&bytes) {
                Ok(b) => println!(
                    "  ring_footprint={:.3} bbox=[{:.3},{:.3},{:.3}]..[{:.3},{:.3},{:.3}] \
                     sphere={:.3} pivot_z={:?} stand_box_z={:.3}",
                    b.ring_footprint,
                    b.bbox_min[0],
                    b.bbox_min[1],
                    b.bbox_min[2],
                    b.bbox_max[0],
                    b.bbox_max[1],
                    b.bbox_max[2],
                    b.sphere_radius,
                    b.pivot_z,
                    b.stand_box_z
                ),
                Err(e) => println!("  bounds failed: {e}"),
            },
            Err(e) => println!("  read failed: {e}"),
        }
    }
}

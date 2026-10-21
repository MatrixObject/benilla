// What does a placed "living" doodad's M2 look like? Dump the rig facts for
// a model path: sequences (anim id, band, frequency, replay, posed bones), the
// playable-animation lookup, the animation lookup, bones and vertices.
//
// Read-only probe: it touches the install and writes nothing.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "world\\critter\\fish\\fish01.m2".to_string());
    // A C string: the FFI reads to the NUL, and a Rust String has none.
    let path_c = std::ffi::CString::new(path.clone()).unwrap();

    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }

    // Read the file through the same chain the FFI uses.
    let mut chain = benilla_formats::Chain::open(std::path::Path::new(
        "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data",
    ))
    .expect("chain");
    let bytes = chain.read_file(&path).expect("read m2");
    println!("{}: {} bytes", path, bytes.len());

    let anims = benilla_formats::parse_m2_animations(&bytes);
    println!("sequences: {}", anims.len());
    for a in &anims {
        println!(
            "  seq[{}] anim_id={} band=[{}..{}] dur={:.3}s loop={} freq={} replay={:?} bones={} rest={}",
            a.seq_index,
            a.anim_id,
            a.start_ms,
            a.end_ms,
            a.duration,
            a.looping,
            a.frequency,
            (a.min_replay, a.max_replay),
            a.bones.len(),
            a.is_rest_pose()
        );
        for b in a.bones.iter().take(4) {
            println!(
                "      bone {} t={} r={} s={}",
                b.bone,
                b.translation.len(),
                b.rotation.len(),
                b.scale.len()
            );
        }
    }

    let pal = benilla_formats::parse_m2_playable_animation_lookup(&bytes).unwrap_or_default();
    println!("playable lookup ({} rows):", pal.len());
    for (i, p) in pal.iter().enumerate().take(8) {
        println!("  id {i} -> {} (dir {:#x})", p.resolved_id, p.dir_flags);
    }

    let al = benilla_formats::parse_m2_animation_lookup(&bytes).unwrap_or_default();
    println!("animation lookup ({} rows): {al:?}", al.len());

    let fmt = benilla_m2::parse_m2(&mut std::io::Cursor::new(&bytes)).expect("parse m2");
    let m = fmt.model();
    println!(
        "bones={} verts={} materials={} textures={}",
        m.bones.len(),
        m.vertices.len(),
        m.materials.len(),
        m.textures.len()
    );
    for (i, b) in m.bones.iter().enumerate() {
        println!(
            "  bone {i} parent={} pivot=({:.3},{:.3},{:.3}) flags={:#x}",
            b.parent, b.pivot.x, b.pivot.y, b.pivot.z, b.flags.bits()
        );
    }
    // Skin: which bones influence which vertices.
    let mut infl: std::collections::BTreeSet<u8> = Default::default();
    for v in m.vertices.iter() {
        for b in v.bone_indices {
            infl.insert(b);
        }
    }
    println!("bones influencing vertices: {infl:?}");

    // What the FFI's own skinning says: does the model move over the idle band?
    let mut buf = vec![0f32; 400_000];
    for a in &anims {
        let cap = (buf.len() / 3) as u32;
        let n0 = benilla::bn_model_skinned_positions(
            w,
            path_c.as_ptr(),
            a.seq_index as u32,
            a.start_ms,
            buf.as_mut_ptr(),
            cap,
        );
        let mid = a.start_ms + ((a.end_ms - a.start_ms) / 2).max(1);
        let n1 = benilla::bn_model_skinned_positions(
            w,
            path_c.as_ptr(),
            a.seq_index as u32,
            mid,
            buf.as_mut_ptr(),
            cap,
        );
        if n0 == 0 || n0 != n1 {
            println!("  seq[{}] skin read failed ({n0}/{n1})", a.seq_index);
            continue;
        }
        let mut maxd = 0f32;
        for v in 0..n0 as usize {
            let d = ((buf[v * 3] - buf[v * 3]).powi(2)).sqrt();
            if d > maxd {
                maxd = d;
            }
        }
        let _ = maxd;
        println!("  seq[{}] skin n={n0}", a.seq_index);
    }

    benilla::bn_shutdown(w);
}

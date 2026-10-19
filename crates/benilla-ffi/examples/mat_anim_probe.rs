// Exercise `bn_doodad_mat_anim`: load one tile, query every loaded doodad
// submesh's animation mask at t=0, then re-sample the animated ones over
// time. Prints channel counts and example trajectories so the host (UE MID
// driving) can trust the FFI before wiring it up.

fn main() {
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let map = std::ffi::CString::new("Azeroth").unwrap();
    // Stormwind/Elwynn neighbourhood: waterfalls, portals and torches carry
    // the UV/alpha loops, and one tile may hold none of them.
    let tiles: Vec<(i32, i32)> = vec![
        (30, 48),
        (31, 48),
        (30, 47),
        (31, 47),
        (29, 48),
        (32, 48),
        (30, 49),
        (30, 46),
        (32, 47),
        (33, 48),
    ];

    let (mut loaded, mut submeshes) = (0u32, 0u32);
    let (mut n_total, mut tiles_ok) = (0u32, 0u32);
    let (mut n_uv, mut n_rgb, mut n_alpha) = (0u32, 0u32, 0u32);
    let (mut uv_moving, mut rgb_moving, mut alpha_moving) = (0u32, 0u32, 0u32);
    // blend class (bn_doodad_blend out value) -> count, plus AlphaTest samples.
    let mut blend_counts: std::collections::BTreeMap<u32, u32> = Default::default();
    let mut alphatest: Vec<String> = Vec::new();
    // Submeshes whose texture resolves to nothing (renders textureless) + models.
    let mut empty_tex = 0u32;
    let mut empty_tex_models: Vec<String> = Vec::new();
    // (path, j, mask, samples) for the first few animated submeshes.
    let mut examples: Vec<(String, u32, u32, Vec<[f32; 6]>)> = Vec::new();
    let mut moving_examples: Vec<(String, u32, u32, Vec<[f32; 6]>)> = Vec::new();

    for &(tx, tz) in &tiles {
        if benilla::bn_load_tile(w, map.as_ptr(), tx, tz) == 0 {
            eprintln!("tile ({tx},{tz}): load failed");
            continue;
        }
        tiles_ok += 1;
        let total = benilla::bn_doodad_count(w);
        n_total += total;
        for i in 0..total {
            let sub_n = benilla::bn_doodad_load(w, i);
            if sub_n == 0 {
                continue;
            }
            loaded += 1;
            let mut path_buf = [0i8; 512];
            benilla::bn_doodad_model_path(w, i, path_buf.as_mut_ptr(), path_buf.len() as u32);
            let path = {
                let n = path_buf
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(path_buf.len());
                let bytes: Vec<u8> = path_buf[..n].iter().map(|&c| c as u8).collect();
                String::from_utf8_lossy(&bytes).into_owned()
            };
            for j in 0..sub_n {
            submeshes += 1;
            let mut blend = 0u32;
            benilla::bn_doodad_blend(w, i, j, &mut blend);
            *blend_counts.entry(blend).or_insert(0) += 1;
            if blend & 0xff == 1 && alphatest.len() < 12 && !alphatest.contains(&path) {
                alphatest.push(path.clone());
            }
            let mut tex_buf = [0i8; 512];
            // bn_doodad_texture returns 1 on success regardless of length;
            // an empty path is a NUL at [0].
            benilla::bn_doodad_texture(w, i, j, tex_buf.as_mut_ptr(), 512);
            if tex_buf[0] == 0 {
                empty_tex += 1;
                if empty_tex_models.len() < 12 && !empty_tex_models.contains(&path) {
                    empty_tex_models.push(path.clone());
                }
            }
            let mut out = [0f32; 6];
                let mask = benilla::bn_doodad_mat_anim(w, i, j, 0.0, out.as_mut_ptr());
                if mask == 0 {
                    continue;
                }
                // Four sample points: catches loops and, via t>0 deltas, constant
                // shifts (a period-0 constant never moves).
                let mut samples = vec![out];
                for &t in &[0.5f64, 1.0, 2.0] {
                    let mut s = [0f32; 6];
                    benilla::bn_doodad_mat_anim(w, i, j, t, s.as_mut_ptr());
                    samples.push(s);
                }
                let uv_moved = samples
                    .iter()
                    .any(|s| (s[0] - out[0]).abs() > 1e-5 || (s[1] - out[1]).abs() > 1e-5);
                let rgb_moved = samples.iter().any(|s| {
                    (s[2] - out[2]).abs() > 1e-5
                        || (s[3] - out[3]).abs() > 1e-5
                        || (s[4] - out[4]).abs() > 1e-5
                });
                let alpha_moved = samples.iter().any(|s| (s[5] - out[5]).abs() > 1e-5);
                if mask & 1 != 0 {
                    n_uv += 1;
                    if uv_moved {
                        uv_moving += 1;
                    }
                }
                if mask & 2 != 0 {
                    n_rgb += 1;
                    if rgb_moved {
                        rgb_moving += 1;
                    }
                }
                if mask & 4 != 0 {
                    n_alpha += 1;
                    if alpha_moved {
                        alpha_moving += 1;
                    }
                }
                if examples.len() < 8 {
                    examples.push((path.clone(), j, mask, samples.clone()));
                }
                if (uv_moved || rgb_moved || alpha_moved) && moving_examples.len() < 8 {
                    moving_examples.push((path.clone(), j, mask, samples));
                }
            }
        }
    }

    println!("{tiles_ok} tiles, {n_total} doodads: {loaded} loaded, {submeshes} submeshes");
    println!("blend class counts (0 opq 1 atest 2 blend 3 mod 4 mod2x, +0x100 additive):");
    for (k, v) in &blend_counts {
        println!("  {k:#x}: {v}");
    }
    println!("alpha-test models:");
    for p in &alphatest {
        println!("  {p}");
    }
    println!("textureless submeshes: {empty_tex}");
    for p in &empty_tex_models {
        println!("  {p}");
    }
    println!(
        "animated: uv {n_uv} ({uv_moving} moving), rgb {n_rgb} ({rgb_moving} moving), alpha {n_alpha} ({alpha_moving} moving)"
    );
    println!("\nfirst animated submeshes (t = 0, 0.5, 1.0, 2.0):");
    for (p, j, mask, samples) in &examples {
        println!("  {p} submesh {j} mask {mask}");
        for s in samples {
            println!(
                "    t: uv=({:+.4},{:+.4}) rgb=({:.3},{:.3},{:.3}) a={:.3}",
                s[0], s[1], s[2], s[3], s[4], s[5]
            );
        }
    }
    println!("\nfirst MOVING submeshes:");
    for (p, j, mask, samples) in &moving_examples {
        println!("  {p} submesh {j} mask {mask}");
        for s in samples {
            println!(
                "    t: uv=({:+.4},{:+.4}) rgb=({:.3},{:.3},{:.3}) a={:.3}",
                s[0], s[1], s[2], s[3], s[4], s[5]
            );
        }
    }

    benilla::bn_shutdown(w);
}

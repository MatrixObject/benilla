// Do terrain doodads (MDDF M2s) actually move? Per unique model path on a
// few tiles: anim_count -> skinned positions at 8 times across the first
// sequence -> max vertex delta. Also times bn_model_skinned_positions
// (re-parses the M2 per call - decides whether per-frame skinning is viable).

fn cstr(buf: &[i8]) -> String {
    std::ffi::CStr::from_bytes_until_nul(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
    })
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default()
}

fn main() {
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let map = std::ffi::CString::new("Azeroth").unwrap();

    // Known doodad-rich tiles: Elwynn + Stormwind.
    let candidates = [(30i32, 48i32), (31, 48), (30, 47), (29, 47)];

    let mut pathbuf = [0i8; 512];
    let mut bufs: Vec<Vec<f32>> = vec![vec![0f32; 400_000]; 2];
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let (mut paths, mut with_anim, mut moving_paths) = (0u32, 0u32, 0u32);
    let mut skin_ms = 0.0f64;
    let mut skin_calls = 0u32;
    let mut moving_names: Vec<(f32, String)> = Vec::new();

    for &(tx, ty) in &candidates {
        if benilla::bn_load_tile(w, map.as_ptr(), tx, ty) == 0 {
            println!("tile {tx},{ty}: load failed");
            continue;
        }
        let total = benilla::bn_doodad_count(w);
        println!("Azeroth_{tx}_{ty}: {total} doodads");
        for i in 0..total {
            if benilla::bn_doodad_model_path(w, i, pathbuf.as_mut_ptr(), pathbuf.len() as u32) == 0
            {
                continue;
            }
            let path = cstr(&pathbuf);
            if !seen.insert(path.clone()) {
                continue;
            }
            paths += 1;
            let cp = std::ffi::CString::new(path.clone()).unwrap();
            let anims = benilla::bn_model_anim_count(w, cp.as_ptr());
            if anims == 0 {
                continue;
            }
            with_anim += 1;
            let mut range = [0u32; 2];
            if benilla::bn_model_anim_time_range(w, cp.as_ptr(), 0, range.as_mut_ptr()) == 0
                || range[1] <= range[0]
            {
                continue;
            }
            let (t0a, t1a) = (range[0], range[1]);
            let cap = (bufs[0].len() / 3) as u32;
            let mut maxd = 0.0f32;
            let mut nref = 0u32;
            let t_start = std::time::Instant::now();
            let (b0, b1) = bufs.split_at_mut(1);
            for s in 0..8u32 {
                let t = t0a + (t1a - t0a) * s / 7;
                let (cur, prev): (&mut [f32], &[f32]) = if s % 2 == 0 {
                    (&mut b0[0], &b1[0])
                } else {
                    (&mut b1[0], &b0[0])
                };
                let n = benilla::bn_model_skinned_positions(
                    w,
                    cp.as_ptr(),
                    0,
                    t,
                    cur.as_mut_ptr(),
                    cap,
                );
                skin_calls += 1;
                if n == 0 {
                    continue;
                }
                if s > 0 && n == nref {
                    for v in 0..n as usize {
                        let (p, q) = (&cur[v * 3..v * 3 + 3], &prev[v * 3..v * 3 + 3]);
                        let d =
                            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2))
                                .sqrt();
                        if d > maxd {
                            maxd = d;
                        }
                    }
                }
                nref = n;
            }
            skin_ms += t_start.elapsed().as_secs_f64() * 1000.0;
            let flag = if maxd > 0.05 {
                moving_paths += 1;
                moving_names.push((maxd, path.clone()));
                "MOVING"
            } else {
                "static"
            };
            println!(
                "  [{flag}] max_delta={maxd:.3} anims={anims} dur={} {path}",
                t1a - t0a
            );
        }
    }

    println!("----");
    println!("unique paths: {paths}, with anims: {with_anim}, moving: {moving_paths}");
    moving_names.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    for (d, p) in moving_names.iter().take(20) {
        println!("  moving {d:.3}: {p}");
    }
    if skin_calls > 0 {
        println!(
            "bn_model_skinned_positions: {:.1}ms / {skin_calls} calls ({:.2}ms/call)",
            skin_ms,
            skin_ms / skin_calls as f64
        );
    }
    benilla::bn_shutdown(w);
}

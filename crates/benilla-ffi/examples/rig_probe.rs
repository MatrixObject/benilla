// Probe the new placed-doodad rig FFI: the rig table of a model, the arm
// chain's weighted pick, the skinned pose over time and the placement matrix.
//
// Read-only probe: it touches the install and writes nothing.

// The rig exports are declared here (not reached through the crate root) so
// the probe also checks the exact C ABI the UE host binds.
extern "C" {
    fn bn_doodad_rig_info(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
        out: *mut u32,
        cap: u32,
    ) -> u32;
    fn bn_doodad_rig_pick(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
        anim_id: u32,
        roll: u32,
    ) -> u32;
    fn bn_doodad_rig_pose(
        world: *const benilla::BnWorld,
        model_path: *const std::os::raw::c_char,
        seq_index: u32,
        time_ms: u32,
        out: *mut f32,
        cap: u32,
    ) -> u32;
    fn bn_doodad_placement(world: *const benilla::BnWorld, i: u32, out: *mut f32) -> u32;
}

fn main() {
    // The rig exports are plain C calls; the whole probe runs inside one
    // unsafe block.
    unsafe { run() }
}

unsafe fn run() {
    let data_dir = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let dir = std::ffi::CString::new(data_dir).unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }

    // A tile with fish in it.
    let map = std::ffi::CString::new("Azeroth").unwrap();
    let ok = benilla::bn_load_tile(w, map.as_ptr(), 30, 29);
    println!("bn_load_tile(30,29) = {ok}");

    let n = benilla::bn_doodad_count(w);
    println!("doodads: {n}");

    let mut path_buf = vec![0i8; 512];
    let mut fish = String::new();
    let mut fish_inst = 0u32;
    for i in 0..n {
        if benilla::bn_doodad_model_path(w, i, path_buf.as_mut_ptr(), 512) == 0 {
            continue;
        }
        let p = std::ffi::CStr::from_ptr(path_buf.as_ptr())
            .to_string_lossy()
            .to_string();
        if p.contains("fish") {
            fish = p;
            fish_inst = i;
            break;
        }
    }
    if fish.is_empty() {
        eprintln!("no fish doodad in this tile");
        benilla::bn_shutdown(w);
        return;
    }
    println!("fish doodad #{fish_inst}: {fish}");
    // A C string: the FFI reads to the NUL, and a Rust String has none.
    let fish_c = std::ffi::CString::new(fish.clone()).unwrap();

    // ---- the rig table ----
    let mut table = vec![0u32; 512];
    eprintln!(
        "PROBE fish.as_ptr()={:p} len={} cap={} bytes={:?}",
        fish.as_ptr(),
        fish.len(),
        fish.capacity(),
        fish.as_bytes()
    );
    let total = bn_doodad_rig_info(w, fish_c.as_ptr(), table.as_mut_ptr(), 512);
    println!("rig info: {total} u32");
    if total >= 12 {
        let t = &table;
        println!(
            "  version={} flags={:#x} subs={} clips={} verts={} idxs={} idle={} seed={} bones={}",
            t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7], t[8]
        );
        let sub_n = t[2] as usize;
        let clip_n = t[3] as usize;
        for j in 0..sub_n {
            let o = 12 + j * 4;
            println!(
                "  submesh {j}: vtx0={} vtxN={} idx0={} idxN={}",
                t[o],
                t[o + 1],
                t[o + 2],
                t[o + 3]
            );
        }
        for j in 0..clip_n {
            let o = 12 + sub_n * 4 + j * 8;
            println!(
                "  clip {j}: seq={} band=[{}..{}] dur={}ms freq={} replay=({}..{}) loop={}",
                t[o],
                t[o + 1],
                t[o + 2],
                t[o + 3],
                t[o + 4],
                t[o + 5],
                t[o + 6],
                t[o + 7]
            );
        }
    }

    // ---- the weighted pick ----
    for roll in [0u32, 1, 16382, 16383, 32766, 32767] {
        let idx = bn_doodad_rig_pick(w, fish_c.as_ptr(), table[6], roll);
        println!("  pick(roll={roll}) -> {idx}");
    }

    // ---- the pose over the clip's band ----
    let sub_n = table[2] as usize;
    let clip_n = table[3] as usize;
    if clip_n > 0 {
        let o = 12 + sub_n * 4;
        let seq = table[o];
        let start = table[o + 1];
        let dur = table[o + 3];
        let mut buf = vec![0f32; 400_000];
        let cap = (buf.len() / 6) as u32;
        let mut prev: Option<Vec<f32>> = None;
        for k in 0..6 {
            let t = start + dur * k / 6;
            let nv = bn_doodad_rig_pose(w, fish_c.as_ptr(), seq, t, buf.as_mut_ptr(), cap);
            let mut maxd = 0f32;
            let mut maxi = 0usize;
            let mut bb_min = [f32::MAX; 3];
            let mut bb_max = [f32::MIN; 3];
            if let Some(p) = &prev {
                for v in 0..nv as usize {
                    let d = ((buf[v * 6] - p[v * 6]).powi(2)
                        + (buf[v * 6 + 1] - p[v * 6 + 1]).powi(2)
                        + (buf[v * 6 + 2] - p[v * 6 + 2]).powi(2))
                    .sqrt();
                    if d > maxd {
                        maxd = d;
                        maxi = v;
                    }
                }
            }
            for v in 0..nv as usize {
                for j in 0..3 {
                    bb_min[j] = bb_min[j].min(buf[v * 6 + j]);
                    bb_max[j] = bb_max[j].max(buf[v * 6 + j]);
                }
            }
            println!(
                "  pose(t={t}ms) verts={nv} maxDelta={maxd:.4} at v{maxi} p0=({:.3},{:.3},{:.3}) n0=({:.3},{:.3},{:.3}) bbox=({:.2},{:.2},{:.2})..({:.2},{:.2},{:.2})",
                buf[0], buf[1], buf[2], buf[6], buf[7], buf[8],
                bb_min[0], bb_min[1], bb_min[2], bb_max[0], bb_max[1], bb_max[2]
            );
            prev = Some(buf[..nv as usize * 6].to_vec());
        }

        // Cross-check against the existing skinning path: the same model, the
        // same sequence and the SAME time, through bn_model_skinned_positions.
        let nv = bn_doodad_rig_pose(w, fish_c.as_ptr(), seq, start, buf.as_mut_ptr(), cap);
        let mut sbuf = vec![0f32; 400_000];
        let sn = benilla::bn_model_skinned_positions(
            w,
            fish_c.as_ptr(),
            seq,
            start,
            sbuf.as_mut_ptr(),
            (sbuf.len() / 3) as u32,
        );
        println!("  rig pose at t={start}ms: {nv} verts; reference: {sn} verts");
        if sn > 0 && nv > 0 {
            // Match by position: the submesh flat order is a permutation of
            // the M2's global vertex order.
            let mut worst = 0f32;
            let mut worst_at = 0usize;
            for v in 0..sn as usize {
                let p = [sbuf[v * 3], sbuf[v * 3 + 1], sbuf[v * 3 + 2]];
                let mut best = f32::MAX;
                for u in 0..nv as usize {
                    let d = ((p[0] - buf[u * 6]).powi(2)
                        + (p[1] - buf[u * 6 + 1]).powi(2)
                        + (p[2] - buf[u * 6 + 2]).powi(2))
                    .sqrt();
                    if d < best {
                        best = d;
                    }
                }
                if best > worst {
                    worst = best;
                    worst_at = v;
                }
            }
            println!("  worst nearest-vertex distance vs the client skinning: {worst:.6} (client vert {worst_at})");
            // The other direction: every rigged vertex must land on a
            // client vertex too.
            let mut worst2 = 0f32;
            for u in 0..nv as usize {
                let p = [buf[u * 6], buf[u * 6 + 1], buf[u * 6 + 2]];
                let mut best = f32::MAX;
                for v in 0..sn as usize {
                    let d = ((p[0] - sbuf[v * 3]).powi(2)
                        + (p[1] - sbuf[v * 3 + 1]).powi(2)
                        + (p[2] - sbuf[v * 3 + 2]).powi(2))
                    .sqrt();
                    if d < best {
                        best = d;
                    }
                }
                worst2 = worst2.max(best);
            }
            println!("  worst nearest-vertex distance the other way: {worst2:.6}");
        }
    }

    // ---- the placement matrix ----
    let mut m = vec![0f32; 16];
    if bn_doodad_placement(w, fish_inst, m.as_mut_ptr()) == 1 {
        println!("placement matrix (row-major):");
        for r in 0..4 {
            println!(
                "  [{:.4} {:.4} {:.4} {:.4}]",
                m[r * 4],
                m[r * 4 + 1],
                m[r * 4 + 2],
                m[r * 4 + 3]
            );
        }
        // The tile centre (WoW yards) plus the tile-local offset: the fish's
        // absolute WoW position, so the host can point a camera at it.
        let mut c = vec![0f32; 2];
        if benilla::bn_tile_center(w, c.as_mut_ptr()) == 2 {
            let (cx, cy) = (c[0], c[1]);
            let wx = m[3] + cx;
            let wy = cy - m[7];
            let wz = m[11];
            println!("tile centre: ({cx:.1}, {cy:.1})");
            println!("fish world (WoW yards): ({wx:.1}, {wy:.1}, {wz:.1})");
        }
    } else {
        println!("bn_doodad_placement failed");
    }

    // ---- the pose cost (per frame, per model) ----
    if clip_n > 0 {
        let o = 12 + sub_n * 4;
        let seq = table[o];
        let start = table[o + 1];
        let mut buf = vec![0f32; 400_000];
        let cap = (buf.len() / 6) as u32;
        // Warm up.
        for k in 0..10 {
            bn_doodad_rig_pose(w, fish_c.as_ptr(), seq, start + k, buf.as_mut_ptr(), cap);
        }
        let n = 2000;
        let t0 = std::time::Instant::now();
        for k in 0..n {
            bn_doodad_rig_pose(
                w,
                fish_c.as_ptr(),
                seq,
                start + (k % 1000),
                buf.as_mut_ptr(),
                cap,
            );
        }
        let el = t0.elapsed();
        println!(
            "  pose cost: {:?} per call ({:.1} us), 100 calls = {:.0} us",
            el / n,
            el.as_secs_f64() * 1e6 / n as f64,
            el.as_secs_f64() * 1e6 * 100.0 / n as f64
        );
    }

    benilla::bn_shutdown(w);
}

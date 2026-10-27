// Detail probe for one ADT tile: terrain Z range, chunk layer textures, WMO
// instance count and (baked, tile-local) Z range. Used to explain why the UE
// render of Stormwind shows buildings floating above the ground.
fn main() {
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        println!("FFI: bn_init failed");
        return;
    }
    let map = std::ffi::CString::new("Azeroth").unwrap();

    for (tx, ty) in [(30i32, 48i32), (30, 47), (29, 46)] {
        println!("===== tile {tx}_{ty} =====");
        let chunks = benilla::bn_load_tile(w, map.as_ptr(), tx, ty);
        if chunks == 0 {
            println!("  load failed (no chunks)");
            continue;
        }
        let nv = benilla::bn_terrain_vertex_count(w);
        let ni = benilla::bn_terrain_index_count(w);
        let nc = benilla::bn_terrain_chunk_count(w);
        let mut verts = vec![
            benilla::BnVertex {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                nx: 0.0,
                ny: 0.0,
                nz: 0.0,
                u: 0.0,
                v: 0.0
            };
            nv as usize
        ];
        let written = benilla::bn_terrain_fill_vertices(w, verts.as_mut_ptr(), nv);
        let (mut mn, mut mx) = (f32::MAX, f32::MIN);
        let (mut xmn, mut xmx, mut ymn, mut ymx) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for v in &verts[..written as usize] {
            mn = mn.min(v.z);
            mx = mx.max(v.z);
            xmn = xmn.min(v.x);
            xmx = xmx.max(v.x);
            ymn = ymn.min(v.y);
            ymx = ymx.max(v.y);
        }
        println!(
            "  mcnk={chunks} verts={nv} idx={ni} chunks={nc} filled={written}\n  z=[{mn:.1},{mx:.1}] x=[{xmn:.1},{xmx:.1}] y=[{ymn:.1},{ymx:.1}]"
        );
        // Terrain height at the tile centre (0,0) and a few offsets.
        for (qx, qy) in [
            (0.0f32, 0.0f32),
            (-100.0, 100.0),
            (100.0, -100.0),
            (-250.0, 250.0),
        ] {
            println!(
                "  h({qx:.0},{qy:.0}) = {:.1}",
                benilla::bn_terrain_height_at(w, qx, qy)
            );
        }
        // Layer textures of the first chunks.
        let mut buf = [0i8; 260];
        for c in 0..nc.min(4) {
            let n = benilla::bn_terrain_chunk_layer_count(w, c);
            let mut names = Vec::new();
            for s in 0..n.min(4) {
                buf = [0; 260];
                let ok = benilla::bn_terrain_chunk_layer(w, c, s, buf.as_mut_ptr(), 260);
                let sname = std::ffi::CStr::from_bytes_until_nul(unsafe {
                    std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
                })
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
                names.push(format!("{ok}:{sname}"));
            }
            println!("  chunk{c} layers({n}) = {names:?}");
        }
        // WMO instances: count and baked z range of the first few.
        let wn = benilla::bn_wmo_instance_count(w);
        println!("  wmo instances = {wn}");
        for i in 0..wn.min(6) {
            let ok = benilla::bn_wmo_instance_load(w, i);
            let vn = benilla::bn_wmo_instance_vertex_count(w, i);
            let mut wv = vec![
                benilla::BnVertex {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    nx: 0.0,
                    ny: 0.0,
                    nz: 0.0,
                    u: 0.0,
                    v: 0.0
                };
                vn as usize
            ];
            let wf = benilla::bn_wmo_instance_fill_vertices(w, i, wv.as_mut_ptr(), vn);
            let (mut wmn, mut wmx) = (f32::MAX, f32::MIN);
            let (mut wxmn, mut wxmx, mut wymn, mut wymx) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            for v in &wv[..wf as usize] {
                wmn = wmn.min(v.z);
                wmx = wmx.max(v.z);
                wxmn = wxmn.min(v.x);
                wxmx = wxmx.max(v.x);
                wymn = wymn.min(v.y);
                wymx = wymx.max(v.y);
            }
            if ok != 0 && wf > 0 {
                println!(
                    "  wmo{i}: v={vn} filled={wf} z=[{wmn:.1},{wmx:.1}] x=[{wxmn:.1},{wxmx:.1}] y=[{wymn:.1},{wymx:.1}]"
                );
                // What does the city look like right above the tile centre?
                for (cx, cy) in [(0.0f32, 0.0f32), (100.0, 0.0), (0.0, 100.0), (-100.0, 0.0)] {
                    let (mut nmn, mut nmx, mut cnt) = (f32::MAX, f32::MIN, 0);
                    for v in &wv[..wf as usize] {
                        // UE axes: x, y = -wow_y; compare in tile-local yards.
                        if (v.x - cx).abs() < 20.0 && (v.y - (-cy)).abs() < 20.0 {
                            nmn = nmn.min(v.z);
                            nmx = nmx.max(v.z);
                            cnt += 1;
                        }
                    }
                    println!("    near({cx:.0},{cy:.0}): verts={cnt} z=[{nmn:.1},{nmx:.1}]");
                }
            }
        }
        let dn = benilla::bn_doodad_count(w);
        println!("  doodads = {dn}");
    }
    benilla::bn_shutdown(w);
}

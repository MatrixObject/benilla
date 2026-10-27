// Load a real ADT tile and report the flattened mesh stats.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let files = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect::<Vec<String>>();

    // Azeroth_26_27 exists in patch.MPQ's listfile.
    let maps = [("Azeroth", 26, 27), ("Kalimdor", 0, 0)];
    for (map, x, y) in maps {
        let path = format!("World\\Maps\\{map}\\{map}_{x}_{y}.adt");
        let mut bytes = None;
        for f in &files {
            let p = format!("{data}\\{f}");
            let a = match benilla_mpq::Archive::open(&p) {
                Ok(a) => a,
                Err(_) => continue,
            };
            if a.contains(&path) {
                match a.read_file(&path) {
                    Ok(b) => {
                        println!("{f}: {path} ({} bytes)", b.len());
                        bytes = Some(b);
                        break;
                    }
                    Err(e) => {
                        println!("{f}: {path} read error {e}");
                        break;
                    }
                }
            }
        }
        let Some(bytes) = bytes else {
            println!("{path}: NOT FOUND");
            continue;
        };
        match benilla_formats::adt_to_tile_mesh(&bytes) {
            Ok(t) => {
                let nv: usize = t.chunks.iter().map(|c| c.positions.len()).sum();
                let ni: usize = t.chunks.iter().map(|c| c.indices.len()).sum();
                println!(
                    "  chunks={} verts={} idx={} tris={} wmos={} doodads={} textures={:?}",
                    t.chunks.len(),
                    nv,
                    ni,
                    ni / 3,
                    t.wmos.len(),
                    t.doodads.len(),
                    t.chunks
                        .iter()
                        .filter_map(|c| c.base_texture.clone())
                        .take(3)
                        .collect::<Vec<_>>(),
                );
                if let Some(c0) = t.chunks.first() {
                    println!(
                        "  chunk0 pos[0]={:?} uvs={} normals={}",
                        c0.positions[0],
                        c0.uvs.len(),
                        c0.normals.len()
                    );
                }
            }
            Err(e) => println!("  adt_to_tile_mesh error: {e}"),
        }
    }

    // FFI roundtrip: bn_init -> bn_load_tile -> counts/fills.
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        println!("FFI: bn_init failed");
        return;
    }
    let map = std::ffi::CString::new("Azeroth").unwrap();
    let chunks = benilla::bn_load_tile(w, map.as_ptr(), 26, 27);
    let nv = benilla::bn_terrain_vertex_count(w);
    let ni = benilla::bn_terrain_index_count(w);
    let nc = benilla::bn_terrain_chunk_count(w);
    println!("FFI: mcnk={} verts={} idx={} chunks={}", chunks, nv, ni, nc);
    let mut r = [0u32; 4];
    let ok = benilla::bn_terrain_chunk_range(w, 0, r.as_mut_ptr());
    let mut tex = [0i8; 260];
    let ok2 = benilla::bn_terrain_chunk_texture(w, 0, tex.as_mut_ptr(), 260);
    let texs = std::ffi::CStr::from_bytes_until_nul(unsafe {
        std::slice::from_raw_parts(tex.as_ptr() as *const u8, tex.len())
    })
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default();
    println!(
        "FFI: range_ok={ok} range={r:?} tex_ok={ok2} tex={texs} h(0,0)={}",
        benilla::bn_terrain_height_at(w, 0.0, 0.0)
    );
    let mut verts = vec![
        benilla::BnVertex {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            nx: 0.0,
            ny: 0.0,
            nz: 0.0,
            u: 0.0,
            v: 0.0,
        };
        nv as usize
    ];
    let written = benilla::bn_terrain_fill_vertices(w, verts.as_mut_ptr(), nv);
    let mut idx = vec![0u16; ni as usize];
    let written_i = benilla::bn_terrain_fill_indices(w, idx.as_mut_ptr(), ni);
    let (mut mn, mut mx) = (f32::MAX, f32::MIN);
    for v in &verts[..written as usize] {
        mn = mn.min(v.z);
        mx = mx.max(v.z);
    }
    let maxidx = idx[..written_i as usize].iter().copied().max().unwrap_or(0);
    println!(
        "FFI: filled v={} i={} z=[{mn:.1},{mx:.1}] maxidx={} (nv={})",
        written, written_i, maxidx, nv
    );
    benilla::bn_shutdown(w);
}

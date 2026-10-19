// Find an Azeroth tile with MODF WMO instances (Stormwind vicinity) and walk
// the WMO FFI end-to-end: load -> bake -> counts -> submesh ranges -> textures.

fn tex_of(w: *mut benilla::BnWorld, i: u32, j: u32) -> String {
    let mut buf = [0i8; 512];
    if benilla::bn_wmo_instance_texture(w, i, j, buf.as_mut_ptr(), buf.len() as u32) == 0 {
        return String::new();
    }
    std::ffi::CStr::from_bytes_until_nul(unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
    })
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_default()
}

fn main() {
    let dir = std::ffi::CString::new(
        "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data",
    )
    .unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        println!("bn_init failed");
        return;
    }

    // Stormwind (-8840.56, 489.7) -> tile ~(48, 31); the filename order is
    // ambiguous, so sweep both orders and the +/-1 neighborhood.
    let mut found: Option<(i32, i32)> = None;
    'outer: for order in 0..2 {
        for dx in -1..=1i32 {
            for dy in -1..=1i32 {
                let (a, b) = if order == 0 {
                    (48 + dx, 31 + dy)
                } else {
                    (31 + dx, 48 + dy)
                };
                let map = std::ffi::CString::new("Azeroth").unwrap();
                let chunks = benilla::bn_load_tile(w, map.as_ptr(), a, b);
                if chunks == 0 {
                    continue;
                }
                let n = benilla::bn_wmo_instance_count(w);
                println!("Azeroth_{a}_{b}: mcnk={chunks} wmos={n}");
                if n > 0 {
                    found = Some((a, b));
                    break 'outer;
                }
            }
        }
    }

    let Some((tx, ty)) = found else {
        println!("no tile with WMO instances found near Stormwind");
        benilla::bn_shutdown(w);
        return;
    };
    println!("== using Azeroth_{tx}_{ty} ==");

    let total = benilla::bn_wmo_instance_count(w);
    let show = total.min(6);
    for i in 0..show {
        let subs = benilla::bn_wmo_instance_load(w, i);
        let nv = benilla::bn_wmo_instance_vertex_count(w, i);
        let ni = benilla::bn_wmo_instance_index_count(w, i);
        println!("inst {i}: submeshes={subs} verts={nv} idx={ni} tris={}", ni / 3);
        if subs == 0 || nv == 0 {
            continue;
        }
        // Bounds of the baked tile-local UE geometry.
        let mut verts = vec![benilla::BnVertex {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            nx: 0.0,
            ny: 0.0,
            nz: 0.0,
            u: 0.0,
            v: 0.0,
        }; nv as usize];
        let written = benilla::bn_wmo_instance_fill_vertices(w, i, verts.as_mut_ptr(), nv);
        let mut tris = vec![benilla::BnTriangle {
            a: 0,
            b: 0,
            c: 0,
            material: 0,
            flags: 0,
        }; ni as usize];
        let written_t = benilla::bn_wmo_instance_fill_triangles(w, i, tris.as_mut_ptr(), ni);
        let (mut mnx, mut mxx, mut mny, mut mxy, mut mnz, mut mxz) = (
            f32::MAX,
            f32::MIN,
            f32::MAX,
            f32::MIN,
            f32::MAX,
            f32::MIN,
        );
        for v in &verts[..written as usize] {
            mnx = mnx.min(v.x);
            mxx = mxx.max(v.x);
            mny = mny.min(v.y);
            mxy = mxy.max(v.y);
            mnz = mnz.min(v.z);
            mxz = mxz.max(v.z);
        }
        let maxidx = tris[..written_t as usize]
            .iter()
            .map(|t| t.a.max(t.b).max(t.c))
            .max()
            .unwrap_or(0);
        let two_sided = tris[..written_t as usize].iter().filter(|t| t.flags & 1 != 0).count();
        println!(
            "  filled v={} t={} bounds x=[{mnx:.0},{mxx:.0}] y=[{mny:.0},{mxy:.0}] z=[{mnz:.0},{mxz:.0}] maxidx={maxidx} two_sided_tris={two_sided}",
            written, written_t
        );
        let show_subs = subs.min(6);
        for j in 0..show_subs {
            let mut r = [0u32; 4];
            let ok = benilla::bn_wmo_instance_submesh(w, i, j, r.as_mut_ptr());
            println!("  sub {j}: ok={ok} vtx0={} vtx_n={} idx0={} idx_n={} tex={}", r[0], r[1], r[2], r[3], tex_of(w, i, j));
        }
        if subs > show_subs {
            println!("  ... {} more submeshes", subs - show_subs);
        }
    }
    benilla::bn_shutdown(w);
}

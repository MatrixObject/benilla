// Dump the MOMT WINDOW (0x20) batches of every WMO placement around a spot.
// Usage: cargo run --release --example winprobe -- <x> <y> [radiusTiles]

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
    let wx: f32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(-9461.0);
    let wy: f32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(98.0);
    let rad: i32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(1);
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        println!("bn_init failed");
        return;
    }
    // WoW tile index: x + 32 (west->east), 32 - y (north->south).
    // Tile name Azeroth_<row>_<col>: col = 32 - x/533.33, row = 32 - y/533.33.
    let tx = (32.0 - wx / 533.333) as i32;
    let ty = (32.0 - wy / 533.333) as i32;
    println!("spot ({wx},{wy}) -> tile ({tx},{ty})");
    let map = std::ffi::CString::new("Azeroth").unwrap();
    for dx in -rad..=rad {
        for dy in -rad..=rad {
            let chunks = benilla::bn_load_tile(w, map.as_ptr(), ty + dy, tx + dx);
            println!("tile ({},{}) chunks={chunks}", ty + dy, tx + dx);
        }
    }
    let n = benilla::bn_wmo_instance_count(w);
    println!("instances={n}");
    for i in 0..n {
        let mut pos = [0.0f32; 3];
        let ok = benilla::bn_wmo_instance_position(w, i, pos.as_mut_ptr());
        if ok == 0 {
            continue;
        }
        let subs = benilla::bn_wmo_instance_load(w, i);
        if subs == 0 {
            continue;
        }
        let near = true;
        println!("== inst {i} pos {:?} submeshes {subs}", pos);
        for j in 0..subs {
            let mut r = [0u32; 8];
            if benilla::bn_wmo_instance_submesh(w, i, j, r.as_mut_ptr()) == 0 {
                continue;
            }
            let mut blend = 0u32;
            benilla::bn_wmo_instance_blend(w, i, j, &mut blend);
            println!(
                "   sub {j}: tris={} grp={} class={} interior={} sag={} window={} blend={:#x} tex={}",
                r[3] / 3,
                r[4],
                r[5],
                r[6] & 1,
                (r[6] >> 1) & 1,
                r[7] & 1,
                blend,
                tex_of(w, i, j)
            );
        }
    }
    benilla::bn_shutdown(w);
}

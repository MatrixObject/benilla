// Probe: the mode-2 (dynamic terrain) shadow, measured from the middle layer.
// Prints, per ADT, how many 64x64 alpha texels carry MCSH shade in their A
// channel (`bn_terrain_chunk_alpha`: A = 255 inside a shadow bit, 0 outside).
//
// The previous round measured Azeroth 31,49 at 72% texel occlusion; this is the
// same measurement, so "the shadow came back / went away" is a number.

use benilla::*;
use std::ffi::c_char;

extern "C" {
    fn bn_load_tile(world: *mut BnWorld, map: *const c_char, x: i32, z: i32) -> u32;
    fn bn_terrain_chunk_count(world: *const BnWorld) -> u32;
    fn bn_terrain_chunk_alpha(world: *const BnWorld, i: u32, out: *mut u8, cap: u32) -> u32;
}

fn probe(world: *mut BnWorld, map: &str, tx: i32, ty: i32) {
    let cmap = std::ffi::CString::new(map).unwrap();
    let n = unsafe { bn_load_tile(world, cmap.as_ptr(), tx, ty) };
    if n == 0 {
        return;
    }
    let chunks = unsafe { bn_terrain_chunk_count(world as *const _) };
    let mut buf = vec![0u8; 64 * 64 * 4];
    let mut texels = 0u64;
    let mut full = 0u64; // A == 255
    let mut dark = 0u64; // A > 32
    let mut sum = 0u64;
    let mut with = 0u32; // chunks with any A > 0
    let mut none = 0u32; // chunks with A all 0
    let mut no_map = 0u32; // bn_terrain_chunk_alpha returned 0
    let mut max_per_chunk: Vec<u32> = Vec::new();
    for i in 0..chunks {
        let w = unsafe {
            bn_terrain_chunk_alpha(world as *const _, i, buf.as_mut_ptr(), buf.len() as u32)
        };
        if w == 0 {
            no_map += 1;
            continue;
        }
        let mut local_max = 0u32;
        let mut local_any = false;
        for k in 0..(w as usize / 4) {
            let a = buf[k * 4 + 3] as u64;
            texels += 1;
            sum += a;
            if a == 255 {
                full += 1;
            }
            if a > 32 {
                dark += 1;
            }
            if a > 0 {
                local_any = true;
            }
            local_max = local_max.max(a as u32);
        }
        if local_any {
            with += 1;
        } else {
            none += 1;
        }
        max_per_chunk.push(local_max);
    }
    max_per_chunk.sort_unstable();
    let med = max_per_chunk.get(max_per_chunk.len() / 2).copied().unwrap_or(0);
    let pct = |v: u64| if texels == 0 { 0.0 } else { 100.0 * v as f64 / texels as f64 };
    println!(
        "Azeroth_{tx}_{ty}: chunks={chunks} (no_alpha={no_map}) texels={texels} \
         A255={:.2}%  A>32={:.2}%  meanA={:.1}  chunks_with_shadow={with}  all_zero={none}  \
         median_chunk_max={med}",
        pct(full),
        pct(dark),
        if texels == 0 { 0.0 } else { sum as f64 / texels as f64 },
    );
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = unsafe { bn_init(cdir.as_ptr()) };
    assert!(!world.is_null(), "bn_init");

    // The cited tile first, then the 5x5 around it (Goldshire/Duskwood road).
    println!("== cited tile ==");
    probe(world, "Azeroth", 31, 49);
    println!("== 5x5 around 31,49 ==");
    for ty in 47..=51 {
        for tx in 29..=33 {
            probe(world, "Azeroth", tx, ty);
        }
    }
    println!("== 5x5 around 32,50 (Goldshire at -9418,75) ==");
    for ty in 48..=52 {
        for tx in 30..=34 {
            probe(world, "Azeroth", tx, ty);
        }
    }
}

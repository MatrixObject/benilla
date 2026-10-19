// Probe: ADT liquid FFI per tile - counts, kinds, index ranges sanity.
use benilla::*;

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let cdir = std::ffi::CString::new(data_dir.clone()).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let cworld = world as *const BnWorld;
    let tiles = [
        ("Azeroth", 32, 48), // Crystal Lake: expect 42 still meshes
        ("Azeroth", 30, 47), // Stormwind canals
        ("Azeroth", 30, 48),
        ("Azeroth", 33, 46), // Burning Steppes: magma
        ("Azeroth", 41, 41), // Baradin Bay: ocean
    ];
    for (map, x, z) in tiles {
        let cmap = std::ffi::CString::new(map).unwrap();
        if bn_load_tile(world, cmap.as_ptr(), x, z) == 0 {
            println!("{map}_{x}_{z}: load FAILED");
            continue;
        }
        let vn = bn_liquid_vertex_count(cworld);
        let ic = bn_liquid_index_count(cworld);
        let n = bn_liquid_count(cworld);
        let mut kinds = [0u32; 3];
        let mut bad = 0u32;
        let mut verts: Vec<BnVertex> = vec![unsafe { std::mem::zeroed() }; 65536];
        let mut idx = vec![0u16; 200_000];
        let wv = bn_liquid_fill_vertices(cworld, verts.as_mut_ptr(), verts.len() as u32);
        let wi = bn_liquid_fill_indices(cworld, idx.as_mut_ptr(), idx.len() as u32);
        for i in 0..n {
            let mut r = [0u32; 3];
            if bn_liquid_range(cworld, i, r.as_mut_ptr()) == 0 {
                bad += 1;
                continue;
            }
            let (idx0, idx_n, kind) = (r[0], r[1], r[2]);
            if kind < 3 {
                kinds[kind as usize] += 1;
            }
            if idx_n == 0 || idx_n % 6 != 0 || idx0 + idx_n > ic || kind > 2 {
                bad += 1;
            }
        }
        assert_eq!(wv, vn, "vertex fill count mismatch");
        assert_eq!(wi, ic, "index fill count mismatch");
        println!(
            "{map}_{x}_{z}: meshes={n} verts={vn} idx={ic} \
             water={} ocean={} magma={} bad={}",
            kinds[0], kinds[1], kinds[2], bad
        );
    }
    bn_shutdown(world);
}

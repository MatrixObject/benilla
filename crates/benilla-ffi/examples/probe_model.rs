// POC probe: verify the model FFI returns real vertices AND real skin-index
// triangles for a known 1.12.1 M2 model, without the UE editor in the loop.
use std::ptr;

// Mirror the Rust FFI entry points through the public crate so we exercise the
// exact code that benilla.dll exports.
use benilla::*;

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data".to_string());

    let cdir = std::ffi::CString::new(data_dir.clone()).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed for {data_dir}");
        std::process::exit(1);
    }
    let archives = bn_num_archives(world);
    println!("archives: {archives}");

    // A handful of real models we know exist in the 1.12.1 chain.
    let models = [
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Interface\\Buttons\\TalkToMe.m2",
        "Creature\\Wolf\\Wolf.m2",
    ];
    let cworld = world as *const BnWorld;
    let mut cap = 1_000_000u32;
    let mut vbuf: Vec<BnVertex> = vec![unsafe { std::mem::zeroed() }; cap as usize];
    let mut tbuf: Vec<BnTriangle> = vec![unsafe { std::mem::zeroed() }; cap as usize];

    for m in &models {
        let cm = std::ffi::CString::new(*m).unwrap();
        let vc = bn_model_vertex_count(cworld, cm.as_ptr());
        if vc == 0 {
            println!("{m:<45} NOT FOUND (0 vertices)");
            continue;
        }
        let filled = bn_model_fill(
            cworld,
            cm.as_ptr(),
            vbuf.as_mut_ptr(),
            tbuf.as_mut_ptr(),
            cap,
        );
        println!("{m:<45} vertices={vc}  triangles={filled}");
    }

    let _ = ptr::null::<u8>();
    bn_shutdown(world);
}

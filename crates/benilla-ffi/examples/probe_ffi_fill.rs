// Verify FFI skinning: call the Rust API directly (not extern "C") and dump
// the resulting vertex coords to confirm the bounding box is now expanded
// (pivots applied) instead of clustered at the origin.
use std::ffi::CString;

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    // The crate name is "benilla-ffi" but the lib name is "benilla".
    // From an example in the same crate, use the crate module path.
    let cdata = CString::new(data).unwrap();
    let world = benilla::bn_init(cdata.as_ptr());
    if world.is_null() {
        eprintln!("bn_init returned null");
        std::process::exit(1);
    }

    let models = [
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Interface\\Buttons\\TalkToMe.m2",
        "Creature\\Wolf\\Wolf.m2",
    ];

    for m in &models {
        let cm = CString::new(*m).unwrap();
        let vc = benilla::bn_model_vertex_count(world, cm.as_ptr());
        if vc == 0 {
            println!("{m}: NOT FOUND");
            continue;
        }
        let mut vb: Vec<benilla::BnVertex> = vec![unsafe { std::mem::zeroed() }; vc as usize];
        let mut tb: Vec<benilla::BnTriangle> = vec![unsafe { std::mem::zeroed() }; vc as usize];
        let nt = benilla::bn_model_fill(world, cm.as_ptr(), vb.as_mut_ptr(), tb.as_mut_ptr(), vc);

        let (mut minx, mut miny, mut minz) = (f32::MAX, f32::MAX, f32::MAX);
        let (mut maxx, mut maxy, mut maxz) = (f32::MIN, f32::MIN, f32::MIN);
        for v in &vb {
            minx = minx.min(v.x);
            miny = miny.min(v.y);
            minz = minz.min(v.z);
            maxx = maxx.max(v.x);
            maxy = maxy.max(v.y);
            maxz = maxz.max(v.z);
        }
        println!(
            "{m}: verts={vc} tris={nt}\n  bbox min=({minx:.1},{miny:.1},{minz:.1}) max=({maxx:.1},{maxy:.1},{maxz:.1})\n  span=({:.1},{:.1},{:.1})",
            maxx - minx, maxy - miny, maxz - minz
        );
    }

    benilla::bn_shutdown(world);
}

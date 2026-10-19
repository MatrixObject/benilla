// Probe the new attachment/bone FFI exports against real 1.12.1 mounts.
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
    let cworld = world as *const BnWorld;

    let mounts = [
        "Creature\\RidingHorse\\RidingHorse.m2",
        "Creature\\RidingHorse\\BrownHorse.m2",
        "Creature\\RidingHorse\\WhiteHorse.m2",
    ];

    for m in &mounts {
        let cm = std::ffi::CString::new(*m).unwrap();
        let vc = bn_model_vertex_count(cworld, cm.as_ptr());
        if vc == 0 {
            println!("{m:<50} NOT FOUND");
            continue;
        }
        let atts = bn_model_attachment_count(cworld, cm.as_ptr());
        let bones = bn_model_bone_count(cworld, cm.as_ptr());
        println!("{m:<50} vertices={vc} attachments={atts} bones={bones}");
        for i in 0..atts.min(6) {
            let mut id: u16 = 0;
            let mut bone: u16 = 0;
            let mut pos = [0f32; 3];
            let ok = bn_model_attachment_info(cworld, cm.as_ptr(), i, &mut id, &mut bone, pos.as_mut_ptr());
            if ok == 1 {
                println!("   att[{i}] id={id} bone={bone} pos=({:.3},{:.3},{:.3})", pos[0], pos[1], pos[2]);
            }
        }
        // Sample bone rest positions if any.
        for bi in 0..bones.min(4) {
            let mut bp = [0f32; 3];
            let ok = bn_model_bone_rest_position(cworld, cm.as_ptr(), bi, bp.as_mut_ptr());
            if ok == 1 {
                println!("   bone[{bi}] rest=({:.3},{:.3},{:.3})", bp[0], bp[1], bp[2]);
            }
        }
    }

    bn_shutdown(world);
}

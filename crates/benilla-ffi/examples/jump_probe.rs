// Probe: the jump. Two questions, both answered from the middle layer.
//
// 1. Which AnimationData ids does the player's own M2 author, and does the
//    airborne pair (37 rising / 40 falling) resolve at all, with what duration
//    and loop flag? A miss means the host keeps the ground clip mid-air.
// 2. What arc does the mover produce for one Space press on flat ground? The
//    laws say JUMP_SPEED 7.955547 yd/s against GRAVITY 19.29 yd/s^2, so the
//    feet should rise 1.640 yd and be back down 0.825 s later.

use benilla::*;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PlayerState {
    pos: [f32; 3],
    yaw: f32,
    cam_eye: [f32; 3],
    cam_target: [f32; 3],
    cam_pitch: f32,
    cam_distance: f32,
    speed: f32,
    grounded: u32,
    mounted: u32,
    anim: u32,
    overlay_anim: u32,
    overlay_weight: f32,
}

extern "C" {
    fn bn_player_reset(world: *mut BnWorld, map: *const std::ffi::c_char, x: f32, y: f32) -> u32;
    fn bn_player_input(
        world: *mut BnWorld,
        forward: f32,
        strafe: f32,
        turn: f32,
        jump: u32,
        walk: u32,
        up: f32,
    ) -> u32;
    fn bn_player_tick(world: *mut BnWorld, dt: f32, out: *mut PlayerState) -> u32;
    fn bn_model_anim_by_id(
        world: *const BnWorld,
        model_path: *const std::ffi::c_char,
        anim_id: u32,
        out: *mut u32,
    ) -> u32;
}

const IDS: [(&str, u32); 8] = [
    ("Stand", 0),
    ("Run", 5),
    ("JumpStart", 37),
    ("Jump", 38),
    ("JumpEnd", 39),
    ("Fall", 40),
    ("JumpLandRun", 187),
    ("Mount", 91),
];

fn anims(world: *const BnWorld, path: &str) {
    let cpath = std::ffi::CString::new(path).unwrap();
    print!("== {path}\n");
    for (name, id) in IDS {
        let mut out = [0u32; 5];
        let hit = unsafe { bn_model_anim_by_id(world, cpath.as_ptr(), id, out.as_mut_ptr()) };
        if hit == 1 {
            println!(
                "  id{id:4} {name:<14} seq={} start={}ms end={}ms dur={}ms loop={}",
                out[0], out[1], out[2], out[3], out[4]
            );
        } else {
            println!("  id{id:4} {name:<14} MISS (caller keeps its current sequence)");
        }
    }
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let cdir = std::ffi::CString::new(data_dir.clone()).unwrap();
    let world = unsafe { bn_init(cdir.as_ptr()) };
    assert!(!world.is_null(), "bn_init");

    // The exact strings the host builds (`UBnBench::BnPlayerModelPath`) plus the
    // install's own `Character\Humanoid\...` spelling, so a path mismatch is
    // visible rather than inferred.
    for path in [
        r"Character\Human\Male\HumanMale.m2",
        r"Character\Humanoid\Human\Male\HumanMale.m2",
        r"Character\Orc\Male\OrcMale.m2",
        r"Creature\RidingHorse\RidingHorse.m2",
    ] {
        anims(world as *const BnWorld, path);
    }

    // A spot with terrain under it: the reset drops the feet on the first floor.
    let spots = [
        ("Elwynn Forest", -8914.0f32, 436.0f32),
        ("Durotar", -2903.0, -9103.0),
        ("Redridge Mountains", -286.0, -450.0),
        ("Azshara", 2950.0, -7800.0),
    ];
    let mut st = PlayerState::default();
    let mut used = "";
    for (map, x, y) in spots {
        let cmap = std::ffi::CString::new(map).unwrap();
        let ok = unsafe { bn_player_reset(world, cmap.as_ptr(), x, y) };
        unsafe { bn_player_input(world, 0.0, 0.0, 0.0, 0, 0, 0.0) };
        for _ in 0..30 {
            unsafe { bn_player_tick(world, 1.0 / 60.0, &mut st) };
        }
        println!(
            "-- reset {map} ({x}, {y}) -> ok={ok} z={:.3} grounded={} anim={}",
            st.pos[2], st.grounded, st.anim
        );
        if ok == 1 && st.grounded == 1 && st.pos[2].is_finite() {
            used = map;
            break;
        }
    }
    if used.is_empty() {
        println!("no spot grounded: the arc below is meaningless");
    }

    // One press: jump held for a single tick, then released, at a fixed 60 Hz.
    let mut apex = 0.0f32;
    let mut airtime = 0.0f32;
    let mut ids: Vec<u32> = Vec::new();
    let dt = 1.0 / 60.0;
    let z0 = st.pos[2];
    for i in 0..120 {
        let jump = if i == 0 { 1 } else { 0 };
        unsafe { bn_player_input(world, 0.0, 0.0, 0.0, jump, 0, 0.0) };
        if unsafe { bn_player_tick(world, dt, &mut st) } == 0 {
            println!("player_tick returned 0 at {i}");
            break;
        }
        apex = apex.max(st.pos[2] - z0);
        if st.grounded == 0 {
            airtime += dt;
        }
        if ids.last() != Some(&st.anim) {
            ids.push(st.anim);
        }
        if i < 66 {
            println!(
                "t={:5.3} z=+{:.4} g={} anim={:<3} spd={:.2}",
                i as f32 * dt,
                st.pos[2] - z0,
                st.grounded,
                st.anim,
                st.speed
            );
        }
    }
    println!(
        "== arc: apex {apex:.3} yd, airtime {airtime:.3} s, anim sequence {:?}",
        ids
    );
    println!(
        "== laws: apex {:.3} yd, airtime {:.3} s",
        7.955547f32 * 7.955547 / (2.0 * 19.29),
        2.0 * 7.955547 / 19.29
    );
}

// Probe: Space in water, after the surface-only jump deviation.
//
// Reference `0x7c6230` takes a swimmer at any depth. The middle layer now
// leaps only while the body swims at the surface (its head, the capsule top,
// not under the waterline); a submerged press is dropped. This probe drives
// the real data:
//   - a body dropped on a lakebed swims deep: a press must not launch it;
//   - a body settled at the floating rest line: a press must leap it out of
//     the water (about 2 yd) and the swim must re-latch on the way down.

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
    mount_display: u32,
    mount_anim: u32,
    anim: u32,
    overlay_anim: u32,
    overlay_weight: f32,
    overlay_anim_ms: u32,
    twist_spine: f32,
    twist_head: f32,
    aim_yaw: f32,
    cam_yaw: f32,
    axes: [f32; 3],
}

extern "C" {
    fn bn_player_reset(world: *mut BnWorld, map: *const std::ffi::c_char, x: f32, y: f32) -> u32;
    fn bn_player_teleport(
        world: *mut BnWorld,
        map: *const std::ffi::c_char,
        x: f32,
        y: f32,
        z: f32,
        yaw: f32,
    ) -> u32;
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
}

const DT: f32 = 1.0 / 60.0;
const SWIM_IDLE: u32 = 41;

fn input(world: *mut BnWorld, jump: bool) {
    unsafe { bn_player_input(world, 0.0, 0.0, 0.0, jump as u32, 0, 0.0) };
}

fn tick(world: *mut BnWorld, st: &mut PlayerState) {
    unsafe { bn_player_tick(world, DT, st) };
}

fn run(world: *mut BnWorld, st: &mut PlayerState, n: usize) {
    for _ in 0..n {
        tick(world, st);
    }
}

/// One press: down for a frame, up for the rest. Prints the arc above `z0`.
fn press(world: *mut BnWorld, st: &mut PlayerState, z0: f32, n: usize) {
    let mut apex = 0.0f32;
    let mut seen = Vec::new();
    for i in 0..n {
        input(world, i == 0);
        tick(world, st);
        apex = apex.max(st.pos[2] - z0);
        if seen.last() != Some(&st.anim) {
            seen.push(st.anim);
        }
    }
    input(world, false);
    println!(
        "   press from z={z0:.3}: apex +{apex:.3} yd, final z={:.3}, anim {:?}",
        st.pos[2], seen
    );
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = unsafe { bn_init(cdir.as_ptr()) };
    assert!(!world.is_null(), "bn_init");

    let map = std::ffi::CString::new("Kalimdor").unwrap();
    let (x, y) = (1983.97f32, -2875.84f32);
    let mut st = PlayerState::default();

    let ok = unsafe { bn_player_reset(world, map.as_ptr(), x, y) };
    run(world, &mut st, 120);
    println!(
        "== reset ({x}, {y}) ok={ok}: z={:.3} anim={} grounded={} (SwimIdle={SWIM_IDLE})",
        st.pos[2], st.anim, st.grounded
    );
    if st.anim != SWIM_IDLE {
        println!("== not swimming there: the numbers below mean nothing at this spot");
    }

    // 1. The lakebed body: deep under the surface, a press must do nothing.
    let bed = st.pos[2];
    press(world, &mut st, bed, 60);

    // 2. Find the surface: drop the body from rising heights over the bed.
    //    Under water it holds the teleport height; above it, it falls in and
    //    the swim latch stops it within a frame of the rest line. The highest
    //    settled Z is the nearest the body gets to the surface.
    let mut best = (f32::NEG_INFINITY, 0.0f32);
    for h in [3.0f32, 5.0, 8.0, 12.0, 16.0, 24.0] {
        unsafe { bn_player_teleport(world, map.as_ptr(), x, y, bed + h, 0.0) };
        run(world, &mut st, 300);
        println!(
            "   drop from +{h:.0}: z={:.3} anim={} grounded={}",
            st.pos[2], st.anim, st.grounded
        );
        if st.anim == SWIM_IDLE && st.pos[2] > best.0 {
            best = (st.pos[2], bed + h);
        }
    }
    println!(
        "== nearest the surface: z={:.3} (from +{:.0} over the bed); rest line is \
         surface - 1.521",
        best.0,
        best.1 - bed
    );

    // 3. The surface body: a press must leap out of the water and the swim
    //    must re-latch on the way down.
    if best.0.is_finite() {
        unsafe { bn_player_teleport(world, map.as_ptr(), x, y, best.1, 0.0) };
        run(world, &mut st, 300);
        let z0 = st.pos[2];
        press(world, &mut st, z0, 150);
    }
}

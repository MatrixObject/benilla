// Probe: the "lame leg / jump does nothing" report, measured from the middle
// layer. Runs the mover on the exact spot from `bnfall.log`
// (-9418, 75 — Goldshire's road) and reports per-frame `grounded`, `anim`
// (AnimationData id: 4 Walk, 5 Run, 37/38/39 jump, 40 Fall) and `speed`, so the
// oscillation the user sees as a dragging leg is a number, not an opinion.
//
// Also presses Space on flat ground and prints the arc: apex, airtime and the
// anim sequence. A jump that "does nothing" shows as grounded staying 1 and
// z never changing.

use benilla::*;
use std::ffi::c_char;

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
    mount_pitch: f32,
    mount_roll: f32,
}

extern "C" {
    fn bn_player_reset(world: *mut BnWorld, map: *const c_char, x: f32, y: f32) -> u32;
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

fn anim_name(a: u32) -> &'static str {
    match a {
        0 => "Stand",
        4 => "Walk",
        5 => "Run",
        37 => "JumpStart",
        38 => "Jump",
        39 => "JumpEnd",
        40 => "Fall",
        187 => "JumpLandRun",
        _ => "?",
    }
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let map = std::env::args().nth(2).unwrap_or_else(|| "Elwynn Forest".to_string());
    let x: f32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(-9418.0);
    let y: f32 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(75.0);

    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = unsafe { bn_init(cdir.as_ptr()) };
    assert!(!world.is_null(), "bn_init");
    let cmap = std::ffi::CString::new(map.clone()).unwrap();
    let ok = unsafe { bn_player_reset(world, cmap.as_ptr(), x, y) };
    let dt = 1.0 / 60.0;
    let mut st = PlayerState::default();

    // Sweep mode: find the "some places" where a running body loses its floor.
    // For a grid around the spawn, reset, run forward 3 s, and report the
    // longest run of grounded==0 frames (a Fall clip the user reads as a
    // dragging leg, and a Space press the mover refuses).
    if std::env::args().nth(5).as_deref() == Some("sweep") {
        let mut worst: Vec<(u32, f32, f32, f32, f32, u32)> = Vec::new();
        for gy in 0..8 {
            for gx in 0..8 {
                let sx = x + gx as f32 * 40.0;
                let sy = y + gy as f32 * 40.0;
                let c = std::ffi::CString::new(map.clone()).unwrap();
                if unsafe { bn_player_reset(world, c.as_ptr(), sx, sy) } == 0 {
                    continue;
                }
                for _ in 0..30 {
                    unsafe { bn_player_input(world, 0.0, 0.0, 0.0, 0, 0, 0.0) };
                    unsafe { bn_player_tick(world, dt, &mut st) };
                }
                let (z0, g0) = (st.pos[2], st.grounded);
                let mut streak = 0u32;
                let mut max_streak = 0u32;
                let mut z_at = st.pos[2];
                let mut z_drop = 0.0f32;
                let mut held_g = st.grounded;
                for _ in 0..180 {
                    unsafe { bn_player_input(world, 1.0, 0.0, 0.0, 0, 0, 0.0) };
                    unsafe { bn_player_tick(world, dt, &mut st) };
                    if st.grounded == 0 {
                        if streak == 0 {
                            z_at = st.pos[2];
                        }
                        streak += 1;
                        if streak > max_streak {
                            max_streak = streak;
                            z_drop = z_at - st.pos[2];
                        }
                    } else {
                        if streak > 0 {
                            held_g = st.grounded;
                        }
                        streak = 0;
                    }
                }
                if max_streak > 6 {
                    worst.push((max_streak, sx, sy, z0, z_at, held_g));
                }
            }
        }
        worst.sort_by(|a, b| b.0.cmp(&a.0));
        println!("== sweep: {} spots with an airborne streak > 6 frames", worst.len());
        for (n, sx, sy, z0, za, g) in worst.iter().take(15) {
            println!(
                "  ({sx:.0},{sy:.0}) z0={z0:.2} airborne {n} frames from z={za:.2} grounded_after={g}"
            );
        }
        return;
    }

    for _ in 0..60 {
        unsafe { bn_player_input(world, 0.0, 0.0, 0.0, 0, 0, 0.0) };
        unsafe { bn_player_tick(world, dt, &mut st) };
    }
    println!(
        "== reset {map} ({x},{y}) ok={ok} -> z={:.3} grounded={} anim={}({}) speed={:.2}",
        st.pos[2],
        st.grounded,
        anim_name(st.anim),
        st.anim,
        st.speed
    );

    // --- phase A: run straight for 20 s, count the airborne frames ---------
    let mut air = 0u32;
    let mut trans = 0u32;
    let mut prev_g = st.grounded;
    let mut zmin = st.pos[2];
    let mut zmax = st.pos[2];
    let mut shown = 0;
    let mut anims: Vec<u32> = Vec::new();
    for i in 0..1200 {
        unsafe { bn_player_input(world, 1.0, 0.0, 0.0, 0, 0, 0.0) };
        if unsafe { bn_player_tick(world, dt, &mut st) } == 0 {
            println!("tick 0 at {i}");
            break;
        }
        zmin = zmin.min(st.pos[2]);
        zmax = zmax.max(st.pos[2]);
        if st.grounded == 0 {
            air += 1;
        }
        if st.grounded != prev_g {
            trans += 1;
            if shown < 12 {
                println!(
                    "  t={:6.3} g {}->{} z={:.3} anim={} speed={:.2}",
                    i as f32 * dt,
                    prev_g,
                    st.grounded,
                    st.pos[2],
                    anim_name(st.anim),
                    st.speed
                );
                shown += 1;
            }
            prev_g = st.grounded;
        }
        if anims.last() != Some(&st.anim) {
            anims.push(st.anim);
        }
    }
    println!(
        "== run 20s: airborne {air}/1200 frames ({:.1}%), grounded transitions {trans}, \
         z {zmin:.3}..{zmax:.3} (range {:.3}), anims {:?}",
        air as f32 / 12.0,
        zmax - zmin,
        anims.iter().map(|a| anim_name(*a)).collect::<Vec<_>>()
    );

    // --- phase B: stop, settle, one Space press ----------------------------
    for _ in 0..60 {
        unsafe { bn_player_input(world, 0.0, 0.0, 0.0, 0, 0, 0.0) };
        unsafe { bn_player_tick(world, dt, &mut st) };
    }
    let z0 = st.pos[2];
    let g0 = st.grounded;
    let mut apex = 0.0f32;
    let mut airtime = 0.0f32;
    let mut seq: Vec<u32> = Vec::new();
    for i in 0..90 {
        let jump = if i == 0 { 1 } else { 0 };
        unsafe { bn_player_input(world, 0.0, 0.0, 0.0, jump, 0, 0.0) };
        unsafe { bn_player_tick(world, dt, &mut st) };
        apex = apex.max(st.pos[2] - z0);
        if st.grounded == 0 {
            airtime += dt;
        }
        if seq.last() != Some(&st.anim) {
            seq.push(st.anim);
        }
        if i < 40 {
            println!(
                "  jump t={:5.3} dz={:+.4} g={} anim={}",
                i as f32 * dt,
                st.pos[2] - z0,
                st.grounded,
                anim_name(st.anim)
            );
        }
    }
    println!(
        "== jump: grounded0={g0} apex={apex:.3} yd airtime={airtime:.3} s anims {:?} \
         (laws: apex 1.640, airtime 0.825)",
        seq.iter().map(|a| anim_name(*a)).collect::<Vec<_>>()
    );
}

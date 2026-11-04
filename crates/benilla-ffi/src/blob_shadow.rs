//! The unit blob shadow: the dark decal under every player, NPC and creature.
//!
//! The reference draws it through the same surface-decal projector as the selection ring
//! (`0x6d7920` -> `0x6d7330`), with a multiply blend, `Textures\ShadowBlob.blp` and the model's
//! base alpha. It is registered for units, players and corpses only, never game objects.
//!
//! Two disagreements with the app's `blob_shadow.rs`, both inherited from the host's decal path:
//! the square is sampled on a lattice over the box (no exact face clipping), and the trapezoid
//! ramp is carried by the material's soft blob instead of the reference's second texture stage.
//!
//! The reference has no user-facing unit-shadow switch (`shadowLOD` is never consulted); this
//! host honours the registered `mapShadows` video CVar (default 1) so the Options-side toggle can
//! turn the decals off without stopping the rest of the scene.

use std::ffi::c_char;
use std::sync::{Mutex, OnceLock};

use crate::net;
use crate::BnWorld;

/// The shadow's own art, a 32x32 grayscale radial blob (`0x6d8070`).
const SHADOW_TEXTURE: &str = "textures\\shadowblob.blp";

/// One world-effect vertex, the app's `EffectVertex` (world WoW yards, z up).
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct BnShadowVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// The built vertices, kept between the count call and the fill call the host makes each frame.
fn held_verts() -> &'static Mutex<Vec<BnShadowVertex>> {
    static CACHE: OnceLock<Mutex<Vec<BnShadowVertex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// `mapShadows`, the reference's registered "Toggle map shadows" video option. Absent before
/// the interface exists, and absent means on, like the client's default.
fn shadows_enabled() -> bool {
    match crate::video_cvar("mapShadows") {
        Some(text) => text.trim().parse::<f32>().map(|v| v != 0.0).unwrap_or(true),
        None => true,
    }
}

/// One unit's shadow grid: 5x5 lattice points over the box, fan-unrolled.
fn shadow_box(
    feet: [f32; 3],
    radius: f32,
    up: f32,
    down: f32,
    ground: impl Fn(f32, f32) -> Option<f32>,
) -> Vec<BnShadowVertex> {
    const CELLS: usize = 4;
    let mut lattice = [[BnShadowVertex::default(); CELLS + 1]; CELLS + 1];
    for (i, row) in lattice.iter_mut().enumerate() {
        let sx = -1.0 + 2.0 * (i as f32 / CELLS as f32);
        for (j, cell) in row.iter_mut().enumerate() {
            let sz = -1.0 + 2.0 * (j as f32 / CELLS as f32);
            let x = feet[0] + radius * sx;
            let y = feet[1] + radius * sz;
            let z = ground(x, y)
                .filter(|z| (*z >= feet[2] - down) && (*z <= feet[2] + up))
                .unwrap_or(feet[2]);
            *cell = BnShadowVertex {
                pos: [x, y, z],
                uv: [(sx + 1.0) * 0.5, (sz + 1.0) * 0.5],
                color: [0.0, 0.0, 0.0, 1.0],
            };
        }
    }
    let mut out = Vec::with_capacity(CELLS * CELLS * 6);
    for i in 0..CELLS {
        for j in 0..CELLS {
            let (a, b, c, d) = (
                lattice[i][j],
                lattice[i + 1][j],
                lattice[i + 1][j + 1],
                lattice[i][j + 1],
            );
            out.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    out
}

/// Build every unit's shadow for this frame and answer the vertex count; `out_tex` receives the
/// BLP path. `0` means draw nothing (no session, no shadows CVar, no units, or the player's
/// collision scene is not up yet).
#[no_mangle]
pub extern "C" fn bn_unit_shadows(
    world: *mut BnWorld,
    out_tex: *mut c_char,
    tex_cap: u32,
) -> u32 {
    let clear = || {
        held_verts().lock().unwrap_or_else(|e| e.into_inner()).clear();
        0
    };
    if world.is_null() {
        return clear();
    }
    // SAFETY: the caller passes the handle `bn_init` returned.
    let w = unsafe { &mut *world };
    if !shadows_enabled() {
        return clear();
    }
    if w.player.is_none() {
        return clear();
    }
    let eye = net::ui_unit_pose(net::ui_self_guid());

    // Resolve each unit's display bounds first: that needs the mutable chain, while the ground
    // projection below holds the player immutably for its whole loop.
    struct Pending {
        kind: u8,
        radius: f32,
        feet: [f32; 3],
        up: f32,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for guid in net::ui_unit_guids() {
        let Some(unit) = net::ui_unit(guid) else {
            continue;
        };
        // The reference registers the blob on units, players and corpses only.
        if !matches!(unit.kind, 3 | 4 | 7) {
            continue;
        }
        let mount = unit.fields.unit_mount_display_id();
        let display = if mount != 0 {
            mount
        } else {
            unit.fields.unit_displayid().unwrap_or(0).max(0) as u32
        };
        if display == 0 {
            continue;
        }
        let (local, row_scale, stand_z) = crate::selection_ring::display_bounds(w, display);
        let scale = if mount != 0 {
            row_scale
        } else {
            unit.fields
                .object_scale_x()
                .filter(|s| *s > 0.0)
                .unwrap_or(row_scale)
        };
        // The reference sizes the box from the Stand sequence's own X/Y extents (HumanFemale
        // 0.77x0.74 yd, blob_shadow.rs:354); the ring's footprint is that box's
        // diagonal-derived radius, so ~0.7 of it is the half-width the decal spans.
        let radius = 0.7 * local * scale.max(0.01);
        if radius <= 0.0 {
            continue;
        }
        let pose = net::ui_unit_pose(guid).unwrap_or(unit.pos);
        // Blob decals only matter around the client; farther units are already faded by the host.
        if let Some(eye) = eye {
            let dx = pose[0] - eye[0];
            let dy = pose[1] - eye[1];
            if dx * dx + dy * dy > 120.0 * 120.0 {
                continue;
            }
        }
        // Blob shadow: `+(h/2)` up and `-(5/3)(h/2)` down about the model origin.
        let up = (0.5 * stand_z.max(0.1) * scale).max(0.1);
        pending.push(Pending {
            kind: unit.kind,
            radius,
            feet: pose,
            up,
        });
    }

    let p = w.player.as_ref().unwrap();
    let mut verts: Vec<BnShadowVertex> = Vec::new();
    for item in pending {
        // The body is drawn on benilla's ground clamp, so the decal reads the same feet the ring
        // does: a creature's translation is clamped before its ring sees it.
        let feet = if item.kind == 3 {
            crate::player::clamped_feet(p, item.feet)
        } else {
            item.feet
        };
        let down = (5.0 / 3.0) * item.up;
        let top = feet[2] + item.up;
        verts.extend(shadow_box(feet, item.radius, item.up, down, |x, y| {
            p.ground(x, y, top).map(|(z, _)| z)
        }));
    }
    if verts.is_empty() {
        return clear();
    }
    let count = verts.len() as u32;
    *held_verts().lock().unwrap_or_else(|e| e.into_inner()) = verts;

    if !out_tex.is_null() && tex_cap > 0 {
        let bytes = SHADOW_TEXTURE.as_bytes();
        let take = bytes.len().min(tex_cap as usize - 1);
        // SAFETY: caller owns `tex_cap` writable bytes.
        unsafe {
            for (i, byte) in bytes.iter().take(take).enumerate() {
                *out_tex.add(i) = *byte as c_char;
            }
            *out_tex.add(take) = 0;
        }
    }
    count
}

/// The vertices [`bn_unit_shadows`] just built, fan-unrolled and world-space. A null `out` or
/// `cap` 0 asks for the count without copying.
#[no_mangle]
pub extern "C" fn bn_unit_shadow_verts(out: *mut BnShadowVertex, cap: u32) -> u32 {
    let held = held_verts().lock().unwrap_or_else(|e| e.into_inner());
    let n = held.len();
    if out.is_null() || cap == 0 {
        return n as u32;
    }
    let take = n.min(cap as usize);
    // SAFETY: caller owns `cap` writable vertices.
    unsafe {
        std::ptr::copy_nonoverlapping(held.as_ptr(), out, take);
    }
    take as u32
}

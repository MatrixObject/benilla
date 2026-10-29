// M2 particle emitters for the UE render host (the glue backdrop's flames and
// the converging motes).
//
// benilla simulates emitters in a renderer crate, which this
// crate cannot link. This module reproduces the same birth kernel and closed
// forms its integrator converges to, sampled in a steady loop: every emitter
// holds `rate x lifespan` particles spread evenly over their life, and each
// particle restarts with a fresh jitter roll when it wraps. That is stateless
// per call (cheap enough for a per-frame FFI) and visually identical to a
// emitter that has been running for a while.
//
// Positions come out in the same space bn_model_fill uses: UE axes, model
// units, relative to the model origin. The UE host multiplies by 100 to cm.

use std::collections::HashMap;
use std::ffi::c_char;
use std::sync::{Arc, Mutex};

use benilla_formats::{parse_m2_particle_emitters, ParamsNow, ParticleEmitterDef, ParticleShape};

use crate::{cstr_to_str, wow_vec_to_ue, BnWorld};

/// One camera-facing particle quad. Fields mirror the client's draw record:
/// centre + half-extent + colour + one atlas cell + in-plane spin.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BnParticleQuad {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Half-extent (model units); corners are centre ± (right, up) * size.
    pub size: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    /// In-plane rotation (radians), signed like the client's spin·age.
    pub spin: f32,
    /// (texture x raw blend) group index; see bn_model_particle_group.
    pub group: u32,
}

struct ParticleModel {
    defs: Vec<ParticleEmitterDef>,
    /// Per emitter: index into `groups`; None = no texture, not drawn.
    group_of: Vec<Option<u32>>,
    /// Deduped (BLP path as authored, raw blend) pairs.
    groups: Vec<(String, u8)>,
    /// Skeleton: emitters ride a bone joint (the client parents each emitter
    /// to `rig.anchor(bone)`), so their local cloud is posed by that joint.
    bones: Vec<crate::BnBone>,
}

static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Option<Arc<ParticleModel>>>>> =
    std::sync::OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, Option<Arc<ParticleModel>>>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const HDR_PTR: usize = 0x140;
const STRIDE: usize = 0x1f8;

fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn load(w: &BnWorld, path: &str) -> Option<Arc<ParticleModel>> {
    if let Some(hit) = cache().lock().ok()?.get(path) {
        return hit.clone();
    }
    let mut built: Option<Arc<ParticleModel>> = None;
    for a in w.archives.iter().rev() {
        if !a.contains(path) {
            continue;
        }
        let Ok(bytes) = a.read_file(path) else {
            continue;
        };
        let Ok(defs) = parse_m2_particle_emitters(&bytes) else {
            continue;
        };
        let count = if bytes.len() >= HDR_PTR + 4 {
            le_u32(&bytes, 0x13c) as usize
        } else {
            0
        };
        let base = if bytes.len() >= HDR_PTR + 4 {
            le_u32(&bytes, HDR_PTR) as usize
        } else {
            0
        };
        let raw_blend: Vec<u8> = (0..defs.len())
            .map(|i| {
                let e = base + i * STRIDE;
                if e + 0x29 <= bytes.len() && i < count {
                    bytes[e + 0x28]
                } else {
                    0
                }
            })
            .collect();
        let mut groups: Vec<(String, u8)> = Vec::new();
        let mut group_of = Vec::with_capacity(defs.len());
        for (i, d) in defs.iter().enumerate() {
            let Some(tex) = d.texture.as_ref() else {
                group_of.push(None);
                continue;
            };
            let key = (tex.clone(), raw_blend[i]);
            let g = match groups.iter().position(|x| *x == key) {
                Some(g) => g as u32,
                None => {
                    groups.push(key);
                    (groups.len() - 1) as u32
                }
            };
            group_of.push(Some(g));
        }
        let bones = crate::parse_m2_bones(&bytes).unwrap_or_default();
        built = Some(Arc::new(ParticleModel {
            defs,
            group_of,
            groups,
            bones,
        }));
        break;
    }
    if let Ok(mut c) = cache().lock() {
        c.insert(path.to_string(), built.clone());
    }
    built
}

fn next_u32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn rand01(state: &mut u32) -> f32 {
    (next_u32(state) >> 8) as f32 / (1u32 << 24) as f32
}

fn rand_s11(state: &mut u32) -> f32 {
    rand01(state) * 2.0 - 1.0
}

fn mix_seed(k: u32, epoch: u32, emitter: u32) -> u32 {
    let mut h = k.wrapping_mul(0x9E37_79B9)
        ^ epoch.wrapping_mul(0x85EB_CA6B)
        ^ emitter.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h | 1 // xorshift never crosses zero
}

/// The emission kernel benilla simulates with, reproduced
/// here because the crate that owns it cannot be linked.
fn emit_local(def: &ParticleEmitterDef, now: &ParamsNow, rng: &mut u32) -> ([f32; 3], [f32; 3]) {
    let origin = def.position;
    let rot90 = |v: [f32; 3]| [-v[1], v[0], v[2]];
    let mut local = [0.0f32; 3];
    let mut dir;
    if let (ParticleShape::Spline, Some(spline)) = (def.shape, &def.spline) {
        let t0 = now.area_length.clamp(0.0, 1.0);
        let t1 = now.area_width.clamp(0.0, 1.0);
        let t = t0 + rand01(rng) * (t1 - t0);
        local = spline.eval(t);
        dir = if now.z_source != 0.0 {
            let d = [local[0], local[1], local[2] - now.z_source];
            let n = norm_or_z(d);
            n
        } else if now.vertical_range != 0.0 {
            let tangent = norm_or_z(spline.tangent(t));
            let (s, c) = (rand_s11(rng) * now.vertical_range).sin_cos();
            // dir = Z * c + (tangent x Z) * s + tangent * (tangent.z * (1 - c))
            let cross = [
                tangent[1] * 1.0 - tangent[2] * 0.0,
                tangent[2] * 0.0 - tangent[0] * 1.0,
                tangent[0] * 0.0 - tangent[1] * 0.0,
            ];
            let k = tangent[2] * (1.0 - c);
            let d = [
                cross[0] * s + tangent[0] * k,
                cross[1] * s + tangent[1] * k,
                c + cross[2] * s + tangent[2] * k,
            ];
            if now.horizontal_range != 0.0 {
                let j = rand01(rng) * now.horizontal_range;
                local[0] += d[0] * j;
                local[1] += d[1] * j;
                local[2] += d[2] * j;
            }
            norm_or_z(d)
        } else {
            [0.0, 0.0, 0.0]
        };
        return (
            [
                origin[0] + -local[1],
                origin[1] + local[0],
                origin[2] + local[2],
            ],
            rot90(dir),
        );
    }
    let mut shell: Option<[f32; 3]> = None;
    if def.shape == ParticleShape::Sphere {
        let r = now.area_length + rand01(rng) * (now.area_width - now.area_length).max(0.0);
        let lat = rand_s11(rng) * now.vertical_range;
        let lon = rand_s11(rng) * now.horizontal_range;
        let (slat, clat) = lat.sin_cos();
        let (slon, clon) = lon.sin_cos();
        let s = [clat * clon, clat * slon, slat];
        local = [r * s[0], r * s[1], r * s[2]];
        shell = Some(s);
    } else {
        local = [
            rand_s11(rng) * 0.5 * now.area_length,
            rand_s11(rng) * 0.5 * now.area_width,
            0.0,
        ];
    }
    dir = if now.z_source != 0.0 {
        norm_or_z([local[0], local[1], local[2] - now.z_source])
    } else if let Some(s) = shell {
        if def.sphere_up() {
            [0.0, 0.0, 1.0]
        } else {
            s
        }
    } else {
        let theta = rand_s11(rng) * now.vertical_range;
        let phi = rand_s11(rng) * now.horizontal_range;
        let (st, ct) = theta.sin_cos();
        let (sp, cp) = phi.sin_cos();
        [st * cp, st * sp, ct]
    };
    // The fixed R(+Z, 90°) frame every M2 emitter prepends.
    dir = rot90(dir);
    (
        [
            origin[0] + -local[1],
            origin[1] + local[0],
            origin[2] + local[2],
        ],
        dir,
    )
}

fn norm_or_z(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l > 1e-6 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// Closed form of the client integrator (pos += v·dt; v.z -= g·dt; drag
/// v -= min(dt·drag,1)·v), drag as exponential decay. `drag` is the authored
/// per-second factor and may be negative (accelerating emitters).
fn integrate(p0: [f32; 3], v0: [f32; 3], g: f32, drag: f32, t: f32) -> [f32; 3] {
    if drag.abs() < 1e-4 {
        return [
            p0[0] + v0[0] * t,
            p0[1] + v0[1] * t,
            p0[2] + v0[2] * t - 0.5 * g * t * t,
        ];
    }
    // v(t) = (v0 + g/d) e^{-d t} - g/d with g_vec = (0, 0, -g)
    let decay = (-drag * t).exp();
    let k = (1.0 - decay) / drag;
    let gd = -g / drag; // the g_vec/d z term
    [
        p0[0] + v0[0] * k,
        p0[1] + v0[1] * k,
        p0[2] + (v0[2] - gd) * k - gd * t,
    ]
}

/// Pose by a `crate::bone_matrix_at` row-major 3x4: a point takes the translation row, a
/// vector only the rotation-and-scale 3x3, which is how the reference turns a birth velocity
/// through the emitter matrix without dragging its origin along.
fn pose(j: &[f32; 16], x: f32, y: f32, z: f32, point: bool) -> (f32, f32, f32) {
    let t = if point { 1.0 } else { 0.0 };
    (
        j[0] * x + j[1] * y + j[2] * z + j[3] * t,
        j[4] * x + j[5] * y + j[6] * z + j[7] * t,
        j[8] * x + j[9] * y + j[10] * z + j[11] * t,
    )
}

static TWINKLE: std::sync::OnceLock<[f32; 128]> = std::sync::OnceLock::new();

fn twinkle_noise(speed: f32, age: f32, phase: u32) -> f32 {
    let lut = TWINKLE.get_or_init(|| {
        let mut s = 0xC0FF_EE11u32;
        let mut t = [0.0f32; 128];
        for v in &mut t {
            *v = rand01(&mut s);
        }
        t
    });
    let idx = (((speed * age).clamp(0.0, 255.0) as u32).wrapping_add(phase) as usize) & 0x7f;
    lut[idx]
}

fn cell_uv(def: &ParticleEmitterDef, idx: u16) -> (f32, f32, f32, f32) {
    let cols = def.tile_cols.max(1);
    let rows = def.tile_rows.max(1);
    let cx = (idx & (cols - 1)) as f32;
    let cy = (idx >> cols.trailing_zeros()) as f32;
    let inv_c = 1.0 / cols as f32;
    let inv_r = 1.0 / rows as f32;
    (
        cx * inv_c,
        cy * inv_r,
        (cx + 1.0) * inv_c,
        (cy + 1.0) * inv_r,
    )
}

/// Number of (texture x blend) groups this model's emitters draw with.
#[no_mangle]
pub extern "C" fn bn_model_particle_group_count(
    world: *const BnWorld,
    model_path: *const c_char,
) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    let w = unsafe { &*world };
    load(w, p).map_or(0, |m| m.groups.len() as u32)
}

/// Group `g`'s BLP path (NUL-terminated) and raw blend byte. Returns the path
/// length written, 0 on a miss.
#[no_mangle]
pub extern "C" fn bn_model_particle_group(
    world: *const BnWorld,
    model_path: *const c_char,
    g: u32,
    blend: *mut u32,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() || out.is_null() || cap == 0 {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    let w = unsafe { &*world };
    let Some(m) = load(w, p) else { return 0 };
    let Some((tex, raw)) = m.groups.get(g as usize) else {
        return 0;
    };
    if !blend.is_null() {
        unsafe { *blend = u32::from(*raw) };
    }
    let bytes = tex.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        for i in 0..n {
            *out.add(i) = bytes[i] as c_char;
        }
        *out.add(n) = 0;
    }
    n as u32
}

/// Fill `out` with this model's particle quads at `time_ms` (model space, UE
/// axes). Answers the total count when `out` is null or `cap` is 0, so the host
/// can size its buffer first.
#[no_mangle]
pub extern "C" fn bn_model_particles(
    world: *const BnWorld,
    model_path: *const c_char,
    time_ms: f32,
    out: *mut BnParticleQuad,
    cap: u32,
) -> u32 {
    if world.is_null() || model_path.is_null() {
        return 0;
    }
    let Some(p) = cstr_to_str(model_path) else {
        return 0;
    };
    let w = unsafe { &*world };
    let Some(m) = load(w, p) else { return 0 };

    let time_s = (time_ms * 0.001).max(0.0);
    // The model plays its Stand sequence; a gseq clock is shared wall time.
    let elapsed = time_s;
    let shared_now = f64::from(time_s);
    let mut written = 0u32;
    let mut total = 0u32;

    for (ei, def) in m.defs.iter().enumerate() {
        let Some(group) = m.group_of[ei] else {
            continue;
        };
        let now = def.params.sample(Some(0), elapsed, shared_now);
        let rate = def.timing.rate(Some(0), elapsed, shared_now);
        let life = now.lifespan.max(0.01);
        let count = ((rate * life).ceil() as u32).min(256);
        if count == 0 {
            continue;
        }
        let g = now.gravity;
        // The reference's two storage modes (`ParticleEmitterDef::model_space`, file flag 0x10):
        // a world-mode emitter bakes each birth through the emitter matrix it has *at that
        // instant* (`0x7b8b0f`, `0x7b8acf`) and integrates in world space from then on, so a
        // moving bone leaves a trail of motes behind it; a model-mode emitter stores the birth
        // locally and the draw re-applies the *live* matrix (`0x7b8aa5`, `0x7b3efb`), so its
        // cloud rides the bone. Sampling the wrong one is what made the login booth's embers
        // clump on the statues instead of hanging in the air they were born into.
        let anchored = !def.model_space();
        let joint_at = |ms: u32| -> Option<crate::BnMat4> {
            if def.bone == u16::MAX || def.bone as usize >= m.bones.len() {
                return None;
            }
            Some(crate::bone_matrix_at(
                &m.bones,
                0,
                ms,
                0,
                &[],
                def.bone as usize,
            ))
        };
        // The emitter's own clock when the model was posed at `ms` (negative ages clamp to the
        // first cycle, the same clock a looping clip replays).
        let clock_at = |ms: f32| -> (f32, f64) {
            let s = (ms * 0.001).max(0.0);
            (s, f64::from(s))
        };
        for k in 0..count {
            total += 1;
            if out.is_null() || written >= cap {
                continue;
            }
            let cycles = time_s / life + k as f32 / count as f32;
            let epoch = cycles.floor() as u32;
            let u_age = (cycles - cycles.floor()).clamp(0.0, 1.0);
            let age = u_age * life;
            let mut rng = mix_seed(k, epoch, ei as u32);

            // Every track is sampled on the frame the particle is born, not the frame it is
            // drawn: the kernel, the speed and the lifetime all come from the birth clock.
            let birth_ms = time_ms - age * 1000.0;
            let (birth_s, birth_g) = clock_at(birth_ms);
            let born = if anchored {
                def.params.sample(Some(0), birth_s, birth_g)
            } else {
                now
            };
            let (p0, dir) = emit_local(def, &born, &mut rng);
            let speed = born.emission_speed * (1.0 + born.speed_variation * rand_s11(&mut rng));
            let v0 = [dir[0] * speed, dir[1] * speed, dir[2] * speed];
            let (p0x, p0y, p0z) = wow_vec_to_ue(p0[0], p0[1], p0[2]);
            let (vx0, vy0, vz0) = wow_vec_to_ue(v0[0], v0[1], v0[2]);
            let (local, joint) = if anchored {
                let j = joint_at(birth_ms.max(0.0) as u32);
                let (bx, by, bz) = match j {
                    Some(j) => pose(&j, p0x, p0y, p0z, true),
                    None => (p0x, p0y, p0z),
                };
                let (ivx, ivy, ivz) = match j {
                    Some(j) => pose(&j, vx0, vy0, vz0, false),
                    None => (vx0, vy0, vz0),
                };
                (integrate([bx, by, bz], [ivx, ivy, ivz], g, def.drag, age), None)
            } else {
                let local = integrate([p0x, p0y, p0z], [vx0, vy0, vz0], g, def.drag, age);
                (local, joint_at(time_ms.max(0.0) as u32))
            };

            let ol = def.over_life.sample(u_age);
            let noise = twinkle_noise(def.twinkle_speed, age, rng & 0x7f);
            if def.twinkle_percent < 1.0 && noise > def.twinkle_percent {
                total -= 1;
                continue;
            }
            let half = ol.size * def.twinkle(noise);
            if half <= 0.0 {
                total -= 1;
                continue;
            }
            let (u0, v0c, u1, v1c) = cell_uv(def, ol.head_cell);
            let phase_bit = (rng & 0x20) != 0;
            let mut spin = def.spin * age;
            if spin < 0.0 && phase_bit {
                spin = -spin;
            }
            let (qx, qy, qz) = match joint {
                Some(j) => pose(&j, local[0], local[1], local[2], true),
                None => (local[0], local[1], local[2]),
            };
            // SAFETY: written < cap, so this index is inside `out`.
            unsafe {
                *out.add(written as usize) = BnParticleQuad {
                    x: qx,
                    y: qy,
                    z: qz,
                    size: half,
                    r: ol.color[0],
                    g: ol.color[1],
                    b: ol.color[2],
                    a: ol.color[3],
                    u0,
                    v0: v0c,
                    u1,
                    v1: v1c,
                    spin,
                    group,
                };
            }
            written += 1;
        }
    }
    total
}

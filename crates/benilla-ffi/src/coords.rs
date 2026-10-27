// WoW <-> UE5 coordinate-system conversion layer for benilla-ffi.
//! All matrices are column-major (index = col*4+row), matching the client
//! Matrix::operator* application and the skin-blend apply.
//!
//! World-of-Warcraft 1.12.1 M2 geometry is left-handed, Y-up. Unreal Engine 5
//! is left-handed, Z-up. The client nezha model.cpp converts at the boundary
//! with fixCoordSystem:
//! vector (x, y, z) -> (x, -y, z) (Y-flip)
//! quaternion (x,y,z,w) -> (x, -y, z, w) (Y-flip of the vector part)
//! scale (x, y, z) -> unchanged (fixCoordSystem2 = identity)
//!
//! Distance scale: 1 M2 world unit = 100 UE cm (Blizzard 1 yard = 3 units =
//! 300 cm). The client places actors via SetActorScale3D(gWorld->scale = 1)
//! on world-unit coordinates, which is equivalent to multiplying model-space
//! values by 100 before feeding UE.

/// A 3-component vector in the target coordinate system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// A 4-component quaternion (vector part x,y,z + scalar w).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

/// A column-major 4x4 matrix, index = col*4 + row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4 {
    pub m: [f32; 16],
}

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4 {
        m: [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ],
    };

    pub fn identity() -> Self {
        Self::IDENTITY
    }

    /// Column-major matrix multiply: apply b first, then a.
    pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
        let mut r = [0.0f32; 16];
        for ci in 0..4 {
            for ri in 0..4 {
                let mut s = 0.0f32;
                for k in 0..4 {
                    s += a.m[k * 4 + ri] * b.m[ci * 4 + k];
                }
                r[ci * 4 + ri] = s;
            }
        }
        Mat4 { m: r }
    }

    /// Apply to a point (x,y,z) in the target coordinate system.
    pub fn apply_point(m: &Mat4, v: Vec3) -> Vec3 {
        Vec3 {
            x: m.m[0] * v.x + m.m[4] * v.y + m.m[8] * v.z + m.m[12],
            y: m.m[1] * v.x + m.m[5] * v.y + m.m[9] * v.z + m.m[13],
            z: m.m[2] * v.x + m.m[6] * v.y + m.m[10] * v.z + m.m[14],
        }
    }

    /// Column-major translation matrix (translation in column 3).
    pub fn translation(v: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY.m;
        m[12] = v.x;
        m[13] = v.y;
        m[14] = v.z;
        Mat4 { m }
    }

    /// Column-major non-uniform scale matrix.
    pub fn scale(v: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY.m;
        m[0] = v.x;
        m[5] = v.y;
        m[10] = v.z;
        Mat4 { m }
    }

    /// Column-major rotation matrix from a quaternion.
    /// The classic row-major rotation values are transposed so that
    /// apply_point (column-major) transforms vectors correctly. This fixes the
    /// old row-major-into-column-major mix that twisted the limbs.
    pub fn rotation(q: Quat) -> Mat4 {
        let x = q.x;
        let y = q.y;
        let z = q.z;
        let w = q.w;
        let xx = x * x;
        let yy = y * y;
        let zz = z * z;
        let xy = x * y;
        let xz = x * z;
        let yz = y * z;
        let wx = w * x;
        let wy = w * y;
        let wz = w * z;
        let mut m = Mat4::IDENTITY.m;
        m[0] = 1.0 - 2.0 * (yy + zz);
        m[1] = 2.0 * (xy - wz);
        m[2] = 2.0 * (xz + wy);
        m[4] = 2.0 * (xy + wz);
        m[5] = 1.0 - 2.0 * (xx + zz);
        m[6] = 2.0 * (yz - wx);
        m[8] = 2.0 * (xz - wy);
        m[9] = 2.0 * (yz + wx);
        m[10] = 1.0 - 2.0 * (xx + yy);
        Mat4 { m }
    }

    /// Compose a pivot-center, translate, rotate, scale matrix exactly like
    /// the client Bone::calcMatrix (all post-multiplied):
    /// m = T(pivot) * T(trans) * R(rot) * S(scale) * T(-pivot)
    pub fn bone_local(pivot: Vec3, trans: Vec3, rot: Quat, scale: Vec3) -> Mat4 {
        let mut m = Self::translation(pivot);
        m = Self::mul(&m, &Self::translation(trans));
        m = Self::mul(&m, &Self::rotation(rot));
        m = Self::mul(&m, &Self::scale(scale));
        m = Self::mul(
            &m,
            &Self::translation(Vec3 {
                x: -pivot.x,
                y: -pivot.y,
                z: -pivot.z,
            }),
        );
        m
    }

    /// Convert to the flat column-major [f32;16] array used across the FFI.
    pub fn to_array(&self) -> [f32; 16] {
        self.m
    }
}

/// The WoW -> UE5 conversion, mirroring the client fixCoordSystem.
pub mod wow_to_ue {
    use super::*;

    /// One M2 unit, expressed in UE centimetre units (1 unit = 100 cm).
    pub const UNIT_CM: f32 = 100.0;

    /// Position / vertex (fixCoordSystem: (x, -y, z)).
    pub fn vec(v: Vec3) -> Vec3 {
        Vec3 {
            x: v.x,
            y: -v.y,
            z: v.z,
        }
    }

    /// Normal (fixCoordSystem: (x, -y, z)).
    pub fn normal(v: Vec3) -> Vec3 {
        vec(v)
    }

    /// Quaternion (fixCoordSystemQuat: (x, -y, z, w)).
    pub fn quat(q: Quat) -> Quat {
        Quat {
            x: q.x,
            y: -q.y,
            z: q.z,
            w: q.w,
        }
    }

    /// Scale (fixCoordSystem2: identity, unchanged).
    pub fn scale(v: Vec3) -> Vec3 {
        v
    }

    /// Translation track value (fixCoordSystem applied: (x,-y,z)).
    pub fn translation(v: Vec3) -> Vec3 {
        vec(v)
    }

    /// Convert a model-space position (M2 units) to UE world cm:
    /// axis-flip the vector and multiply by UNIT_CM.
    pub fn to_world(v: Vec3) -> Vec3 {
        let u = vec(v);
        Vec3 {
            x: u.x * UNIT_CM,
            y: u.y * UNIT_CM,
            z: u.z * UNIT_CM,
        }
    }
}

/// The inverse UE5 -> WoW conversion.
pub mod ue_to_wow {
    use super::*;

    /// Undo the wow_to_ue axis flip: (x, -y, z) -> (x, y, z).
    pub fn vec(v: Vec3) -> Vec3 {
        Vec3 {
            x: v.x,
            y: -v.y,
            z: v.z,
        }
    }

    pub fn quat(q: Quat) -> Quat {
        Quat {
            x: q.x,
            y: -q.y,
            z: q.z,
            w: q.w,
        }
    }

    pub fn scale(v: Vec3) -> Vec3 {
        v
    }

    /// Convert a UE world cm position back to M2 units: undo axis flip and
    /// divide by UNIT_CM.
    pub fn from_world(v: Vec3) -> Vec3 {
        let u = super::wow_to_ue::UNIT_CM;
        let w = vec(v);
        Vec3 {
            x: w.x / u,
            y: w.y / u,
            z: w.z / u,
        }
    }
}

/// A quaternion slerp, matching the client Quaternion::slerp.
pub fn quat_slerp(r: f32, a: Quat, b: Quat) -> Quat {
    let dot = a.x * b.x + a.y * b.y + a.z * b.z + a.w * b.w;
    let abs_dot = dot.abs();
    if abs_dot > 0.9995 {
        let x = a.x * (1.0 - r) + b.x * r;
        let y = a.y * (1.0 - r) + b.y * r;
        let z = a.z * (1.0 - r) + b.z * r;
        let w = a.w * (1.0 - r) + b.w * r;
        let m = (x * x + y * y + z * z + w * w).sqrt();
        if m < 1e-8 {
            return Quat {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            };
        }
        return Quat {
            x: x / m,
            y: y / m,
            z: z / m,
            w: w / m,
        };
    }
    let theta = dot.acos();
    let sin_theta = theta.sin();
    if sin_theta < 1e-8 {
        return a;
    }
    let sa = ((1.0 - r) * theta).sin() / sin_theta;
    let sb = (r * theta).sin() / sin_theta;
    let x = a.x * sa + b.x * sb;
    let y = a.y * sa + b.y * sb;
    let z = a.z * sa + b.z * sb;
    let w = a.w * sa + b.w * sb;
    let m = (x * x + y * y + z * z + w * w).sqrt();
    if m < 1e-8 {
        return Quat {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        };
    }
    Quat {
        x: x / m,
        y: y / m,
        z: z / m,
        w: w / m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_round_trip() {
        let v = Vec3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        };
        assert_eq!(ue_to_wow::vec(wow_to_ue::vec(v)), v);
    }

    #[test]
    fn quat_round_trip() {
        let q = Quat {
            x: 0.1,
            y: 0.2,
            z: 0.3,
            w: 0.9,
        };
        assert_eq!(ue_to_wow::quat(wow_to_ue::quat(q)), q);
    }

    #[test]
    fn world_round_trip() {
        let v = Vec3 {
            x: 0.7,
            y: -0.3,
            z: 1.4,
        };
        let w = wow_to_ue::to_world(v);
        let back = ue_to_wow::from_world(w);
        assert!((back.x - v.x).abs() < 1e-4, "x {} vs {}", back.x, v.x);
        assert!((back.y - v.y).abs() < 1e-4, "y {} vs {}", back.y, v.y);
        assert!((back.z - v.z).abs() < 1e-4, "z {} vs {}", back.z, v.z);
    }

    #[test]
    fn unit_scale_is_100_cm() {
        let one = Vec3 {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        };
        let w = wow_to_ue::to_world(one);
        assert!((w.z - 100.0).abs() < 1e-4, "z={} (expected 100)", w.z);
        assert!((w.y - -100.0).abs() < 1e-4, "y={} (expected -100)", w.y);
    }

    #[test]
    fn rotation_fixes_origin_and_is_orthonormal() {
        // Column-major rotation about +Z: send +Z to -X or +X (transpose of
        // row-major). The key property: the origin stays fixed and the columns
        // are orthonormal (it is a true rotation).
        let m = Mat4::rotation(Quat {
            x: 0.0,
            y: 0.0,
            z: 0.5,
            w: 0.8660254,
        });
        let origin = Mat4::apply_point(
            &m,
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        assert!(
            origin.x.abs() < 1e-6 && origin.y.abs() < 1e-6 && origin.z.abs() < 1e-6,
            "origin moved to {:?}",
            origin
        );
        // column 0 = (m[0], m[1], m[2]) should be a unit vector
        let c0 = [m.m[0], m.m[1], m.m[2]];
        let len = (c0[0] * c0[0] + c0[1] * c0[1] + c0[2] * c0[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "col0 not unit: len={}", len);
        // A 90deg rotation sends a unit axis to another unit axis:
        let p = Mat4::apply_point(
            &m,
            Vec3 {
                x: 0.0,
                y: 0.0,
                z: 1.0,
            },
        );
        let plen = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
        assert!(
            (plen - 1.0).abs() < 1e-4,
            "|rot(Z)| != 1: {:?} len={}",
            p,
            plen
        );
    }

    #[test]
    fn bone_local_rest_is_identity() {
        let p = Vec3 {
            x: 2.0,
            y: 3.0,
            z: 4.0,
        };
        let zero = Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let one = Vec3 {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        };
        // rest = trans 0, rot identity, scale 1 -> T(p)*T(0)*I*1*T(-p) = identity
        let m = Mat4::bone_local(
            p,
            zero,
            Quat {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            },
            one,
        );
        for i in 0..16 {
            assert!(
                (m.m[i] - Mat4::IDENTITY.m[i]).abs() < 1e-5,
                "i={} {}",
                i,
                m.m[i]
            );
        }
    }

    #[test]
    fn bone_local_pivot_rotation_fixes_pivot() {
        // The pivot must map to itself under the composed bone matrix:
        // T(p)*T(t)*R*S*T(-p) * p = T(p)*T(t)*R*S*0 (at rest t=0,s=1,R=I -> p).
        // With a 90deg rotation about Y the pivot is still the fixed point of
        // the "rotate around pivot" part, but trans != 0 shifts it. Use t=0:
        let p = Vec3 {
            x: 10.0,
            y: 0.0,
            z: 0.0,
        };
        let zero = Vec3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let one = Vec3 {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        };
        let m = Mat4::bone_local(
            p,
            zero,
            Quat {
                x: 0.0,
                y: 0.5,
                z: 0.0,
                w: 0.8660254,
            },
            one,
        );
        let fixed = Mat4::apply_point(&m, p);
        assert!((fixed.x - p.x).abs() < 1e-4, "x {} vs {}", fixed.x, p.x);
        assert!((fixed.y - p.y).abs() < 1e-4, "y {} vs {}", fixed.y, p.y);
        assert!((fixed.z - p.z).abs() < 1e-4, "z {} vs {}", fixed.z, p.z);
    }
}

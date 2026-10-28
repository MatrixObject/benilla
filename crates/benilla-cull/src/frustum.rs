//! The two frustum tests the cull needs, over glam: Bevy's `Frustum` is `bevy_camera`'s, and this
//! crate must not pull the camera (and with it `bevy_ecs`, `bevy_asset` and `bevy_image`) into the
//! FFI. The construction and the OBB test are `bevy_camera` 0.18's, line for line, so a plane set
//! built here is the one `Frustum::from_clip_from_world` builds; `benilla-world`'s
//! `frustum_agrees_with_bevy` test holds the two together on random matrices.

use glam::{Affine3A, Mat4, Vec3A, Vec4};

/// The near plane's index, `Frustum::NEAR_PLANE_IDX`.
pub const NEAR_PLANE_IDX: usize = 4;
/// The far plane's index, `Frustum::FAR_PLANE_IDX`.
pub const FAR_PLANE_IDX: usize = 5;

/// An axis-aligned box, Bevy's `Aabb` in `Vec3A` terms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub center: Vec3A,
    pub half_extents: Vec3A,
}

impl Aabb {
    pub fn from_min_max(min: Vec3A, max: Vec3A) -> Self {
        let center = (min + max) * 0.5;
        let half_extents = (max - min) * 0.5;
        Self {
            center,
            half_extents,
        }
    }

    /// The support radius along `p_normal` for a box turned by `world_from_local`, Bevy's
    /// `Aabb::relative_radius`.
    fn relative_radius(&self, p_normal: &Vec3A, world_from_local: &glam::Mat3A) -> f32 {
        Vec3A::new(
            p_normal.dot(world_from_local.x_axis),
            p_normal.dot(world_from_local.y_axis),
            p_normal.dot(world_from_local.z_axis),
        )
        .abs()
        .dot(self.half_extents)
    }
}

/// Six unit-normal half-spaces, Bevy's `Frustum`.
#[derive(Clone, Copy, Debug)]
pub struct Frustum {
    planes: [Vec4; 6],
}

/// `HalfSpace::new`: the normal normalized, the distance scaled with it.
fn half_space(v: Vec4) -> Vec4 {
    v * v.truncate().length_recip()
}

impl Frustum {
    /// Bevy's `Frustum::from_clip_from_world`: the side planes from the clip matrix's rows, the near
    /// plane `row3 + row2`, the far `row2`.
    pub fn from_clip_from_world(clip_from_world: &Mat4) -> Self {
        let (row0, row1, row2, row3) = (
            clip_from_world.row(0),
            clip_from_world.row(1),
            clip_from_world.row(2),
            clip_from_world.row(3),
        );
        Self {
            planes: [
                half_space(row3 + row0),
                half_space(row3 - row0),
                half_space(row3 + row1),
                half_space(row3 - row1),
                half_space(row3 + row2),
                half_space(row2),
            ],
        }
    }

    /// Bevy's `Frustum::intersects_obb`: culled when a tested plane's support term is not positive.
    pub fn intersects_obb(
        &self,
        aabb: &Aabb,
        world_from_local: &Affine3A,
        intersect_near: bool,
        intersect_far: bool,
    ) -> bool {
        let center = world_from_local.transform_point3a(aabb.center).extend(1.0);
        for (idx, plane) in self.planes.iter().enumerate() {
            if (idx == NEAR_PLANE_IDX && !intersect_near)
                || (idx == FAR_PLANE_IDX && !intersect_far)
            {
                continue;
            }
            let normal = Vec3A::from(plane.truncate());
            let radius = aabb.relative_radius(&normal, &world_from_local.matrix3);
            if plane.dot(center) + radius <= 0.0 {
                return false;
            }
        }
        true
    }
}

/// The sub-frustum for one NDC window rect, with no width gate: the reference builds one volume
/// per window for both the open-world walk (the copy at `0xc7cb7c`, for `0x682fa0`) and the
/// building's own Pass 2 (`0x682930`, for `0x6b3b20`), and only the walk rejects a narrow window
/// ([`scene_window_frustum`]). The rect is a scale and offset on clip space with depth untouched,
/// so the extracted planes bound the window at the camera's own near and far.
pub fn window_frustum(rect: [f32; 4], clip_from_world: &Mat4) -> Frustum {
    let [x0, y0, x1, y1] = rect;
    let (w, h) = (x1 - x0, y1 - y0);
    // Column-major: `Mat4::from_cols` takes columns, so the `w`-column carries the offsets.
    let rect_to_ndc = Mat4::from_cols(
        Vec4::new(2.0 / w, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 2.0 / h, 0.0, 0.0),
        Vec4::new(0.0, 0.0, 1.0, 0.0),
        Vec4::new(-(x0 + x1) / w, -(y0 + y1) / h, 0.0, 1.0),
    );
    Frustum::from_clip_from_world(&(rect_to_ndc * *clip_from_world))
}

/// The open-world walk's narrowest window, `0.01` of the 0..1 screen (0.02 NDC): `0x682fa0` returns
/// before touching a bucket unless both extents reach it.
pub const MIN_WINDOW_NDC: f32 = 0.02;

/// [`window_frustum`] behind the open-world walk's entry gate: `0x682fa0` returns before touching a
/// bucket unless both extents are `>= [0x8029d0]` (0.01 of the 0..1 screen, 0.02 NDC), so a
/// narrower window draws no terrain, doodad, world WMO, liquid, far band or outdoor unit. The
/// shared builder `0x682930` has no compares, and Pass 2's body `[0x6b3c73, 0x6b3d6f)` none on the
/// rect, so a window too narrow for the open world still draws the building's own shell.
pub fn scene_window_frustum(rect: [f32; 4], clip_from_world: &Mat4) -> Option<Frustum> {
    let [x0, y0, x1, y1] = rect;
    if x1 - x0 < MIN_WINDOW_NDC || y1 - y0 < MIN_WINDOW_NDC {
        return None;
    }
    Some(window_frustum(rect, clip_from_world))
}

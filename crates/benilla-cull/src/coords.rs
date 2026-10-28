//! The WoW to render-space axis map, the cull's half of `benilla-assets::coords` (which re-exports
//! these two and keeps the rotation/`Transform` helpers that need Bevy): WoW is right-handed with
//! +X north, +Y west, +Z up; the render space is +Y up, −Z forward. The map is the rotation
//! `bevy = (−y, z, −x)`, determinant +1, so it never mirrors a winding and 1 unit stays 1 yard.

use glam::Vec3;

/// WoW `[x, y, z]` to render space.
pub fn wow_to_bevy(p: [f32; 3]) -> Vec3 {
    Vec3::new(-p[1], p[2], -p[0])
}

/// Inverse of [`wow_to_bevy`], for sending a render-space point back upstream.
pub fn bevy_to_wow(b: Vec3) -> [f32; 3] {
    [-b.z, -b.x, b.y]
}

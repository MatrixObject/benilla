//! Collision geometry: the M2 collision hull and the WMO group gathers for walking and the camera.

use std::collections::HashMap;
use std::io::Cursor;

use anyhow::{Context, Result};
use benilla_m2::parse_m2;
use benilla_wmo::{parse_wmo, ParsedWmo, WmoGroup};

use crate::Chain;

use super::model_path;

/// A collision triangle list in raw model-local coordinates, the space of
/// [`super::RenderSubmesh::positions`]; empty when the source has no collision geometry.
#[derive(Debug, Clone, Default)]
pub struct CollisionMesh {
    pub positions: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

impl CollisionMesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// An M2 doodad's collision hull, the header's `bounding_vertices` (`C3Vector`) and
/// `bounding_triangles` (`u16` indices): a coarse solid far smaller than the render mesh, so the
/// trunk blocks and the canopy does not. A doodad with no hull does not collide.
pub fn load_m2_collision_hull(chain: &mut Chain, raw_path: &str) -> Result<CollisionMesh> {
    let path = model_path(raw_path);
    let bytes = chain
        .read_file(&path)
        .with_context(|| format!("reading M2 {path}"))?;
    parse_m2_collision_hull(&bytes).with_context(|| format!("parsing M2 {path}"))
}

/// [`load_m2_collision_hull`] from file bytes already in hand; empty when there is no hull.
pub fn parse_m2_collision_hull(bytes: &[u8]) -> Result<CollisionMesh> {
    let format =
        parse_m2(&mut Cursor::new(bytes)).map_err(|e| anyhow::anyhow!("parsing M2 hull: {e}"))?;
    let rd = &format.model().raw_data;
    let positions: Vec<[f32; 3]> = rd
        .bounding_vertices
        .as_chunks::<12>()
        .0
        .iter()
        .map(|c| {
            [
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                f32::from_le_bytes([c[8], c[9], c[10], c[11]]),
            ]
        })
        .collect();
    let n = positions.len() as u32;
    let indices: Vec<u32> = rd
        .bounding_triangles
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u32::from(u16::from_le_bytes([c[0], c[1]])))
        .collect();
    // Every 1.12 hull has whole triangles and in-range indices; anything else counts as no hull.
    if indices.is_empty() || !indices.len().is_multiple_of(3) || indices.iter().any(|&i| i >= n) {
        return Ok(CollisionMesh::default());
    }
    Ok(CollisionMesh { positions, indices })
}

/// MOPY `DETAIL`, the face bit the walking gather skips. The reference builds a reject mask from a
/// per-query class word: walking, `0x10_0111` (`0x6315f0`), gives `0x84`, tested at the box-query
/// leaf `0x6bca50`; camera and line of sight, `0x10_0171` (`0x50e570`), give `0x82`, tested at
/// the segment-query leaf `0x6bc700`. The same code builds both masks, and `0x80` is the query's
/// transient visited bit, so walking skips `DETAIL` and the camera `NOCAMCOLLIDE`.
const MOPY_DETAIL: u8 = 0x04;
/// MOPY `NOCAMCOLLIDE`, the face bit the camera and line-of-sight gather skips.
const MOPY_NOCAMCOLLIDE: u8 = 0x02;

/// A WMO's walking-collidable triangles across every group file, in raw WMO-local coordinates: a
/// face collides unless it is `DETAIL` ([`MOPY_DETAIL`]). Wider than the vmangos vmap rule
/// `COLLISION || (RENDER && !DETAIL)`: faces with flags `0x00` or only `0x40` collide too. Every
/// group is gathered, with no whole-group skip by MOGP flags.
pub fn load_wmo_collision_tris(chain: &mut Chain, raw_path: &str) -> Result<CollisionMesh> {
    let root_path = raw_path.to_ascii_lowercase();
    let bytes = chain
        .read_file(&root_path)
        .with_context(|| format!("reading WMO {root_path}"))?;
    let ParsedWmo::Root(root) = parse_wmo(&mut Cursor::new(&bytes))
        .map_err(|e| anyhow::anyhow!("parsing {root_path}: {e}"))?
    else {
        anyhow::bail!("{root_path} is not a WMO root file");
    };
    let stem = root_path.strip_suffix(".wmo").unwrap_or(&root_path);
    let (mut positions, mut indices): (Vec<[f32; 3]>, Vec<u32>) = (Vec::new(), Vec::new());
    // Two-sided surfaces (MOMT flag 0x04) are drawn from both sides by the
    // renderer, but collision kept only the authored winding, and the mover's
    // one-sided wall test (`0x632700`) skips a face approached from its back -
    // a body inside Goldshire Inn walked out through its walls. The back-face
    // winding is added wherever the drawn surface is two-sided.
    let two_sided: Vec<bool> = root.materials.iter().map(|m| m.flags & 0x04 != 0).collect();
    for gi in 0..root.n_groups {
        let group_path = format!("{stem}_{gi:03}.wmo");
        let Ok(gbytes) = chain.read_file(&group_path) else {
            continue;
        };
        let Ok(ParsedWmo::Group(group)) = parse_wmo(&mut Cursor::new(&gbytes)) else {
            continue;
        };
        // Interior groups (`MOGI` flags `& 0x48 == 0`) are the rooms a body walks
        // inside; their shell is authored outward, so the room-facing side is a
        // back face the one-sided mover ignores. Two-sided materials (MOMT 0x04)
        // get the same treatment wherever they are drawn.
        let interior = group.flags & 0x48 == 0;
        append_group_faces(&group, MOPY_DETAIL, 0, interior, Some(&two_sided), &mut positions,
                           &mut indices);
    }
    Ok(CollisionMesh { positions, indices })
}

/// Steepness gate for the back-face pass, the mover's own `GROUND_COS`
/// (`benilla-ffi/src/player.rs:33`): faces at least this vertical get the
/// reversed winding; floors and ceilings keep their authored side, so the
/// ground/ceiling probes cannot see a phantom plane.
const WMO_WALL_GROUND_COS: f32 = 0.642_788;

/// Append one WMO group file's walking-collidable triangles: every face but `DETAIL`.
pub fn accumulate_wmo_group_collision(
    group_bytes: &[u8],
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    accumulate_wmo_group_faces(group_bytes, MOPY_DETAIL, 0, positions, indices);
}

/// Append one WMO group file's camera and line-of-sight triangles: every face but `NOCAMCOLLIDE`,
/// so a `DETAIL` overhang the player walks under (forge pipes) still stops the camera.
pub fn accumulate_wmo_group_camera_collision(
    group_bytes: &[u8],
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    accumulate_wmo_group_faces(group_bytes, MOPY_NOCAMCOLLIDE, 0, positions, indices);
}

/// Append one WMO group file's camera-only triangles (`DETAIL` set, `NOCAMCOLLIDE` clear): the
/// camera set minus the walking set, for the down-ray's fallback once the walking leg misses.
pub fn accumulate_wmo_group_camera_only_collision(
    group_bytes: &[u8],
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    accumulate_wmo_group_faces(
        group_bytes,
        MOPY_NOCAMCOLLIDE,
        MOPY_DETAIL,
        positions,
        indices,
    );
}

/// Append every triangle whose MOPY flags miss `skip_mask` and carry all of `require_mask`, its
/// vertices remapped per group (the same index in two groups is two vertices).
fn accumulate_wmo_group_faces(
    group_bytes: &[u8],
    skip_mask: u8,
    require_mask: u8,
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    let Ok(ParsedWmo::Group(group)) = parse_wmo(&mut Cursor::new(group_bytes)) else {
        return;
    };
    append_group_faces(&group, skip_mask, require_mask, false, None, positions, indices);
}

/// Append one parsed group's kept faces. `group_two_sided` / `two_sided_mats`
/// (MOMT two-sided, one entry per material) also append each steep face's
/// reversed winding, so a one-sided mover stops from either side.
fn append_group_faces(
    group: &WmoGroup,
    skip_mask: u8,
    require_mask: u8,
    group_two_sided: bool,
    two_sided_mats: Option<&[bool]>,
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    let n_tri = group.vertex_indices.len() / 3;
    let mut map: HashMap<u16, u32> = HashMap::new();
    for t in 0..n_tri {
        let Some(mopy) = group.material_info.get(t) else {
            continue;
        };
        if mopy.flags & skip_mask != 0 || mopy.flags & require_mask != require_mask {
            continue;
        }
        let mut tri = [0u32; 3];
        let mut pts = [[0.0f32; 3]; 3];
        let mut complete = true;
        for k in 0..3 {
            let Some(&vidx) = group.vertex_indices.get(t * 3 + k) else {
                complete = false;
                break;
            };
            let Some(p) = group.vertex_positions.get(vidx as usize) else {
                complete = false;
                break;
            };
            pts[k] = [p.x, p.y, p.z];
            tri[k] = *map.entry(vidx).or_insert_with(|| {
                positions.push([p.x, p.y, p.z]);
                (positions.len() - 1) as u32
            });
        }
        if !complete {
            continue;
        }
        indices.extend_from_slice(&tri);
        let two_sided = group_two_sided
            || two_sided_mats
                .and_then(|m| m.get(mopy.material_id as usize))
                .copied()
                .unwrap_or(false);
        if two_sided {
            let a = [pts[1][0] - pts[0][0], pts[1][1] - pts[0][1], pts[1][2] - pts[0][2]];
            let b = [pts[2][0] - pts[0][0], pts[2][1] - pts[0][1], pts[2][2] - pts[0][2]];
            let n = [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 0.0 && (n[2] / len).abs() < WMO_WALL_GROUND_COS {
                indices.extend_from_slice(&[tri[2], tri[1], tri[0]]);
            }
        }
    }
}

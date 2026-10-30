//! The item-push cards: the stock `ItemAnimTemplate` panes' icon quad, posed for the HUD.
//!
//! `ITEM_PUSH` runs `ItemAnim_OnEvent`, which gives a stock bag button's child `<Model>` the
//! item's icon (`ReplaceIconTexture`, the type-14 batch slot) and restarts its one-shot sequence.
//! A `Model` is an orthographic scene in the reference: one model unit is `1280 · modelScale ·
//! frameScale` root units (`0x76d1a0`), `SetPosition` is in `768·√(a²+1)` layout units, and the
//! model root sits on the pane's bottom-left corner after the facing. The host paints the
//! interface from the dump, which carries no 3D pass, so the middle layer poses the card's
//! icon-slot batch here and the HUD draws its triangles as one textured quad. The stock card is
//! `Interface\ItemAnimations\ForcedBackpackItem.m2`: one bone, four vertices, one 1000 ms clamped
//! sequence whose transparency fades it in over 133 ms and out to zero by the end.
//!
//! This module also drains the pushes `net` queues ([`crate::net::take_item_pushes`]) into
//! `ITEM_PUSH` events, holding one back until its item template lands exactly the way `OnItemPush
//! 0x491a60` re-enters from the item cache (`0x491a93` → `0x491ee0`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use benilla_ui::script::UiScript;
use benilla_ui::widget::{model_key, ModelFileFacts, ModelPlayHead, SequenceFacts};

use crate::ui_feed::Catalogs;
use crate::BnWorld;

/// Frames a push waits for its item template before it is dropped, about 2 s at 60 fps: the
/// app's `ui_loot::RECEIVE_MAX_TRIES`, kept because the reference's cache callback has no timeout.
const PUSH_MAX_TRIES: u16 = 120;

/// One parsed card file: its icon-slot batches and the rig that poses them.
struct CardFile {
    subs: Vec<benilla_formats::RenderSubmesh>,
    bones: Vec<crate::BnBone>,
    gseqs: Vec<u32>,
    /// `(anim id, file slot, band start in ms)` per sequence, in file order (`facts.sequence`
    /// answers the same first slot for an id).
    seqs: Vec<(u16, usize, u32)>,
    facts: ModelFileFacts,
}

/// One batch of a posed card: the triangles' `(x, y, u, v)` corners in root units (y up), the
/// icon and the alpha the HUD paints them at.
struct CardQuad {
    alpha: f32,
    texture: String,
    tris: Vec<[[f32; 4]; 3]>,
}

/// The pane values a pose reads, so the unit test can build one without a VM.
struct CardPose<'a> {
    play: Option<ModelPlayHead>,
    clock_ms: u64,
    rect: benilla_ui::layout::Rect,
    icon: &'a str,
    alpha: f32,
    scale: f32,
    model_scale: f32,
    position: (f32, f32, f32),
    facing: f32,
}

fn card_cache() -> &'static Mutex<HashMap<String, Option<Arc<CardFile>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<CardFile>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pane facts for the model key `key` (case-folded, no extension), read through the
/// archives: what `Model:SetModel` needs to take the file's implicit rect and seed its Stand arm.
pub(crate) fn facts_for(world: &BnWorld, key: &str) -> Option<ModelFileFacts> {
    card_file(world, key).map(|f| f.facts.clone())
}

fn card_file(world: &BnWorld, key: &str) -> Option<Arc<CardFile>> {
    let cache = card_cache();
    if let Ok(c) = cache.lock() {
        if let Some(hit) = c.get(key) {
            return hit.clone();
        }
    }
    let parsed = parse_card(world, key).map(Arc::new);
    if let Ok(mut c) = cache.lock() {
        c.insert(key.to_string(), parsed.clone());
    }
    parsed
}

fn parse_card(world: &BnWorld, key: &str) -> Option<CardFile> {
    let path = format!("{}.m2", key.replace('/', "\\"));
    let bytes = crate::doodad_rig::model_bytes(world, &path)?;
    let dir = path.rsplit_once('\\').map_or("", |(d, _)| d);
    parse_card_bytes(&bytes, dir)
}

fn parse_card_bytes(bytes: &[u8], dir: &str) -> Option<CardFile> {
    let subs: Vec<benilla_formats::RenderSubmesh> =
        benilla_formats::parse_m2_render_submeshes(bytes, dir, &[])
            .ok()?
            .into_iter()
            .filter(|s| s.icon_slot)
            .collect();
    if subs.is_empty() {
        return None;
    }
    let bones = crate::parse_m2_bones(bytes)?;
    let gseqs = crate::parse_m2_global_sequences(bytes);
    let anims = benilla_formats::parse_m2_animations(bytes);
    let facts = ModelFileFacts {
        sequences: anims
            .iter()
            .map(|a| SequenceFacts {
                anim_id: a.anim_id,
                duration_ms: a.end_ms.saturating_sub(a.start_ms),
                looping: a.looping,
            })
            .collect(),
        bbox: benilla_formats::parse_m2_bounds(bytes)
            .map_or(([0.0; 3], [0.0; 3]), |b| (b.bbox_min, b.bbox_max)),
        cameras: benilla_formats::parse_m2_pane_cameras(bytes).len() as u32,
    };
    let seqs = anims
        .iter()
        .map(|a| (a.anim_id, a.seq_index, a.start_ms))
        .collect();
    Some(CardFile {
        subs,
        bones,
        gseqs,
        seqs,
        facts,
    })
}

/// Pose each of a card's icon batches at its pane's play head: the icon-slot vertices ride the
/// bone palette, then the facing turns them about model +Z, one model unit is `1280 · modelScale ·
/// frameScale` root units, and the model root is the pane's bottom-left corner plus `SetPosition`.
fn pose(file: &CardFile, card: &CardPose, layout_unit: f32) -> Vec<CardQuad> {
    // The armed band's file slot and its start on the file timeline, where the bone keys are cut;
    // `None` draws the bind pose (slot 0's first keys), as the reference's rest skeleton does.
    let (slot, cursor_ms, start_ms) = match card.play {
        Some(ph) => match file.seqs.iter().find(|s| s.0 == ph.anim_id) {
            Some(&(_, slot, start)) => (Some(slot), ph.cursor_ms, start),
            None => (None, 0, 0),
        },
        None => (None, 0, 0),
    };
    let cursor_s = cursor_ms as f32 / 1000.0;
    let clock_s = card.clock_ms as f64 / 1000.0;
    let time_ms = start_ms.saturating_add(cursor_ms);
    let mats: Vec<crate::BnMat4> = (0..file.bones.len())
        .map(|i| {
            crate::bone_matrix_at(
                &file.bones,
                slot.unwrap_or(0),
                time_ms,
                card.clock_ms as u32,
                &file.gseqs,
                i,
            )
        })
        .collect();
    let (sin, cos) = card.facing.sin_cos();
    let px_per_unit = 1280.0 * card.model_scale * card.scale;
    let pos_unit = layout_unit * card.scale;
    let origin = (
        card.rect.left + card.position.0 * pos_unit,
        card.rect.bottom + card.position.1 * pos_unit,
    );
    let mut out = Vec::new();
    for sm in &file.subs {
        let weight = sm
            .alpha_anim
            .as_ref()
            .map_or(1.0, |a| a.sample(slot, cursor_s, clock_s));
        let alpha = (card.alpha * weight).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            continue; // the `A ≤ 0` cull (`0x707b3a`)
        }
        let mut verts: Vec<[f32; 4]> = Vec::with_capacity(sm.positions.len());
        for (i, p) in sm.positions.iter().enumerate() {
            // The bone matrices are in the mirrored UE frame (x, −y, z), as `doodad_rig` feeds
            // them; the model's own space is the raw WoW one.
            let local = [p[0], -p[1], p[2]];
            let joints = sm.joints.get(i).copied().unwrap_or([0, 0, 0, 0]);
            let weights = sm.weights.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
            let total: f32 = weights.iter().sum();
            let mut q = local;
            if total > 1e-6 {
                let mut acc = [0.0f32; 3];
                for k in 0..4 {
                    let bi = joints[k] as usize;
                    if bi >= mats.len() || weights[k] == 0.0 {
                        continue;
                    }
                    let sk = apply_point(&mats[bi], local);
                    for j in 0..3 {
                        acc[j] += weights[k] * sk[j];
                    }
                }
                q = acc;
            }
            let (px, py) = (q[0], -q[1]);
            // `wow_to_screen() · R_y(facing) · wow_to_bevy` is a counter-clockwise yaw about
            // model +Z in the screen's own (x right, y up) frame.
            let x = origin.0 + (px * cos - py * sin) * px_per_unit;
            let y = origin.1 + (px * sin + py * cos) * px_per_unit;
            let uv = sm.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
            verts.push([x, y, uv[0], uv[1]]);
        }
        let tris: Vec<[[f32; 4]; 3]> = sm
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter_map(|t| {
                let a = *verts.get(t[0] as usize)?;
                let b = *verts.get(t[1] as usize)?;
                let c = *verts.get(t[2] as usize)?;
                Some([a, b, c])
            })
            .collect();
        if !tris.is_empty() {
            out.push(CardQuad {
                alpha,
                texture: card.icon.to_string(),
                tris,
            });
        }
    }
    out
}

/// Row-major `m * v` (the translation in `m[3]`, `m[7]`, `m[11]`).
fn apply_point(m: &crate::BnMat4, v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2] + m[3],
        m[4] * v[0] + m[5] * v[1] + m[6] * v[2] + m[7],
        m[8] * v[0] + m[9] * v[1] + m[10] * v[2] + m[11],
    ]
}

/// Every visible item-push card, as TSV: one row per batch,
/// `alpha \t texture \t triangles \t x y u v \t …` in root units (y up), three corners per
/// triangle in the file's own index order. Empty when no card shows or no world is attached.
pub(crate) fn rows() -> String {
    let cards = crate::BN_UI.with(|slot| {
        let held = slot.borrow();
        let s = held.as_ref()?;
        Some((s.visible_model_cards(), s.model_layout_unit()))
    });
    let Some((cards, layout_unit)) = cards else {
        return String::new();
    };
    if cards.is_empty() {
        return String::new();
    }
    let Some(world) = (unsafe { crate::ui_feed::world_ptr().as_ref() }) else {
        return String::new();
    };
    let mut out = String::new();
    for card in &cards {
        let Some(file) = card_file(world, &model_key(&card.path)) else {
            continue;
        };
        let pane = CardPose {
            play: card.play,
            clock_ms: card.clock_ms,
            rect: card.rect,
            icon: &card.icon,
            alpha: card.alpha,
            scale: card.scale,
            model_scale: card.model_scale,
            position: card.position,
            facing: card.facing,
        };
        for quad in pose(&file, &pane, layout_unit) {
            out.push_str(&format!(
                "{:.4}\t{}\t{}",
                quad.alpha,
                quad.texture,
                quad.tris.len()
            ));
            for tri in &quad.tris {
                for v in tri {
                    out.push_str(&format!(
                        "\t{:.2} {:.2} {:.5} {:.5}",
                        v[0], v[1], v[2], v[3]
                    ));
                }
            }
            out.push('\n');
        }
    }
    out
}

/// Emit `ITEM_PUSH` for every queued push whose item template has landed, in wire order. Each
/// event is `(container, icon path)`: the stock `ItemAnim_OnEvent` compares the first against its
/// button's id and gives the model the second. A push whose template has not answered waits for
/// the next tick, [`PUSH_MAX_TRIES`] at most.
pub(crate) fn drain_item_pushes(script: &mut UiScript, cats: Option<&Catalogs>) {
    let pending = crate::net::take_item_pushes();
    if pending.is_empty() {
        return;
    }
    let mut still = Vec::new();
    for mut p in pending {
        let info = crate::net::ui_item_info(p.entry);
        let (name, _quality, icon) = crate::ui_feed::item_look(&info, cats);
        if name.is_none() {
            p.tries += 1;
            if p.tries < PUSH_MAX_TRIES {
                still.push(p);
            }
            continue;
        }
        // `arg2` is the icon path the reference builds from the display record (`0x491baa`).
        if let Some(icon) = icon {
            script.fire_event(
                "ITEM_PUSH",
                vec![
                    benilla_ui::script::ScriptValue::Int(p.container),
                    benilla_ui::script::ScriptValue::Str(icon),
                ],
            );
        }
    }
    crate::net::keep_item_pushes(still);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real card file, through the install: one 1000 ms clamped Stand sequence, a 0.02707 x
    /// 0.07962 header box (the app's `bag_tests` facts, cross-checked from the other side), and a
    /// four-vertex icon batch that fades in by 133 ms and out to nothing at the end.
    #[test]
    fn forced_backpack_item_poses_its_icon_batch() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let bytes = chain
            .read_file("interface\\itemanimations\\forcedbackpackitem.m2")
            .expect("read ForcedBackpackItem.m2");
        let file = parse_card_bytes(&bytes, "").expect("parse the card");
        assert_eq!(
            file.facts.sequences,
            vec![SequenceFacts {
                anim_id: 0,
                duration_ms: 1000,
                looping: false,
            }],
        );
        let (min, max) = file.facts.bbox;
        assert!(
            (max[0] - min[0] - 0.02707).abs() < 5e-4 && (max[1] - min[1] - 0.07962).abs() < 5e-4,
            "header box {:?}..{:?}",
            min,
            max
        );
        assert_eq!(file.facts.cameras, 0);

        let rect = benilla_ui::layout::Rect::new(0.0, 100.0, 200.0, 300.0);
        let card = CardPose {
            play: Some(ModelPlayHead {
                anim_id: 0,
                cursor_ms: 0,
            }),
            clock_ms: 0,
            rect,
            icon: r"Interface\Icons\INV_Misc_Bag_08",
            alpha: 1.0,
            scale: 1.0,
            model_scale: 1.0,
            position: (0.0, 0.0, 0.0),
            facing: 0.0,
        };
        let layout_unit = 768.0 * ((16.0f32 / 9.0).powi(2) + 1.0).sqrt();
        // At the band's start the weight is 0, the batch culled.
        assert!(pose(&file, &card, layout_unit).is_empty());
        let peak = CardPose {
            play: Some(ModelPlayHead {
                anim_id: 0,
                cursor_ms: 133,
            }),
            clock_ms: 133,
            ..card
        };
        let quads = pose(&file, &peak, layout_unit);
        assert_eq!(quads.len(), 1, "one icon batch");
        assert!((quads[0].alpha - 1.0).abs() < 1e-3, "opaque at 133 ms");
        assert_eq!(quads[0].tris.len(), 2, "four-vertex quad");
        // The 0.03 x 0.03 quad at 1280 root units per model unit.
        let (mut l, mut r, mut b, mut t) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for v in quads[0].tris.iter().flatten() {
            l = l.min(v[0]);
            r = r.max(v[0]);
            b = b.min(v[1]);
            t = t.max(v[1]);
        }
        assert!(
            (r - l - 0.030 * 1280.0).abs() < 4.0 && (t - b - 0.030 * 1280.0).abs() < 4.0,
            "posed quad {:.1} x {:.1} root units",
            r - l,
            t - b
        );
        // The model root rides the pane's bottom-left corner, so the card sits inside the frame.
        assert!(l >= rect.left && b >= rect.bottom, "inside the pane");
        // The one-shot clamps at its end, where the transparency reaches zero.
        let end = CardPose {
            play: Some(ModelPlayHead {
                anim_id: 0,
                cursor_ms: 1000,
            }),
            clock_ms: 1000,
            ..card
        };
        assert!(pose(&file, &end, layout_unit).is_empty(), "gone at 1 s");
    }
}

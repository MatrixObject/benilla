//! WoW `FontString` rasterization for the UE HUD painter.
//!
//! The host draws a whole FontString as one texture: this module shapes the
//! string with the client's own TTFs (read off the patch chain, same paths the
//! app crate's `ui_text` uses) under the reference's step law, applies WoW's
//! dark outline, and returns RGBA where the glyph is white and the outline is
//! black — so the host's per-region tint multiply reproduces the FontString
//! colour while the outline stays black.

use std::cell::RefCell;
use std::collections::HashMap;

use ab_glyph::{point, Font, FontArc, Glyph, PxScale, ScaleFont};

/// One rasterized string: RGBA8 pixels plus the pen box — the layout origin
/// offset inside the bitmap and the WoW-measured extent the host aligns to
/// the region rect under justifyH / justifyV.
pub struct TextBitmap {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// Pen-box top-left inside the bitmap, in bitmap pixels.
    pub ox: f32,
    pub oy: f32,
    /// Pen-box size (the measured string extent, not the ink box).
    pub pw: f32,
    pub ph: f32,
}

thread_local! {
    /// Faces by lowercased path, loaded once per process (fonts are small and
    /// never change mid-run).
    static FONTS: RefCell<HashMap<String, FontArc>> = RefCell::new(HashMap::new());
}

/// WoW's default UI face (what a FontString with no/broken font falls back to).
pub const DEFAULT_FONT: &str = r"Fonts\FRIZQT__.TTF";

/// Load (and memoize) one face off the patch chain. `path` is the WoW-style
/// path from `GetFont()` (`Fonts\FRIZQT__.TTF`); a bare file name is looked
/// up under `Fonts\`, mirroring the client's font open order loosely.
pub fn font_for(chain: &benilla_formats::Chain, path: &str) -> Option<FontArc> {
    let mut candidates: Vec<String> = Vec::new();
    let p = path.trim_matches('\0').trim();
    if !p.is_empty() {
        candidates.push(p.replace('/', "\\"));
        if !p.contains('\\') && !p.contains('/') {
            candidates.push(format!(r"Fonts\{p}"));
        }
    }
    candidates.push(DEFAULT_FONT.to_string());

    let mut loaded: Option<(String, FontArc)> = None;
    for cand in &candidates {
        let key = cand.to_ascii_lowercase();
        if let Some(f) = FONTS.with(|m| m.borrow().get(&key).cloned()) {
            loaded = Some((key, f));
            break;
        }
        if let Ok(bytes) = chain.read(cand) {
            if let Ok(f) = FontArc::try_from_vec(bytes) {
                loaded = Some((key.clone(), f.clone()));
                FONTS.with(|m| {
                    m.borrow_mut().insert(key.clone(), f);
                });
                break;
            }
        }
    }
    loaded.map(|(_, f)| f)
}

/// Strip WoW's pipe escapes (`|cffRRGGBB..`, `|r`, `|T..|t`, `|H..|h`) so the
/// rasterizer sees the displayed text, not the markup.
fn strip_escapes(s: &str) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(ch.len());
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] == '|' && i + 1 < ch.len() {
            match ch[i + 1] {
                'c' | 'C' => {
                    // |cRRGGBBAA colour open (6-8 hex digits).
                    i += 2;
                    let mut hex = 0usize;
                    while hex < 8 && i < ch.len() && ch[i].is_ascii_hexdigit() {
                        i += 1;
                        hex += 1;
                    }
                    continue;
                }
                'r' | 'R' => {
                    i += 2;
                    continue;
                }
                'T' | 't' => {
                    // |T...|t embedded texture: skip to and past "|t".
                    i += 2;
                    while i + 1 < ch.len() && !(ch[i] == '|' && ch[i + 1] == 't') {
                        i += 1;
                    }
                    i = (i + 2).min(ch.len());
                    continue;
                }
                'H' | 'h' => {
                    // "|H...|h" hyper/battle-net link: drop the tag around the
                    // label; the label itself stays visible.
                    i += 2;
                    while i + 1 < ch.len() && !(ch[i] == '|' && ch[i + 1] == 'h') {
                        i += 1;
                    }
                    i = (i + 2).min(ch.len());
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    out
}

/// Rasterize one (already display-sized) string.
///
/// * `size_px` — the drawn pixel size (the host applies WoW's em cap and the
///   seam scale before calling).
/// * `outline` — 0 none, 1 thin (WoW `OUTLINE`), 2 thick (`THICKOUTLINE`).
///
/// Returns `None` for empty text or a face that will not open.
pub fn rasterize(
    chain: &benilla_formats::Chain,
    font_path: &str,
    size_px: f32,
    outline: u32,
    text: &str,
) -> Option<TextBitmap> {
    let clean = strip_escapes(text);
    if clean.trim().is_empty() || size_px <= 0.5 {
        return None;
    }
    let font = font_for(chain, font_path)?;
    let size = size_px.max(1.0);
    let sf = font.as_scaled(PxScale::from(size));
    let thick = outline >= 2;
    let pad = if outline == 0 { 0.0 } else if thick { 2.0 } else { 1.0 };
    let ascent = sf.ascent();

    // ---- layout: the reference's step law (advance whole px + 1 [+1 thick]
    // + the pair's kern, rounded up), one line per '\n', pitch = size ----
    let mut lines: Vec<Vec<Glyph>> = Vec::new();
    let mut cur: Vec<Glyph> = Vec::new();
    let mut pen = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in clean.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
            pen = 0.0;
            prev = None;
            continue;
        }
        if ch == '\r' {
            continue;
        }
        let id = font.glyph_id(ch);
        let adv = sf.h_advance(id);
        let adv26 = (f64::from(adv) * 64.0).round() as i64;
        let mut step = (adv26 >> 6) as f32 + 1.0;
        if thick {
            step += 1.0;
        }
        if let Some(p) = prev {
            step += sf.kern(p, id);
        }
        let step = step.ceil();
        cur.push(id.with_scale_and_position(PxScale::from(size), point(pen, ascent)));
        pen += step;
        prev = Some(id);
    }
    lines.push(cur);
    let n_lines = lines.len();
    let pitch = size;
    let pen_w = lines
        .iter()
        .map(|l| l.last().map(|g| g.position.x + sf.h_advance(g.id)).unwrap_or(0.0))
        .fold(0.0f32, f32::max);
    let pen_h = n_lines as f32 * pitch;
    if pen_w <= 0.0 {
        return None;
    }

    // ---- outline the laid-out glyphs, collecting the ink box ----
    let mut min_x = 0.0f32;
    let mut min_y = 0.0f32;
    let mut max_x = pen_w;
    let mut max_y = pen_h;
    for (li, line) in lines.iter().enumerate() {
        let dy = li as f32 * pitch;
        for g0 in line {
            let mut g = g0.clone();
            g.position.y += dy;
            let Some(og) = sf.outline_glyph(g) else {
                continue;
            };
            let pb = og.px_bounds();
            min_x = min_x.min(pb.min.x);
            min_y = min_y.min(pb.min.y);
            max_x = max_x.max(pb.max.x);
            max_y = max_y.max(pb.max.y);
        }
    }

    let x0 = min_x.floor() - pad;
    let y0 = min_y.floor() - pad;
    let x1 = max_x.ceil() + pad;
    let y1 = max_y.ceil() + pad;
    let w = (x1 - x0).ceil().max(1.0) as u32;
    let h = (y1 - y0).ceil().max(1.0) as u32;
    if w > 8192 || h > 512 {
        return None; // pathological: refuse the allocation
    }
    let xi = x0 as i32;
    let yi = y0 as i32;

    // ---- coverage pass ----
    let mut g_cov = vec![0.0f32; (w * h) as usize];
    for (li, line) in lines.iter().enumerate() {
        let dy = li as f32 * pitch;
        for g0 in line {
            let mut g = g0.clone();
            g.position.y += dy;
            let Some(og) = sf.outline_glyph(g) else {
                continue;
            };
            let pb = og.px_bounds();
            let bx = pb.min.x.round() as i32 - xi;
            let by = pb.min.y.round() as i32 - yi;
            og.draw(|x, y, c| {
                let px = bx + x as i32;
                let py = by + y as i32;
                if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                    let idx = (py as u32 * w + px as u32) as usize;
                    if c > g_cov[idx] {
                        g_cov[idx] = c.min(1.0);
                    }
                }
            });
        }
    }

    // ---- outline dilation (chebyshev max over the pad radius) ----
    let r = pad as i32;
    let mut alpha = g_cov.clone();
    if r > 0 {
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut o = 0.0f32;
                for dy in -r..=r {
                    for dx in -r..=r {
                        let sx = x + dx;
                        let sy = y + dy;
                        if sx >= 0 && sy >= 0 && (sx as u32) < w && (sy as u32) < h {
                            let v = g_cov[(sy as u32 * w + sx as u32) as usize];
                            if v > o {
                                o = v;
                            }
                        }
                    }
                }
                let i = (y as u32 * w + x as u32) as usize;
                let g = g_cov[i];
                // glyph (white) over the dilated outline (black)
                let a = g + o * (1.0 - g);
                alpha[i] = a;
            }
        }
    }

    // ---- compose RGBA: rgb = white scaled by the glyph's share, a = alpha ----
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let a = alpha[i];
        if a <= 0.0 {
            continue;
        }
        let g = g_cov[i];
        let rgb = if a > 0.0001 { (g / a).clamp(0.0, 1.0) } else { 0.0 };
        let base = i * 4;
        let v = (rgb * 255.0).round() as u8;
        rgba[base] = v;
        rgba[base + 1] = v;
        rgba[base + 2] = v;
        rgba[base + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    Some(TextBitmap {
        rgba,
        w,
        h,
        ox: -x0,
        oy: -y0,
        pw: pen_w,
        ph: pen_h,
    })
}

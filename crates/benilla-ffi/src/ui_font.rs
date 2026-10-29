//! WoW `FontString` rasterization for the UE HUD painter.
//!
//! The host draws a whole FontString as one texture: this module shapes the
//! string with the client's own TTFs (read off the patch chain, same paths the
//! app crate's `ui_text` uses) under the client's step law, applies WoW's
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
/// path from `GetFont` (`Fonts\FRIZQT__.TTF`); a bare file name is looked
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
pub(crate) fn strip_escapes(s: &str) -> String {
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

/// One laid-out line under the step law: each glyph's pen x (where the glyph
/// is placed) plus its id, in reading order.
pub(crate) type ShapedLine = Vec<(f32, ab_glyph::GlyphId)>;

/// Shape `clean` (escapes already stripped) at `size` under the client's
/// step law — one line per `'\n'`, `'\r'` dropped — returning the lines with
/// their pen positions and the per-line pen widths (last glyph's x + its
/// horizontal advance, the extent the host aligns to under justifyH). Shared
/// by the rasterizer and the measurer so both agree on every advance.
pub(crate) fn shape(
    font: &FontArc,
    size: f32,
    thick: bool,
    clean: &str,
) -> (Vec<ShapedLine>, Vec<f32>) {
    let size = size.max(1.0);
    let sf = font.as_scaled(PxScale::from(size));
    let mut lines: Vec<ShapedLine> = Vec::new();
    let mut cur: ShapedLine = Vec::new();
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
        cur.push((pen, id));
        pen += step;
        prev = Some(id);
    }
    lines.push(cur);
    let widths = lines
        .iter()
        .map(|l| l.last().map(|(x, id)| x + sf.h_advance(*id)).unwrap_or(0.0))
        .collect();
    (lines, widths)
}

/// Preload every face the chain can name into the [`FONTS`] cache.
///
/// The measure answers inside `bn_ui_tick`, where no world handle is in
/// scope, so a face must already be local by then: the four client faces are
/// tried by name first, then the whole listing (a base archive carries no
/// `(listfile)`, so the listing can be partial). Returns how many faces the
/// cache now holds.
pub fn preload_fonts(chain: &benilla_formats::Chain) -> usize {
    const KNOWN: &[&str] = &[
        r"Fonts\FRIZQT__.TTF",
        r"Fonts\ARIALN.TTF",
        r"Fonts\MORPHEUS.TTF",
        r"Fonts\SKURRI.TTF",
    ];
    let load = |path: &str| -> bool {
        let key = path.replace('/', "\\").to_ascii_lowercase();
        if FONTS.with(|m| m.borrow().contains_key(&key)) {
            return false;
        }
        let Ok(bytes) = chain.read(path) else {
            return false;
        };
        let Ok(face) = FontArc::try_from_vec(bytes) else {
            return false;
        };
        FONTS.with(|m| {
            m.borrow_mut().insert(key, face);
        });
        true
    };
    for p in KNOWN {
        load(p);
    }
    if let Ok(entries) = chain.list() {
        for e in entries {
            let lower = e.name.to_ascii_lowercase();
            if lower.ends_with(".ttf") || lower.ends_with(".otf") {
                load(&e.name);
            }
        }
    }
    // The default face lands even if every name above missed: a FontString
    // with no font falls back to it, and the measure answers 0 without one.
    load(DEFAULT_FONT);
    FONTS.with(|m| m.borrow().len())
}

/// The cached face for `path`, the [`font_for`] candidate order minus the
/// chain read: `None` only when nothing was ever preloaded.
pub fn cached_font(path: &str) -> Option<FontArc> {
    let mut candidates: Vec<String> = Vec::new();
    let p = path.trim_matches('\0').trim();
    if !p.is_empty() {
        candidates.push(p.replace('/', "\\"));
        if !p.contains('\\') && !p.contains('/') {
            candidates.push(format!(r"Fonts\{p}"));
        }
    }
    candidates.push(DEFAULT_FONT.to_string());
    for cand in &candidates {
        let key = cand.to_ascii_lowercase();
        if let Some(f) = FONTS.with(|m| m.borrow().get(&key).cloned()) {
            return Some(f);
        }
    }
    None
}

/// One line's stepped pen (what the wrap kernel fits against) and its pen box
/// extent (the last glyph's x plus its advance, what [`rasterize`] reports):
/// [`shape`]'s law, `'\\n'` ending the walk and `'\\r'` dropping.
fn step_line(font: &FontArc, size: f32, thick: bool, s: &str) -> (f32, f32) {
    let size = size.max(1.0);
    let sf = font.as_scaled(PxScale::from(size));
    let mut pen = 0.0f32;
    let mut last: Option<(f32, ab_glyph::GlyphId)> = None;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in s.chars() {
        if ch == '\n' {
            break;
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
        last = Some((pen, id));
        pen += step;
        prev = Some(id);
    }
    let extent = match last {
        Some((x, id)) => x + sf.h_advance(id),
        None => 0.0,
    };
    (pen, extent)
}

/// `s` wrapped into byte ranges at `limit` px: a row ends at the last space
/// that fit (the break's whitespace dropping, the rest staying verbatim), or
/// at the fitting glyph when `non_space_wrap` says there is no break
/// opportunity — the line kernel's shape, priced by the same
/// extent [`measure`] reports for a row (the stepped pen up to the last glyph
/// plus that glyph's raw advance), so a region the measurer sized to its text
/// always holds that one row: pricing the fit with the stepped pen instead cut
/// every exactly-sized label one glyph short, which stacked a two-glyph CJK
/// button label vertically. Rounded up to a whole pixel as before.
/// A row always takes one char.
fn wrap_line(
    font: &FontArc,
    size: f32,
    thick: bool,
    s: &str,
    limit: f32,
    non_space_wrap: bool,
) -> Vec<(usize, usize)> {
    let chars: Vec<char> = s.chars().collect();
    let offs: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    let at = |i: usize| -> usize {
        if i >= chars.len() {
            s.len()
        } else {
            offs[i]
        }
    };
    if chars.is_empty() {
        return vec![(0, 0)];
    }
    let fit = limit.ceil();
    let sf = font.as_scaled(PxScale::from(size.max(1.0)));
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        // Extend the row while its stepped pen stays inside the width.
        let mut pen = 0.0f32;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        let mut end = start;
        while end < chars.len() {
            let ch = chars[end];
            if ch == '\r' {
                end += 1;
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
            // The row's price is what `step_line` would report as its extent:
            // the stepped pen up to this glyph plus this glyph's raw advance.
            if pen + adv > fit && end > start {
                break;
            }
            pen += step;
            prev = Some(id);
            end += 1;
        }
        if end >= chars.len() {
            rows.push((at(start), s.len()));
            break;
        }
        // The break opportunity inside the fitting part; none leaves the
        // glyph cut, as the kernel does when no classifier call answered.
        let mut row_end = end;
        let mut next = end;
        if !non_space_wrap {
            if let Some(sp) = (start + 1..end).rev().find(|&i| chars[i] == ' ') {
                row_end = sp;
                next = sp + 1;
            }
        }
        rows.push((at(start), at(row_end)));
        start = next.max(start + 1);
    }
    rows
}

/// The measure answer at the drawn raster size: `(laid_w, block_h, natural_w)`
/// in drawn px, so the caller divides its own `seam × scale` back out. The
/// laid pair is the pen box [`rasterize`] reports, so a region sized by it
/// holds the bitmap the HUD later draws (justify lands the same extent on
/// both sides); `natural_w` is the unwrapped extent `GetStringWidth` reports.
/// `spacing_px` is the inter-line gap already scaled, stepped to a whole
/// pixel by the same rounding.
pub fn measure(
    font_path: Option<&str>,
    size_px: f32,
    outline: u32,
    text: &str,
    wrap_px: Option<f32>,
    non_space_wrap: bool,
    max_lines: u32,
    spacing_px: f32,
) -> (f32, f32, f32) {
    if size_px <= 0.5 {
        return (0.0, 0.0, 0.0);
    }
    let clean = strip_escapes(text);
    let Some(font) = cached_font(font_path.unwrap_or(DEFAULT_FONT)) else {
        return (0.0, 0.0, 0.0);
    };
    let size = size_px.max(1.0);
    let thick = outline >= 2;
    let gap = if spacing_px > 0.0 {
        (spacing_px + 0.99999).trunc()
    } else {
        0.0
    };
    let wrap = wrap_px.filter(|w| *w > 1.0);

    let mut natural = 0.0f32;
    let mut widths: Vec<f32> = Vec::new();
    for line in clean.split('\n') {
        natural = natural.max(step_line(&font, size, thick, line).1);
        match wrap {
            Some(w) => {
                for (a, b) in wrap_line(&font, size, thick, line, w, non_space_wrap) {
                    widths.push(step_line(&font, size, thick, &line[a..b]).1);
                }
            }
            None => widths.push(step_line(&font, size, thick, line).1),
        }
    }
    let mut rows = widths.len().max(1);
    let mut laid = widths.iter().copied().fold(0.0f32, f32::max);
    if max_lines > 0 && rows > max_lines as usize {
        // `maxLines` cuts the laid rows first; the ellipsis the
        // client appends is a draw-side detail the width does not take.
        rows = max_lines as usize;
        laid = widths.iter().take(rows).copied().fold(0.0f32, f32::max);
    }
    let h = rows as f32 * size + (rows as f32 - 1.0) * gap;
    (laid, h, natural)
}

/// The EditBox advance table in drawn px: the stepped pen at every raw byte
/// boundary (`text.len + 1` from 0, a char's interior bytes carrying its
/// end's value), so index `i` is the x the char at byte `i` is drawn at —
/// the caret's own byte cursor. Markup is not stripped: an EditBox shows its
/// text literally, and the index must stay the raw string's.
pub fn advances_bytes(font_path: Option<&str>, size_px: f32, outline: u32, text: &str) -> Vec<f32> {
    let mut cum = vec![0.0f32; text.len() + 1];
    if size_px <= 0.5 {
        return cum;
    }
    let Some(font) = cached_font(font_path.unwrap_or(DEFAULT_FONT)) else {
        return cum;
    };
    let size = size_px.max(1.0);
    let thick = outline >= 2;
    let sf = font.as_scaled(PxScale::from(size));
    let mut written = vec![false; text.len() + 1];
    written[0] = true;
    let mut pen = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for (i, ch) in text.char_indices() {
        if ch == '\n' || ch == '\r' {
            continue; // no glyph, no step: the pen carries past the break
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
        pen += step.ceil();
        prev = Some(id);
        let end = i + ch.len_utf8();
        cum[end] = pen;
        written[end] = true;
    }
    for i in 1..cum.len() {
        if !written[i] {
            cum[i] = cum[i - 1];
        }
    }
    cum
}

/// The EditBox row table in raw byte starts, plus the row pitch in drawn px:
/// [`wrap_line`] over each hard line, with the VM's own bookkeeping after it
/// — a retained walk never passes the end, a text
/// ending in a break keeps the empty row its caret sits on, and an empty text
/// is one row. `non_space_wrap` is forced on, as the box's draw does
pub fn wrap_rows(
    font_path: Option<&str>,
    text: &str,
    size_px: f32,
    outline: u32,
    wrap_px: f32,
    spacing_px: f32,
) -> (Vec<usize>, f32) {
    let size = size_px.max(1.0);
    let gap = if spacing_px > 0.0 {
        (spacing_px + 0.99999).trunc()
    } else {
        0.0
    };
    let cell_h = size + gap;
    let mut rows: Vec<usize> = Vec::new();
    if size_px <= 0.5 || wrap_px <= 1.0 {
        // No wrap asked for: one row per hard line, by byte.
        let mut at = 0usize;
        for line in text.split('\n') {
            rows.push(at);
            at += line.len() + 1;
        }
    } else {
        let Some(font) = cached_font(font_path.unwrap_or(DEFAULT_FONT)) else {
            return (vec![0], cell_h);
        };
        for line in text.split('\n') {
            // wrap_line ranges are relative to `line`; the hard line's own
            // start rides along, so rows stay raw byte offsets into `text`.
            let base = line.as_ptr() as usize - text.as_ptr() as usize;
            for (a, _b) in wrap_line(&font, size, outline >= 2, line, wrap_px, true) {
                rows.push(base + a);
            }
        }
    }
    rows.retain(|&at| at < text.len());
    if text.ends_with('\n') {
        rows.push(text.len());
    }
    if rows.is_empty() {
        rows.push(0);
    }
    (rows, cell_h)
}

/// Rasterize one (already display-sized) string, one hard line per `'\n'`
/// (rows advance by the em). [`rasterize_wrapped`] adds width wrapping.
///
/// * `size_px` — the drawn pixel size (the host applies WoW's em cap and the
/// seam scale before calling).
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
    let thick = outline >= 2;
    rasterize_rows(&font, size, thick, outline, &clean, size)
}

/// Rasterize one `FontString` wrapped to `wrap_px` drawn pixels wide (the
/// region's own width) at `size_px`; hard `'\n'` still breaks rows, so quest
/// paragraphs survive. Row pitch is the face's own line height plus
/// `spacing_px` (WoW `SetSpacing`), so wrapped rows never overlap the way the
/// plain em pitch made them. `wrap_px <= 1` means no width wrap.
pub fn rasterize_wrapped(
    chain: &benilla_formats::Chain,
    font_path: &str,
    size_px: f32,
    outline: u32,
    text: &str,
    wrap_px: f32,
    spacing_px: f32,
) -> Option<TextBitmap> {
    let clean = strip_escapes(text);
    if clean.trim().is_empty() || size_px <= 0.5 {
        return None;
    }
    let font = font_for(chain, font_path)?;
    let size = size_px.max(1.0);
    let thick = outline >= 2;

    // Wrap each hard line to `wrap_px` (the break's own whitespace dropping,
    // the rest verbatim) and join the rows with `'\n'` for `shape`.
    let laid: String = if wrap_px > 1.0 {
        let mut out = String::with_capacity(clean.len() + 16);
        for (li, line) in clean.split('\n').enumerate() {
            if li > 0 {
                out.push('\n');
            }
            let mut first = true;
            for (a, b) in wrap_line(&font, size, thick, line, wrap_px, false) {
                if !first {
                    out.push('\n');
                }
                out.push_str(&line[a..b]);
                first = false;
            }
        }
        out
    } else {
        clean
    };

    // The face's own line height (ascent minus descent plus the line gap) is
    // the client's `Resolved::size`; never smaller than the em.
    let sf = font.as_scaled(PxScale::from(size));
    let line_h = (sf.ascent() - sf.descent() + sf.line_gap()).max(size);
    let pitch = line_h + spacing_px.max(0.0);
    rasterize_rows(&font, size, thick, outline, &laid, pitch)
}

/// Shared body of [`rasterize`] / [`rasterize_wrapped`]: `text` is already laid
/// out with `'\n'` hard breaks; rows advance by `pitch`.
fn rasterize_rows(
    font: &FontArc,
    size: f32,
    thick: bool,
    outline: u32,
    text: &str,
    pitch: f32,
) -> Option<TextBitmap> {
    let sf = font.as_scaled(PxScale::from(size));
    let pad = if outline == 0 {
        0.0
    } else if thick {
        2.0
    } else {
        1.0
    };

    // ---- layout: the client's step law, shared with the measurer ----
    // Per-glyph y stays at `ascent`; the passes below add `li * pitch`.
    let (shaped, line_w) = shape(font, size, thick, text);
    let ascent = sf.ascent();
    let lines: Vec<Vec<Glyph>> = shaped
        .iter()
        .map(|line| {
            line.iter()
                .map(|(x, id)| id.with_scale_and_position(PxScale::from(size), point(*x, ascent)))
                .collect()
        })
        .collect();
    let n_lines = lines.len();
    let pen_w = line_w.iter().copied().fold(0.0f32, f32::max);
    let pen_h = if n_lines == 0 {
        0.0
    } else {
        (n_lines - 1) as f32 * pitch + size
    };
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
        let rgb = if a > 0.0001 {
            (g / a).clamp(0.0, 1.0)
        } else {
            0.0
        };
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

// ---- the VM's inline text measure ------------------------------------------

thread_local! {
    /// The host's pixel factor, `canvas height / 768`: the VM lays out in UI
    /// units and the host rasterizes at units x seam. Glyph steps ceil to whole
    /// pixels, so a measure answered at the unscaled size is not proportional
    /// to the drawn one and the rows would overflow the box they were laid out
    /// in.
    static SEAM: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}

/// Seat the host's pixel factor; a non-positive seam falls back to 1:1.
pub fn set_seam(seam: f32) {
    SEAM.with(|s| s.set(if seam > 0.0 { seam } else { 1.0 }));
}

/// The seam the measure answers under.
pub fn seam() -> f32 {
    SEAM.with(|s| s.get())
}

/// A FontString's drawn em: the one-to-one regime caps it at 32 units, a
/// `SetTextHeight` size draws uncapped, and no size at all is the stock default.
fn drawn_em(height: Option<f32>, text_height: Option<f32>) -> f32 {
    match (text_height, height) {
        (Some(t), _) => t,
        (None, Some(h)) => h.min(32.0),
        (None, None) => 12.0,
    }
}

/// [`TextMeasure`] over this module's own [`measure`], so the layout box a
/// FontString gets and the bitmap [`rasterize`] later returns come from one
/// step law. Without it every wrapped string measures 0 tall, which is what
/// makes `GossipResize` stack a giver's quest rows on top of its greeting.
pub struct VmMeasure;

impl benilla_ui::script::TextMeasure for VmMeasure {
    fn measure(&mut self, req: &benilla_ui::script::MeasureRequest) -> (f32, f32, f32) {
        let rs = seam() * req.scale;
        let (w, h, natural) = measure(
            req.font.as_deref(),
            drawn_em(req.height, req.text_height) * rs,
            req.outline as u32,
            &req.text,
            req.wrap_width.map(|x| x * rs),
            req.non_space_wrap,
            req.max_lines,
            req.spacing * rs,
        );
        (w / rs, h / rs, natural / rs)
    }
}

#[cfg(test)]
mod tests {
    use super::rasterize_wrapped;

    const FACE: &str = r"Fonts\FZLBJW.TTF";

    /// The login button's label: the measurer sized the region to this exact
    /// two-glyph string, so the wrap must take it as one row. Priced by the
    /// stepped pen it came out one glyph short and stacked the characters.
    #[test]
    fn a_label_sized_to_its_own_text_wraps_to_one_row() {
        let data = benilla_formats::wow_data_or_skip!();
        let chain = benilla_formats::open_chain(&data).expect("open chain");
        assert!(super::preload_fonts(&chain) > 0, "no fonts in the install");
        let seam = 1.875;
        let (w, _, _) = super::measure(Some(FACE), 16.0 * seam, 2, "登录", None, false, 0, 0.0);
        let box_w = w / seam;
        let bmp = rasterize_wrapped(&chain, FACE, 16.0 * seam, 2, "登录", box_w * seam, 0.0)
            .expect("raster");
        assert!(
            bmp.ph < 16.0 * seam * 1.5,
            "one row expected, pen box {} tall at {} px (box {:.1} VM units)",
            bmp.ph,
            16.0 * seam,
            box_w
        );
    }

    /// The tolerance is a fit test, not a licence to overflow: text twice the
    /// width of its box still breaks into rows, which is how a Chinese quest
    /// paragraph (no spaces to break at) reaches the player.
    #[test]
    fn a_long_cjk_paragraph_still_wraps() {
        let data = benilla_formats::wow_data_or_skip!();
        let chain = benilla_formats::open_chain(&data).expect("open chain");
        let text = "登录登录登录登录登录登录";
        let bmp = rasterize_wrapped(&chain, FACE, 30.0, 2, text, 60.0, 0.0).expect("raster");
        assert!(bmp.ph > 30.0 * 3.0, "{} rows expected", bmp.ph / 30.0);
    }
}

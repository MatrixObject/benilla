//! The loading-screen overlay's state machine (benilla's `loading_screen` + `game_tip` laws).
//!
//! The host feeds one observation per frame (map id, 3x3 readiness, focus residency, whether it
//! is in the world, dt) and forwards the boundary events (`Entry` / `Transfer` / `Snap` /
//! `Blackout`). Everything else - art resolution via `Map.dbc` -> `LoadingScreens.dbc`, the
//! monotonic progress bar, the 3-frame clear latch, the 1 s fallback raise, the tip cursor - lives
//! here so the UE side only decodes and draws.

use std::cell::RefCell;
use std::ffi::c_char;

use benilla_formats::GameTipsCatalog;
use benilla_ui::markup::{self, TokenKind};

use crate::BnWorld;

/// The reference's `CLEAR_AFTER_READY_FRAMES`: the destination must be presentable for three
/// consecutive frames before the cover drops (`0x407e80`).
const CLEAR_AFTER_READY_FRAMES: u32 = 3;

/// How long the ground under the focus may stay off the world before a fallback raise, so a walk
/// across a tile seam does not flash the cover.
const FALLBACK_DELAY_S: f32 = 1.0;

/// A raise that never sees its `snap` must not pin the cover forever.
const AWAIT_TIMEOUT_S: f32 = 15.0;

/// `0xFFFFFFFF` in `tick`'s `map_id`: no map known (no art, no fallback raise by map).
pub const NO_MAP: u32 = u32::MAX;

const TIP_FONT_KOREAN: &str = r"Fonts\2002.TTF";
const TIP_FONT_CJK: &str = r"Fonts\FZLBJW.TTF";
const TIP_FONT_DEFAULT: &str = r"Fonts\FRIZQT__.TTF";

/// One frame's observation, mirroring `bn_loading_screen_tick`'s argument list.
#[derive(Clone, Copy, Debug, Default)]
pub struct BnLoadingScreenObs
{
    pub map_id: u32,
    pub ready: u32,
    pub total: u32,
    pub focus_resident: bool,
    pub in_world: bool,
    pub dt: f32,
}

/// The FFI snapshot the host draws from. Mirrors the C++ `BnLoadingScreen`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BnLoadingScreen
{
    pub active: u32,
    pub blackout: u32,
    pub awaiting: u32,
    pub ready: u32,
    pub total: u32,
    pub progress: f32,
}

/// The process-wide state: one loading screen per client.
#[derive(Clone, Debug, Default)]
pub struct LoadingScreenState
{
    active: bool,
    blackout: bool,
    awaiting: bool,
    map: Option<u32>,
    /// The latched art path; `None` = no art for this raise (draw the black backing only).
    art: Option<String>,
    art_resolved: bool,
    ready_frames: u32,
    displayed: f32,
    focus_gap: f32,
    awaiting_s: f32,
    tip: Option<String>,
    /// The next `gameTip` index (the reference stores the next one, never the shown one).
    tip_cursor: i64,
}

impl LoadingScreenState
{
    /// `Entry`: the glue-to-world raise. Resets the raise and takes the tip.
    pub fn raise(&mut self, map: Option<u32>, tip: Option<String>)
    {
        self.active = true;
        self.blackout = false;
        self.awaiting = true;
        self.map = map;
        self.art_resolved = false;
        self.ready_frames = 0;
        self.displayed = 0.0;
        self.focus_gap = 0.0;
        self.awaiting_s = 0.0;
        self.tip = tip;
    }

    /// `Transfer`: a far snap (map change). Active: end the wait, re-resolve art when the
    /// destination differs. Inactive: a raise without a tip (the fallback path).
    pub fn transfer(&mut self, map: Option<u32>)
    {
        if self.active {
            self.awaiting = false;
            if map != self.map {
                self.map = map;
                self.art_resolved = false;
            }
            return;
        }
        self.active = true;
        self.blackout = false;
        self.awaiting = true;
        self.map = map;
        self.art_resolved = false;
        self.ready_frames = 0;
        self.displayed = 0.0;
        self.focus_gap = 0.0;
        self.awaiting_s = 0.0;
    }

    /// `Snap`: the destination answered, only the wait ends.
    pub fn snap(&mut self)
    {
        self.awaiting = false;
    }

    /// `Blackout`: leaving the world (logout / disconnect / refused). Pure black, no tip.
    pub fn blackout_on(&mut self)
    {
        self.active = true;
        self.blackout = true;
        self.awaiting = false;
        self.ready_frames = 0;
        self.awaiting_s = 0.0;
    }

    /// One frame. `art_of` resolves a map id to its loading art (called at most once per raise).
    pub fn tick<F>(&mut self, o: BnLoadingScreenObs, mut art_of: F) -> BnLoadingScreen
    where
        F: FnMut(u32) -> Option<String>,
    {
        // 1. The blackout is only a transition: once the world reports it is gone, the menu's own
        //    black backing takes over.
        if self.blackout && !o.in_world {
            self.active = false;
            self.blackout = false;
            self.art = None;
            self.art_resolved = false;
            self.tip = None;
            self.awaiting = false;
            self.ready_frames = 0;
        }

        // 2. Fallback: the ground under the focus is missing and nothing covers the screen.
        if !self.active && o.in_world && !o.focus_resident {
            self.focus_gap += o.dt;
            if self.focus_gap >= FALLBACK_DELAY_S {
                self.focus_gap = 0.0;
                let map = (o.map_id != NO_MAP).then_some(o.map_id);
                self.transfer(map);
            }
        } else {
            self.focus_gap = 0.0;
        }

        // 3. A raise waiting for a destination snap that never comes must not hang the cover.
        if self.awaiting {
            self.awaiting_s += o.dt;
            if o.in_world && self.awaiting_s > AWAIT_TIMEOUT_S {
                self.awaiting = false;
            }
        }

        // Art latches once per raise (`art_resolved`), a miss is not retried.
        if self.active && !self.blackout && !self.art_resolved {
            self.art = self.map.and_then(|m| art_of(m));
            self.art_resolved = true;
        }

        // 4. The bar only grows; a total of 0 (no window yet) reads as 0.
        if !self.awaiting && o.total > 0 {
            let fraction = (o.ready.min(o.total) as f32) / (o.total as f32);
            if fraction > self.displayed {
                self.displayed = fraction;
            }
        }

        // 5. Clear after three consecutive ready frames; any miss resets the latch.
        let clearable = self.active
            && !self.blackout
            && o.in_world
            && !self.awaiting
            && o.total > 0
            && o.ready >= o.total;
        if clearable {
            self.ready_frames += 1;
            if self.ready_frames >= CLEAR_AFTER_READY_FRAMES {
                self.active = false;
                self.blackout = false;
                self.awaiting = false;
                self.ready_frames = 0;
                self.tip = None;
                self.art_resolved = false;
            }
        } else {
            self.ready_frames = 0;
        }

        // 6. The snapshot.
        BnLoadingScreen {
            active: u32::from(self.active),
            blackout: u32::from(self.blackout),
            awaiting: u32::from(self.awaiting),
            ready: o.ready,
            total: o.total,
            progress: if o.total > 0 {
                self.displayed.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }

    pub fn art(&self) -> Option<&str>
    {
        self.art.as_deref()
    }

    pub fn tip(&self) -> Option<&str>
    {
        self.tip.as_deref()
    }
}

thread_local! {
    static STATE: RefCell<LoadingScreenState> = RefCell::new(LoadingScreenState::default());
}

/// Take the next tip: the stored cursor is the *next* index, so out-of-range (and negative) reads
/// fall back to 0 (`0x46b682` clamps, it does not wrap by modulo). With `show` off neither the
/// text nor the cursor moves.
fn take_tip(catalog: Option<&GameTipsCatalog>, cursor: i64, show: bool) -> (Option<String>, i64)
{
    let (Some(catalog), true) = (catalog, show) else {
        return (None, cursor);
    };
    let count = catalog.len();
    if count == 0 {
        return (None, cursor);
    }
    let index = if cursor < 0 || cursor >= count as i64 {
        0
    } else {
        cursor as usize
    };
    let tip = catalog
        .get(index)
        .map(strip_markup)
        .filter(|text| !text.is_empty());
    (tip, index as i64 + 1)
}

/// Drop the colour/link tokens, honour `||` and line breaks, and trim the data's trailing `\r\n`
/// (the authored tips end in one, which would draw as an empty line and lift the block).
fn strip_markup(text: &str) -> String
{
    let mut out = String::new();
    let mut at = 0;
    while let Some(token) = markup::token_at(text, at) {
        at += token.byte_len.max(1);
        match token.kind {
            TokenKind::Char(c) => out.push(c),
            TokenKind::EscapedPipe => out.push('|'),
            TokenKind::LineBreak => out.push('\n'),
            TokenKind::Color(_) | TokenKind::ColorReset | TokenKind::LinkOpen { .. }
            | TokenKind::LinkClose => {}
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The locale's tip face (`0x406663`: koKR `2002.TTF`, zhCN/zhTW `FZLBJW.TTF`, else `FRIZQT__`).
fn tip_font_for(locale: Option<&str>) -> &'static str
{
    match locale {
        Some("koKR") => TIP_FONT_KOREAN,
        Some("zhCN") | Some("zhTW") => TIP_FONT_CJK,
        _ => TIP_FONT_DEFAULT,
    }
}

fn write_out(buf: *mut c_char, cap: u32, text: &str) -> u32
{
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let n = (text.len() as u64).min(cap as u64 - 1) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
    n as u32
}

impl BnWorld
{
    fn ensure_loading_screens(&mut self) -> bool
    {
        if self.loading_screens.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(catalog) = benilla_formats::load_loading_screens(chain) else {
                return false;
            };
            self.loading_screens = Some(catalog);
        }
        true
    }

    fn ensure_game_tips(&mut self) -> bool
    {
        if self.game_tips.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(catalog) = benilla_formats::load_game_tips(chain) else {
                return false;
            };
            self.game_tips = Some(catalog);
        }
        true
    }

    fn ensure_maps(&mut self) -> bool
    {
        if self.maps.is_none() {
            let Some(chain) = self.chain.as_mut() else {
                return false;
            };
            let Ok(catalog) = benilla_formats::load_map_catalog(chain) else {
                return false;
            };
            self.maps = Some(catalog);
        }
        true
    }

    /// `Map.dbc` `LoadingScreenID` -> `LoadingScreens.dbc` BLP path; `None` = pure black.
    fn loading_art(&mut self, map_id: u32) -> Option<String>
    {
        self.ensure_maps();
        self.ensure_loading_screens();
        let id = self.maps.as_ref()?.loading_screen_id(map_id)?;
        self.loading_screens
            .as_ref()?
            .path(id)
            .map(ToString::to_string)
    }

    fn locale_name(&self) -> Option<&'static str>
    {
        self.chain.as_ref().map(|chain| chain.locale().name())
    }
}

/// `edge`: 1 = Entry (picks the tip), 2 = Transfer, 3 = Snap, 4 = Blackout.
#[no_mangle]
pub extern "C" fn bn_loading_screen_edge(world: *mut BnWorld, edge: u32, map_id: u32) -> u32
{
    if world.is_null() {
        return 0;
    }
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let map = (map_id != NO_MAP).then_some(map_id);
        let mut tip = None;
        if edge == 1 {
            let show = crate::video_flag("showGameTips", true) != 0;
            let world = unsafe { &mut *world };
            world.ensure_game_tips();
            let cursor = STATE.with(|slot| slot.borrow().tip_cursor);
            let (text, next) = take_tip(world.game_tips.as_ref(), cursor, show);
            tip = text;
            STATE.with(|slot| slot.borrow_mut().tip_cursor = next);
        }
        STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            match edge {
                1 => state.raise(map, tip),
                2 => state.transfer(map),
                3 => state.snap(),
                4 => state.blackout_on(),
                _ => {}
            }
        });
        1u32
    }));
    out.unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bn_loading_screen_tick(
    world: *mut BnWorld,
    map_id: u32,
    ready: u32,
    total: u32,
    focus_resident: u32,
    in_world: u32,
    dt: f32,
    out: *mut BnLoadingScreen,
) -> u32
{
    if world.is_null() || out.is_null() {
        return 0;
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let obs = BnLoadingScreenObs {
            map_id,
            ready,
            total,
            focus_resident: focus_resident != 0,
            in_world: in_world != 0,
            dt: if dt.is_finite() { dt.max(0.0) } else { 0.0 },
        };
        let snapshot = STATE.with(|slot| {
            let mut state = slot.borrow_mut();
            let world = unsafe { &mut *world };
            state.tick(obs, |map| world.loading_art(map))
        });
        unsafe { *out = snapshot };
        1u32
    }));
    res.unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn bn_loading_screen_art(world: *mut BnWorld, buf: *mut c_char, cap: u32) -> u32
{
    if world.is_null() {
        return 0;
    }
    let art = std::panic::catch_unwind(|| {
        STATE.with(|slot| slot.borrow().art().unwrap_or_default().to_string())
    })
    .unwrap_or_default();
    write_out(buf, cap, &art)
}

#[no_mangle]
pub extern "C" fn bn_loading_screen_tip(world: *mut BnWorld, buf: *mut c_char, cap: u32) -> u32
{
    if world.is_null() {
        return 0;
    }
    let tip = std::panic::catch_unwind(|| {
        STATE.with(|slot| slot.borrow().tip().unwrap_or_default().to_string())
    })
    .unwrap_or_default();
    write_out(buf, cap, &tip)
}

#[no_mangle]
pub extern "C" fn bn_loading_screen_tip_font(world: *mut BnWorld, buf: *mut c_char, cap: u32) -> u32
{
    if world.is_null() {
        return 0;
    }
    let font = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let world = unsafe { &*world };
        tip_font_for(world.locale_name())
    }))
    .unwrap_or(TIP_FONT_DEFAULT);
    write_out(buf, cap, font)
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn obs(ready: u32, total: u32, in_world: bool) -> BnLoadingScreenObs
    {
        BnLoadingScreenObs {
            map_id: 0,
            ready,
            total,
            focus_resident: true,
            in_world,
            dt: 0.1,
        }
    }

    #[test]
    fn entry_raises_with_the_tip_and_latches_art()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), Some("hello".to_string()));
        assert!(state.active);
        assert!(state.awaiting);
        assert!(!state.blackout);
        assert_eq!(state.tip(), Some("hello"));

        let out = state.tick(obs(0, 9, true), |_| Some("art.blp".to_string()));
        assert_eq!(out.active, 1);
        assert_eq!(out.awaiting, 1);
        assert_eq!(out.progress, 0.0);
        assert_eq!(state.art(), Some("art.blp"));
        assert!(state.art_resolved);

        // The latch: a second frame with a different answer must not replace the art.
        state.snap();
        let out = state.tick(obs(1, 9, true), |_| Some("other.blp".to_string()));
        assert_eq!(state.art(), Some("art.blp"));
        assert_eq!(out.progress, 1.0 / 9.0);
    }

    #[test]
    fn clearing_needs_three_consecutive_ready_frames()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), None);
        state.snap();
        let mut art = |_: u32| Some("art.blp".to_string());

        let out = state.tick(obs(9, 9, true), &mut art);
        assert_eq!(out.active, 1);
        let out = state.tick(obs(9, 9, true), &mut art);
        assert_eq!(out.active, 1);
        let out = state.tick(obs(9, 9, true), &mut art);
        assert_eq!(out.active, 0, "the third ready frame clears the cover");
        assert_eq!(state.ready_frames, 0);
        assert_eq!(state.tip(), None);
    }

    #[test]
    fn a_not_ready_frame_resets_the_latch()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), None);
        state.snap();
        let mut art = |_: u32| Some("art.blp".to_string());

        state.tick(obs(9, 9, true), &mut art);
        state.tick(obs(9, 9, true), &mut art);
        assert_eq!(state.ready_frames, 2);
        state.tick(obs(8, 9, true), &mut art);
        assert_eq!(state.ready_frames, 0);
        assert!(state.active);
        assert_eq!(state.displayed, 1.0, "the bar does not fall back");
    }

    #[test]
    fn transfer_inactive_raises_and_active_only_ends_the_wait()
    {
        let mut state = LoadingScreenState::default();
        state.transfer(Some(1));
        assert!(state.active);
        assert!(state.awaiting);
        assert!(state.map == Some(1));

        state.raise(Some(0), None);
        let mut art = |_: u32| Some("a.blp".to_string());
        state.tick(obs(0, 9, true), &mut art);
        assert_eq!(state.art(), Some("a.blp"));

        state.transfer(Some(1));
        assert!(!state.awaiting);
        assert!(!state.art_resolved, "the destination map re-resolves the art");
        assert_eq!(state.art(), Some("a.blp"), "the old art holds until the next tick");

        let out = state.tick(obs(0, 9, true), |map| {
            Some(if map == 1 { "b.blp" } else { "a.blp" }.to_string())
        });
        assert_eq!(state.art(), Some("b.blp"));
        assert_eq!(out.active, 1);
        assert_eq!(out.awaiting, 0);
    }

    #[test]
    fn transfer_on_the_same_map_keeps_the_art_latch()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), None);
        state.tick(obs(0, 9, true), |_| Some("a.blp".to_string()));
        state.transfer(Some(0));
        assert!(state.art_resolved);
    }

    #[test]
    fn blackout_dismisses_once_out_of_the_world()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), Some("tip".to_string()));
        state.tick(obs(0, 9, true), |_| Some("art.blp".to_string()));
        state.blackout_on();
        assert!(state.blackout);

        let out = state.tick(obs(0, 9, false), |_| Some("art.blp".to_string()));
        assert_eq!(out.active, 0);
        assert_eq!(out.blackout, 0);
        assert_eq!(state.tip(), None);
        assert_eq!(state.art(), None);
        assert!(!state.art_resolved);

        let out = state.tick(obs(0, 9, true), |_| Some("art.blp".to_string()));
        assert_eq!(out.active, 0, "no raise without the ground being ready");
    }

    #[test]
    fn progress_is_monotonic()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), None);
        state.snap();
        let mut art = |_: u32| Some("art.blp".to_string());
        for ready in [3, 9, 2, 7, 1, 8] {
            let out = state.tick(obs(ready, 9, true), &mut art);
            assert!(out.progress >= 3.0 / 9.0 - 1e-6, "ready={ready}");
        }
        assert_eq!(
            state.tick(obs(0, 0, true), &mut art).progress,
            0.0,
            "no window = no progress"
        );
    }

    #[test]
    fn fallback_waits_one_second_before_raising()
    {
        let mut state = LoadingScreenState::default();
        let mut art = |_: u32| Some("art.blp".to_string());
        let step = BnLoadingScreenObs {
            map_id: 0,
            ready: 0,
            total: 0,
            focus_resident: false,
            in_world: true,
            dt: 0.25,
        };
        let mut out = state.tick(step, &mut art);
        out = state.tick(step, &mut art);
        out = state.tick(step, &mut art);
        assert_eq!(out.active, 0, "0.75 s is still inside the debounce");
        out = state.tick(step, &mut art);
        assert_eq!(out.active, 1, "1.0 s raises the cover");
        assert!(state.awaiting);

        // A resident focus resets the gap.
        let mut state = LoadingScreenState::default();
        state.tick(
            BnLoadingScreenObs {
                focus_resident: false,
                dt: 0.5,
                ..step
            },
            &mut art,
        );
        state.tick(
            BnLoadingScreenObs {
                focus_resident: true,
                dt: 0.5,
                ..step
            },
            &mut art,
        );
        assert_eq!(state.focus_gap, 0.0);
    }

    #[test]
    fn an_away_raise_times_out_after_fifteen_seconds()
    {
        let mut state = LoadingScreenState::default();
        state.raise(Some(0), None);
        let mut art = |_: u32| Some("art.blp".to_string());
        let step = BnLoadingScreenObs {
            map_id: 0,
            ready: 9,
            total: 9,
            focus_resident: true,
            in_world: true,
            dt: 1.0,
        };
        for _ in 0..15 {
            state.tick(step, &mut art);
        }
        assert!(state.awaiting, "still waiting at 15 s");
        state.tick(step, &mut art);
        assert!(!state.awaiting, "past 15 s the wait must end");
    }

    #[test]
    fn tip_cursor_advances_and_clamps()
    {
        let catalog = GameTipsCatalog::from_tips(vec![
            "|cffffd100Tip:|r first\r\n".to_string(),
            "second\r\n".to_string(),
        ]);
        let (text, next) = take_tip(Some(&catalog), 0, true);
        assert_eq!(text.as_deref(), Some("Tip: first"));
        assert_eq!(next, 1);

        let (text, next) = take_tip(Some(&catalog), 1, true);
        assert_eq!(text.as_deref(), Some("second"));
        assert_eq!(next, 2);

        let (text, next) = take_tip(Some(&catalog), 2, true);
        assert_eq!(text.as_deref(), Some("Tip: first"), "past the end clamps to 0");
        assert_eq!(next, 1);

        let (text, next) = take_tip(Some(&catalog), -5, true);
        assert_eq!(text.as_deref(), Some("Tip: first"), "negative clamps to 0");
        assert_eq!(next, 1);

        let (text, next) = take_tip(Some(&catalog), 4, false);
        assert_eq!(text, None, "showGameTips off draws nothing");
        assert_eq!(next, 4, "and freezes the cursor");

        let (text, next) = take_tip(None, 3, true);
        assert_eq!(text, None, "no catalog draws nothing");
        assert_eq!(next, 3);
    }

    #[test]
    fn markup_is_stripped()
    {
        let text = strip_markup("|cffffd100Tip:|r use || and\r\n|Hitem:1|hlink|h");
        assert_eq!(text, "Tip: use | and\nlink");
        assert!(!text.contains('\r'));
        let text = strip_markup("plain\r\n\r\n");
        assert_eq!(text, "plain");
    }

    #[test]
    fn tip_font_follows_the_locale()
    {
        assert_eq!(tip_font_for(Some("koKR")), r"Fonts\2002.TTF");
        assert_eq!(tip_font_for(Some("zhCN")), r"Fonts\FZLBJW.TTF");
        assert_eq!(tip_font_for(Some("zhTW")), r"Fonts\FZLBJW.TTF");
        assert_eq!(tip_font_for(Some("enUS")), r"Fonts\FRIZQT__.TTF");
        assert_eq!(tip_font_for(None), r"Fonts\FRIZQT__.TTF");
    }
}

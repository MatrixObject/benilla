//! The nameplates: `CGNamePlateFrame`'s health-bar plate hung over a unit, ported across the FFI
//! boundary from `crates/benilla-app/src/vplates.rs`. The dll owns the world side — gate, anchor,
//! projection, seat — and hands the result to the widget layer as [`PlateState`]s, so the stock
//! `NAMEPLATES_ON` verbs drive real `CGNamePlateFrame` widgets in the VM and the host paints them
//! out of the dump it already pulls. The host adds nothing.
//!
//! - Toggles: the `[0xc4da34]` bitmask, bit `0x1` enemy and bit `0x8` friendly, both off at boot and
//!   written only by the four plate verbs, folded in [`fold_bits`].
//! - Gate: never our own unit, never a `NOT_SELECTABLE` one, never a ghost; health above 0; 20 yd;
//!   the `0x200` created-by rule; enemy or friendly by the category test below.
//! - Anchor: the overhead head point (`0x608640`) plus 2/3 yd (`[0x80abfc]`), projected through the
//!   same cone the host renders, the plate's top centre on it (`0x509ec0`), sized in gx units of the
//!   screen diagonal (`0x41ad10`), outside `UIParent`'s scale.
//! - Seat: squared distance from gx (0.4, 0.3) (`0x608870`), then [`crate::smart_rect`] off the
//!   plates already claimed.
//!
//! Three gaps, all from machinery the dll does not carry, each a rung that only bites outside a
//! first zone:
//! - [`plate_is_friendly`] and [`plate_rank`] reduce `CanAttack` (`0x606980`) to the two
//!   `FactionTemplate.dbc` reactions (`0x606640`), the comparator [`crate::selection_ring`] already
//!   answers the ring colour with, because the dll holds no reputation standings and no at-war walk.
//! - The anchor is the authored PlayerName attachment at the rest pose, not the posed one the
//!   reference reads (`0x608640`): no dll export answers another model's bone pose, so a unit whose
//!   animation lifts or drops its head hangs its plate at the bind height.
//! - [`held_bucket`] seats on the UI root's rect, which the host letterboxes inside the window; the
//!   reference's `WorldFrame` is the whole window.

use std::cell::{Cell, RefCell};

use benilla_formats::{FactionCatalog, Reaction};
use benilla_protocol::messages::ObjectFields;
use benilla_ui::script::{
    unit_is_grey, PlateGeometry, PlateState, UiScript, PLATE_BIT_ENEMY, PLATE_BIT_FRIEND,
};
use glam::Vec2;

use crate::creature::{catalog, m2_path};
use crate::net;
use crate::selection_ring::{factions, template_of};
use crate::smart_rect::{Seat, SmartBucket};
use crate::ui_font;
use crate::BnWorld;

/// The plate frame in gx units (`[0x87d9cc]`, `[0x87d9d0]`); child offsets from `0x7cb250` and
/// `0x7cb6d0`.
const PLATE_W: f32 = 0.1;
const PLATE_H: f32 = 0.025;
/// The health bar: BOTTOMLEFT at the plate's BOTTOMLEFT plus the offset.
const BAR_OFF_X: f32 = 0.0031;
const BAR_OFF_Y: f32 = 0.003125;
const BAR_W: f32 = 0.0804;
const BAR_H: f32 = 0.007025;
/// The name's BOTTOM sits at the plate CENTER; the level's CENTER at plate BOTTOMRIGHT plus
/// (-0.0092, +0.0071); the skull overlays the level. Deviation: `LEVEL_H` is 0.0086, one em under
/// the reference's 0.009, because the level number read too large.
const NAME_H: f32 = 0.01;
const LEVEL_H: f32 = 0.0086;
const LEVEL_OFF_X: f32 = 0.0092;
const LEVEL_OFF_Y: f32 = 0.0071;
const SKULL_SIZE: f32 = 0.01;
/// The raid-target icon (`0x7cb250`): its RIGHT on the border's LEFT, hanging off the plate's left
/// edge.
const RAID_ICON_SIZE: f32 = 0.02;
/// `[0x80abfc]`.
const PLATE_LIFT: f32 = 2.0 / 3.0;
/// 20 yd, hardcoded in the reference.
const MAX_DIST_SQ: f32 = 20.0 * 20.0;
/// Deviation: the reference dims non-target plates to `0x7F`; raised because that fades them too
/// far to read.
const DIM_ALPHA: f32 = 178.0 / 255.0;
/// `0x608640`'s fallback multiplier (`[0x80c5d0]`): a unit whose model names no PlayerName
/// attachment anchors overhead content at `feet + scale × bbox_z × 1.25`.
const OVERHEAD_FALLBACK_FACTOR: f32 = 1.25;
/// The cone the screen is drawn in: benilla-world's `view::CAM_FOVY` (45° vertical), which the
/// host stamps its own camera from every frame, so a plate lands on the body it names.
const CAM_FOVY: f32 = std::f32::consts::FRAC_PI_4;

/// The bar-fill palette, the dwords at `0xcf60d0`/`e8`/`c8`/`dc`, in sRGB.
const PLATE_HOSTILE: [f32; 3] = [1.0, 0.0, 0.0];
const PLATE_NEUTRAL: [f32; 3] = [1.0, 1.0, 0.0];
const PLATE_FRIENDLY: [f32; 3] = [0.0, 1.0, 0.0];
const PLATE_PLAYER: [f32; 3] = [0.0, 0.0, 1.0];

/// `0x7cbaa0`'s test order: reaction 1 or less red, else a player blue, else 4 or more green,
/// else (2 and 3) yellow.
fn plate_tint(rank: u8, is_player: bool) -> [f32; 3] {
    if rank <= 1 {
        PLATE_HOSTILE
    } else if is_player {
        PLATE_PLAYER
    } else if rank >= 4 {
        PLATE_FRIENDLY
    } else {
        PLATE_NEUTRAL
    }
}

/// The level con palette, `0x7cbd50`'s dwords, not FrameXML's `QuestDifficultyColor`.
const CON_RED: [f32; 3] = [1.0, 25.0 / 255.0, 25.0 / 255.0];
const CON_ORANGE: [f32; 3] = [1.0, 127.0 / 255.0, 63.0 / 255.0];
const CON_YELLOW: [f32; 3] = [1.0, 1.0, 0.0];
const CON_GREEN: [f32; 3] = [63.0 / 255.0, 178.0 / 255.0, 63.0 / 255.0];
const CON_GRAY: [f32; 3] = [127.0 / 255.0, 127.0 / 255.0, 127.0 / 255.0];

/// The con colour (`0x7cbd50`): +5 or more red, +3/+4 orange, -2..+2 yellow, then green until the
/// shared grey test says gray.
fn con_color(pl_level: u32, unit_level: u32) -> [f32; 3] {
    let diff = i64::from(unit_level) - i64::from(pl_level);
    if diff >= 5 {
        CON_RED
    } else if diff >= 3 {
        CON_ORANGE
    } else if diff >= -2 {
        CON_YELLOW
    } else if !unit_is_grey(pl_level, unit_level) {
        CON_GREEN
    } else {
        CON_GRAY
    }
}

/// Deviation: the reference's plates grow with the diagonal without limit (`0x7705b0`), which reads
/// too big and soft; past this diagonal, where the frame is the border art's native 128x32 px, the
/// basis grows at [`PLATE_GROWTH_DAMP`] of that rate.
const PLATE_DIAG_KNEE: f32 = 1280.0;
/// 0 pins the native size, 1 is the reference's rate.
const PLATE_GROWTH_DAMP: f32 = 0.5;

/// One gx unit in pixels: the screen diagonal (`0x41ad10`), never uiScale, damped past
/// [`PLATE_DIAG_KNEE`].
fn plate_basis(viewport: Vec2) -> f32 {
    let d = viewport.length();
    if d <= PLATE_DIAG_KNEE {
        d
    } else {
        PLATE_DIAG_KNEE + (d - PLATE_DIAG_KNEE) * PLATE_GROWTH_DAMP
    }
}

/// gx units to pixels for frame geometry; text takes [`text_px`].
fn gx_px(v: f32, basis: f32) -> f32 {
    (v * basis).round()
}

/// A plate FontString height to its em, `min(32, round(h * basis))`: the reference's `0x44d040` and
/// `0x5ca030` net `h` times the diagonal, capped at the 32 px atlas cell.
fn text_px(h: f32, basis: f32) -> f32 {
    (h * basis).round().min(32.0)
}

/// The two script bits of `[0xc4da34]`, both off at boot. The verbs clear the word at every world
/// entry (`0x60394b`), which is where the host's own reset lands.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct PlateMode {
    enemies: bool,
    friends: bool,
}

impl PlateMode {
    fn bits(self) -> u8 {
        let bit = |on: bool, b: u8| if on { b } else { 0 };
        bit(self.enemies, PLATE_BIT_ENEMY) | bit(self.friends, PLATE_BIT_FRIEND)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            enemies: bits & PLATE_BIT_ENEMY != 0,
            friends: bits & PLATE_BIT_FRIEND != 0,
        }
    }
}

thread_local! {
    static MODE: Cell<u8> = const { Cell::new(0) };
    /// The UI root's size in root units, off the host's `bn_ui_set_screen_size`.
    static ROOT_UNITS: Cell<[f32; 2]> = const { Cell::new([0.0; 2]) };
    /// The camera's backbuffer in device pixels, off the host's `bn_ui_set_viewport`.
    static SCREEN_PX: Cell<[f32; 2]> = const { Cell::new([0.0; 2]) };
    /// The plate seat bucket, rebuilt every frame; the app's combat text owns the other one.
    static BUCKET: RefCell<SmartBucket> = RefCell::new(SmartBucket::default());
    /// The last overhead height answered per display id and mount state, in WoW yards above the
    /// feet.
    static OVERHEAD: RefCell<std::collections::HashMap<(u32, bool), f32>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Fold the VM's plate verb writes into `[0xc4da34]`, in the order it made them.
fn fold_bits(script: &mut UiScript) -> PlateMode {
    let writes = script.take_nameplate_bit_writes();
    if writes.is_empty() {
        return PlateMode::from_bits(MODE.with(|m| m.get()));
    }
    let now = PlateMode::from_bits(writes.apply(MODE.with(|m| m.get())));
    MODE.with(|m| m.set(now.bits()));
    now
}

/// The engine clears `[0xc4da34]` at every world entry (`0x60394b`, `0x603952`) and a fresh VM holds
/// no plate, so a new load starts with both toggles off.
pub(crate) fn clear_bits() {
    MODE.with(|m| m.set(0));
}

/// The UI root's rect in root units; the host reports it every frame before the tick.
pub(crate) fn set_root_units(w: f32, h: f32) {
    ROOT_UNITS.with(|r| r.set([w, h]));
}

/// The camera's backbuffer in device pixels, so a world point projects where the host draws it.
pub(crate) fn set_screen_px(w: f32, h: f32) {
    SCREEN_PX.with(|s| s.set([w, h]));
}

/// The screen and where the UI root sits inside it, all in device pixels. `None` before the host
/// reports both.
struct SeatScreen {
    /// The backbuffer, the cone's own frame.
    screen: Vec2,
    /// The root's rect inside it: its size and its top-left corner.
    root: Vec2,
    off: Vec2,
    seam: f32,
}

fn seat_screen() -> Option<SeatScreen> {
    let [sw, sh] = SCREEN_PX.with(|s| s.get());
    let [rw, rh] = ROOT_UNITS.with(|r| r.get());
    let seam = ui_font::seam();
    if sw <= 0.0 || sh <= 0.0 || rw <= 0.0 || rh <= 0.0 || seam <= 0.0 {
        return None;
    }
    let root = Vec2::new(rw * seam, rh * seam);
    let raw = (Vec2::new(sw, sh) - root) * 0.5;
    Some(SeatScreen {
        screen: Vec2::new(sw, sh),
        root,
        // The host centres the root, so a window narrower than the root's aspect leaves the bars.
        off: Vec2::new(raw.x.max(0.0), raw.y.max(0.0)),
        seam,
    })
}

/// A world point (WoW yards, z up) to the backbuffer's device pixels, y down from the top: the
/// orbit's screen frame (`fwd`, `right = fwd × world_up`, `up = right × fwd`) and the cone's focal
/// length. `None` behind the camera or looking straight along the vertical, where the frame
/// degenerates.
fn project(eye: [f32; 3], fwd: [f32; 3], point: [f32; 3], screen: Vec2) -> Option<Vec2> {
    let [fx, fy, fz] = fwd;
    let rl = (fx * fx + fy * fy).sqrt();
    if rl < 1.0e-6 {
        return None;
    }
    let (rx, ry) = (fy / rl, -fx / rl);
    let d = [point[0] - eye[0], point[1] - eye[1], point[2] - eye[2]];
    let depth = d[0] * fx + d[1] * fy + d[2] * fz;
    if depth <= 0.0 {
        return None;
    }
    let x = d[0] * rx + d[1] * ry;
    let y = d[0] * (ry * fz) + d[1] * (-rx * fz) + d[2] * (rx * fy - ry * fx);
    let k = (screen.y * 0.5) / (CAM_FOVY * 0.5).tan();
    Some(Vec2::new(
        screen.x * 0.5 + x * k / depth,
        screen.y * 0.5 - y * k / depth,
    ))
}

/// The overhead attachment (`PlayerName`), where the name, combat text and quest marker hang.
const ATTACH_OVERHEAD: u16 = 18;
/// The mounted one (`PlayerNameMounted`, id 29), authored on the RIDER's own body model and
/// preferred while a mount is attached (`0x6074c0`, `0x608640`).
const ATTACH_OVERHEAD_MOUNTED: u16 = 29;

/// The height above its feet a unit's overhead content hangs (`0x608640`): the body model's
/// PlayerName attachment — 29 while mounted, else 18 — whose raw model-space Z is the point,
/// because the rest pose is identity and the pivot chain telescopes. Only a model naming neither
/// takes `[0x80c5d0]`'s box fallback, `bbox_z × 1.25`. The unit's wire scale multiplies at the
/// call site, as the app's entity transform does; the display id's DBC scale sizes the geometry,
/// which neither the app's anchor nor this one applies.
fn overhead_from_model(bytes: &[u8], mounted: bool) -> f32 {
    let slots: &[u16] = if mounted {
        &[ATTACH_OVERHEAD_MOUNTED, ATTACH_OVERHEAD]
    } else {
        &[ATTACH_OVERHEAD]
    };
    let authored = benilla_formats::parse_m2_attachments(bytes)
        .ok()
        .and_then(|list| {
            slots.iter().find_map(|slot| {
                list.iter()
                    .find(|a| a.id == *slot)
                    .map(|a| a.position[2].max(0.0))
            })
        });
    authored.unwrap_or_else(|| {
        benilla_formats::parse_m2_bounds(bytes)
            .map_or(0.0, |b| (b.bbox_max[2] - b.bbox_min[2]).max(0.0))
            * OVERHEAD_FALLBACK_FACTOR
    })
}

/// The same answer cached per model and mount state, since it needs an M2 read. A unit with no
/// loadable model anchors at its feet, as the client does.
fn overhead_height(world: &mut BnWorld, display_id: u32, mounted: bool) -> f32 {
    let key = (display_id, mounted);
    if let Some(h) = OVERHEAD.with(|o| o.borrow().get(&key).copied()) {
        return h;
    }
    let height = catalog(world)
        .as_ref()
        .and_then(|c| c.model(display_id))
        .and_then(|model| {
            crate::doodad_rig::model_bytes(world, &m2_path(&model.model_path))
                .map(|bytes| overhead_from_model(bytes.as_ref(), mounted))
        })
        .unwrap_or(0.0);
    OVERHEAD.with(|o| {
        o.borrow_mut().insert(key, height);
    });
    height
}

/// `CanAttack` from the player (`0x606980`), reduced to the template comparator: either side reads
/// the other as hostile. See the module header for what the reduction drops.
fn is_enemy(factions: Option<&FactionCatalog>, target: &ObjectFields, own: &ObjectFields) -> bool {
    let (Some(t), Some(p)) = (template_of(factions, target), template_of(factions, own)) else {
        return false;
    };
    t.reaction_toward(&p) == Reaction::Hostile || p.reaction_toward(&t) == Reaction::Hostile
}

/// The plate's category, `0x60f70b`-`0x60f72d`: not attackable from us is the friendly side, and a
/// player subject must also stand with us — which the template comparator alone answers the same
/// way.
fn plate_is_friendly(
    factions: Option<&FactionCatalog>,
    target: &ObjectFields,
    own: &ObjectFields,
) -> bool {
    !is_enemy(factions, target, own)
}

/// The bar colour's rank, `0x606530` by standing: the unit's own reaction toward us, Neutral where
/// either side has no template row.
fn plate_rank(factions: Option<&FactionCatalog>, target: &ObjectFields, own: &ObjectFields) -> u8 {
    template_of(factions, target)
        .zip(template_of(factions, own))
        .map_or(Reaction::Neutral, |(t, p)| t.reaction_toward(&p)) as u8
}

/// Every UI tick: gate the units, seat each plate, and hand the result to the widget layer.
pub(crate) fn drive(world: *mut BnWorld, script: &mut UiScript) {
    // SAFETY: the caller hands over the handle it holds on this thread, and the game thread owns
    // both the VM and the world it reads.
    let Some(world) = (!world.is_null()).then(|| unsafe { &mut *world }) else {
        script.retire_nameplates();
        return;
    };
    let mode = fold_bits(script);
    if !mode.enemies && !mode.friends {
        script.retire_nameplates();
        BUCKET.with(|b| b.borrow_mut().clear());
        return;
    }
    let (Some(cam), Some(self_guid), Some(own)) = (
        world.player.as_ref().map(|p| p.camera()),
        (net::ui_self_guid() != 0).then_some(net::ui_self_guid()),
        net::ui_self_fields(),
    ) else {
        script.retire_nameplates();
        return;
    };
    // `Player::camera` answers the orbit's eye and the point it looks at; the cone needs the
    // direction between them, which is what the host's camera rotation is made of.
    let eye = cam.0;
    let look = [
        cam.1[0] - cam.0[0],
        cam.1[1] - cam.0[1],
        cam.1[2] - cam.0[2],
    ];
    let ll = (look[0] * look[0] + look[1] * look[1] + look[2] * look[2]).sqrt();
    let fwd = if ll > 1.0e-6 {
        [look[0] / ll, look[1] / ll, look[2] / ll]
    } else {
        [1.0, 0.0, 0.0]
    };
    let player = (eye, fwd);
    let Some(view) = seat_screen() else {
        script.retire_nameplates();
        return;
    };
    let basis = plate_basis(view.root);
    let gx = |v: f32| gx_px(v, basis);
    let (pw, ph) = (gx(PLATE_W), gx(PLATE_H));
    let my_level = own.unit_level().unwrap_or(1);
    let selection = net::ui_selection();
    let hovered_key = script.hovered_nameplate();
    let clicked = script.take_nameplate_clicks();
    let cats = factions(world);
    let names = net::ui_names();

    // The body's position, for the 20 yd gate: the mover's own feet, which is what the camera and
    // the host draw. `ui_unit_pose` is the wire snapshot, and it lags the local body by the whole
    // distance walked.
    let Some(self_pos) = crate::player::ui_pos(world as *mut BnWorld)
        .or_else(|| net::ui_unit_pose(self_guid))
        .or_else(|| net::ui_unit(self_guid).map(|u| u.pos))
    else {
        script.retire_nameplates();
        return;
    };

    // Plates seat in squared-distance order from the gx point (0.4, 0.3), Y mirrored from the
    // reference's Y-up space (`0x608870`, `0x6089a0`, walked by `0x608ce0`). The undamped diagonal:
    // this is position, not size.
    let sort_pt = Vec2::new(
        0.4 * view.root.length(),
        view.root.y - 0.3 * view.root.length(),
    );
    let mut cands: Vec<(f32, Vec2, u8, bool, u64, ObjectFields)> = Vec::new();
    for guid in net::ui_unit_guids() {
        if guid == self_guid {
            continue;
        }
        let Some(unit) = net::ui_unit(guid) else {
            continue;
        };
        // Only a unit and a player: an item, container or game object has no plate.
        if unit.kind != 3 && unit.kind != 4 {
            continue;
        }
        let f = &unit.fields;
        // `NOT_SELECTABLE` (flags bit 25) never gets a plate, re-tested every tick (`0x60f622`).
        if f.unit_flags() & (1 << 25) != 0 {
            continue;
        }
        // `0x60f600`'s first test: health 0 or less (absent reads 0) has no plate.
        if f.unit_health().unwrap_or(0) == 0 {
            continue;
        }
        // `0x60f62e`: `bytes_1` byte 3 `& 3` (ghost or creep) has no plate.
        if f.unit_is_ghost_visual() {
            continue;
        }
        let feet = net::ui_unit_pose(guid).unwrap_or(unit.pos);
        let d = [
            feet[0] - self_pos[0],
            feet[1] - self_pos[1],
            feet[2] - self_pos[2],
        ];
        if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > MAX_DIST_SQ {
            continue;
        }
        // A CREATEDBY owner, no SUMMONEDBY owner and flags bit 9 (`0x200`): no plate in either
        // category (`0x60f70b`-`0x60f72d`); the bit's 1.12 name is unknown.
        if f.unit_created_by().is_some_and(|g| g != 0)
            && f.unit_summoned_by().is_none_or(|g| g == 0)
            && f.unit_flags() & 0x200 != 0
        {
            continue;
        }
        let friendly = plate_is_friendly(cats.as_deref(), f, &own);
        if friendly && !mode.friends || !friendly && !mode.enemies {
            continue;
        }
        // Only on the hostile side, a dead-looking unit has no plate (`0x60f733`).
        if !friendly && f.unit_reads_dead() {
            continue;
        }
        // Behind the camera or outside the root there is no plate: `0x60f600` destroys it before
        // the seat, never clamping it in from off-screen.
        // The anchor is the RIDER's own model, never the mount's: `0x6074c0` re-picks the
        // attachment to `PlayerNameMounted` on the body while a mount display is set.
        let lift = overhead_height(
            world,
            f.unit_displayid().unwrap_or(0).max(0) as u32,
            f.unit_mount_display_id() != 0,
        ) * f.object_scale_x().unwrap_or(1.0).max(0.01)
            + PLATE_LIFT;
        let Some(on_screen) = project(
            player.0,
            player.1,
            [feet[0], feet[1], feet[2] + lift],
            view.screen,
        ) else {
            continue;
        };
        // Into the root's own pixel frame, which is where the plate is painted and seated.
        let screen = on_screen - view.off;
        if !(0.0..=view.root.x).contains(&screen.x) || !(0.0..=view.root.y).contains(&screen.y) {
            continue;
        }
        cands.push((
            screen.distance_squared(sort_pt),
            screen,
            plate_rank(cats.as_deref(), f, &own),
            unit.kind == 4,
            guid,
            f.clone(),
        ));
    }
    cands.sort_by(|a, b| a.0.total_cmp(&b.0));

    BUCKET.with(|b| b.borrow_mut().clear());
    let mut states: Vec<PlateState> = Vec::with_capacity(cands.len());
    for (_, screen, rank, is_player, guid, f) in cands {
        let alpha = if selection == 0 || selection == guid {
            1.0
        } else {
            DIM_ALPHA
        };
        let tint = plate_tint(rank, is_player);
        // The seat (`0x509ec0`): the plate hangs from its top centre on the projected point, is
        // solved off the plates already claimed, then clamped half a plate inside the screen.
        let desired = Seat::new(
            screen.x - pw * 0.5,
            screen.y,
            screen.x + pw * 0.5,
            screen.y + ph,
        );
        let solved = BUCKET.with(|b| b.borrow().resolve(desired, view.root));
        // Snap the left edge, not the centre, which an odd width would put between texels; the
        // claim takes the snapped rect, so later plates dodge what is drawn. The host's canvas
        // pixels are the device pixels, so the snap is the identity.
        let left = solved.min.x.round();
        let top = solved.min.y.round();
        let plate = Seat::new(left, top, left + pw, top + ph);
        BUCKET.with(|b| b.borrow_mut().claim(plate));
        let hover = hovered_key == Some(guid);
        let level = f.unit_level();
        // The plates hang off `WorldFrame`, outside `UIParent`'s scale, so root units are their own.
        states.push(PlateState {
            key: guid,
            top_centre: (
                (plate.min.x + plate.max.x) * 0.5 / view.seam,
                (view.root.y - plate.min.y) / view.seam,
            ),
            // Raw health, not a fraction: `healthbar:GetValue()` is `[bar+0x320]` (`0x78f5d0`).
            health: f.unit_health().unwrap_or(0) as f32,
            max_health: f.unit_max_health().unwrap_or(1).max(1) as f32,
            bar_colour: tint,
            name: names
                .get(&guid)
                .cloned()
                .or_else(|| net::ui_creature_name(guid))
                .unwrap_or_default(),
            level,
            // The skull (`0x7cbb40`): a boss's rank rung needs `CreatureTemplate::rank`, which this
            // crate's creature cache does not carry; the hostile-ten-or-more rung is here.
            skull: level.is_some_and(|level| {
                rank <= 1 && level >= my_level + 10 && !unit_is_grey(my_level, level)
            }),
            level_colour: level.map_or([1.0, 1.0, 1.0], |level| con_color(my_level, level)),
            // The mark rides the raid roster's target list, which the feed does not carry; no raid,
            // no mark, which is what `GetRaidTargetIndex` answers on its own.
            raid_icon: None,
            alpha,
            lit: hover || selection == guid,
            hovered: hover,
        });
    }

    // A click on a plate is the reference's click slot (`0x7cb910`), ending in the same
    // `SetSelection` as a body click; only the left button selects.
    for click in &clicked {
        if click.button == "LeftButton" {
            net::select(click.key);
        }
    }

    // The widget layer rewrites the static anchors only when this moves, so an addon's own
    // re-anchoring survives between resizes.
    script.sync_nameplates(
        PlateGeometry {
            width: pw / view.seam,
            height: ph / view.seam,
            bar_off_x: gx(BAR_OFF_X) / view.seam,
            bar_off_y: gx(BAR_OFF_Y) / view.seam,
            bar_width: gx(BAR_W) / view.seam,
            bar_height: gx(BAR_H) / view.seam,
            level_off_x: gx(LEVEL_OFF_X) / view.seam,
            level_off_y: gx(LEVEL_OFF_Y) / view.seam,
            skull_size: gx(SKULL_SIZE) / view.seam,
            raid_size: gx(RAID_ICON_SIZE) / view.seam,
            name_height: text_px(NAME_H, basis) / view.seam,
            level_height: text_px(LEVEL_H, basis) / view.seam,
            // Deviation: a constant one-logical-pixel shadow, not the reference's 0.001 gx, which
            // draws 2 px thick and too heavy at larger windows. Divided by the seam, which the
            // shared text arm would otherwise scale it by.
            shadow_offset: 1.0 / view.seam,
        },
        &states,
    );
}

#[cfg(test)]
mod tests {
    use super::overhead_from_model;

    /// The wolf's authored `PlayerName` (attachment 18) is 2.365 yd above its feet, well under the
    /// 4.86 its all-animation box would give, so the two rungs are distinguishable on real data.
    #[test]
    fn overhead_prefers_the_authored_playername_attachment() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let bytes = chain
            .read_file(r"Creature\Wolf\Wolf.m2")
            .expect("read Wolf.m2");
        let h = overhead_from_model(&bytes, false);
        let bbox = benilla_formats::parse_m2_bounds(&bytes).unwrap();
        let fallback = (bbox.bbox_max[2] - bbox.bbox_min[2]) * super::OVERHEAD_FALLBACK_FACTOR;
        assert!(h > 1.0 && h < 3.0, "attachment height {h}");
        assert!(
            h < fallback,
            "{h} should sit under the box fallback {fallback}"
        );
    }

    /// A model that names no `PlayerNameMounted` still takes its `PlayerName` while the unit is
    /// mounted, and one naming neither answers the box: the wolf authors 17 and 18 only, so its
    /// mounted height is its standing one and both sit under the box it would otherwise use.
    #[test]
    fn overhead_keeps_the_attachment_when_the_mounted_slot_is_missing() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let bytes = chain
            .read_file(r"Creature\Wolf\Wolf.m2")
            .expect("read Wolf.m2");
        let list = benilla_formats::parse_m2_attachments(&bytes).unwrap();
        assert!(list.iter().all(|a| a.id != super::ATTACH_OVERHEAD_MOUNTED));
        let bbox = benilla_formats::parse_m2_bounds(&bytes).unwrap();
        let fallback =
            (bbox.bbox_max[2] - bbox.bbox_min[2]).max(0.0) * super::OVERHEAD_FALLBACK_FACTOR;
        let mounted = overhead_from_model(&bytes, true);
        assert_eq!(mounted, overhead_from_model(&bytes, false));
        assert!(mounted < fallback, "{mounted} vs the box {fallback}");
    }
}

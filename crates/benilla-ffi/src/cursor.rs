//! The world cursor. One pick under the pointer resolves to a mode as the reference's
//! `CGWorldFrame` classifier (`0x4828d0`, the unit arm `0x482200`) does, and that mode is one
//! sticky cell (`0xbe2c2c`) with two writers - this classifier and FrameXML's `SetCursor` family -
//! exactly as `crates/benilla-app/src/{target/cursor_mode.rs,cursor.rs}` drive it. The host draws
//! `Interface\Cursor\<stem>.blp` at the pointer; which stem this frame wants is all it asks.
//!
//! Three of the app's gates are not reproducible from the middle layer's own data, and each reads
//! as the app reads in the same state rather than as a new rule:
//! - the reputation and forced-reaction legs of `CanInteract`/`CanAttack` (`Reputations` is not
//!   cached here), so a unit resolves on its `FactionTemplate` pair alone - the same simplification
//!   the enemy cycle and the click-to-attack path already make, so the cursor never disagrees with
//!   what the click does;
//! - the Skinning learn latch (`[0xb700e4]`), so a skinnable corpse shows Point;
//! - the `gameobject_template` lock row, which the app also treats as unanswered: "until its
//!   template answers, a locked GameObject reads the gear", so Mine/GatherHerbs/PickLock are never
//!   set here.

use std::cell::RefCell;

use benilla_formats::Reaction;
use benilla_protocol::messages::ObjectFields;

use crate::net;
use crate::ui_feed;

/// The reference's mode table (`0x853b8c`); each name is an `Interface\Cursor\<Name>.blp` stem.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum CursorKind {
    /// The arrow: mode 0, and what every data gap reads.
    #[default]
    Point,
    Attack,
    Speak,
    /// Pickup(8): the vendor's pouch and the loot leg's base mode.
    Pickup,
    /// LootAll(16): the loot leg's triple pouch while the effective auto-loot is on.
    LootAll,
    /// Interact(5): the gear, for a GameObject with no data-named cursor and for the innkeeper.
    Interact,
    Buy,
    /// Inspect(7): the UI's Ctrl-hover magnifier and a readable TEXT(9) GameObject (`0x5f5890`).
    Inspect,
    Trainer,
    Taxi,
    /// The skinnable corpse's icon. The learn latch (`[0xb700e4]`) is not cached here, so the table
    /// keeps the mode without a writer - see the module doc.
    #[allow(dead_code)]
    Skin,
    /// Repair(17): never set by the world classifier; `ShowRepairCursor` (`0x4fbcc0`) parks the
    /// base cell there.
    Repair,
    /// Mail(15): a MAILBOX(19) or type-28 GameObject.
    Mail,
    /// Mine(11) / GatherHerbs(13) / PickLock(14): named by the lock's `LockType.dbc` row; set only
    /// once the template answers, which this build never asks.
    #[allow(dead_code)]
    Mine,
    #[allow(dead_code)]
    GatherHerbs,
    #[allow(dead_code)]
    PickLock,
    /// Cast(2): the spell-targeting cursor (`0x4820f0`), which pre-empts the object classifier.
    Cast,
}

impl CursorKind {
    fn name(self) -> &'static str {
        match self {
            CursorKind::Point => "Point",
            CursorKind::Attack => "Attack",
            CursorKind::Speak => "Speak",
            CursorKind::Pickup => "Pickup",
            CursorKind::LootAll => "LootAll",
            CursorKind::Interact => "Interact",
            CursorKind::Buy => "Buy",
            CursorKind::Inspect => "Inspect",
            CursorKind::Trainer => "Trainer",
            CursorKind::Taxi => "Taxi",
            CursorKind::Skin => "Skin",
            CursorKind::Repair => "Repair",
            CursorKind::Mail => "Mail",
            CursorKind::Mine => "Mine",
            CursorKind::GatherHerbs => "GatherHerbs",
            CursorKind::PickLock => "PickLock",
            CursorKind::Cast => "Cast",
        }
    }
}

/// This frame's cursor: `unable` picks the grayed `Unable<Name>` twin (the reference's `mode + 20`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct WorldCursor {
    pub(crate) kind: CursorKind,
    pub(crate) unable: bool,
}

impl WorldCursor {
    /// The stem the reference would park in the mode cell. `Point` has no grayed twin, and neither
    /// Repair nor PickLock is ever grayed (`0x4fbcc0` parks the base, `0x5f3070` skips the usable
    /// gate for `LockType.Id == 1`).
    fn stem(&self) -> &'static str {
        match (self.kind, self.unable) {
            (CursorKind::Point, _) | (CursorKind::Repair, _) | (CursorKind::PickLock, _) => {
                self.kind.name()
            }
            (k, false) => k.name(),
            (k, true) => match k {
                CursorKind::Attack => "UnableAttack",
                CursorKind::Speak => "UnableSpeak",
                CursorKind::Pickup => "UnablePickup",
                CursorKind::LootAll => "UnableLootAll",
                CursorKind::Interact => "UnableInteract",
                CursorKind::Buy => "UnableBuy",
                CursorKind::Inspect => "UnableInspect",
                CursorKind::Trainer => "UnableTrainer",
                CursorKind::Taxi => "UnableTaxi",
                CursorKind::Skin => "UnableSkin",
                CursorKind::Mail => "UnableMail",
                CursorKind::Mine => "UnableMine",
                CursorKind::GatherHerbs => "UnableGatherHerbs",
                CursorKind::Cast => "UnableCast",
                _ => self.kind.name(),
            },
        }
    }
}

/// `UNIT_NPC_FLAGS`, 1.12 values; REPAIR (`0x4000`) is left out because the ladder never tests it.
mod npc_flags {
    pub const GOSSIP: u32 = 0x1;
    pub const QUESTGIVER: u32 = 0x2;
    pub const VENDOR: u32 = 0x4;
    pub const FLIGHTMASTER: u32 = 0x8;
    pub const TRAINER: u32 = 0x10;
    pub const SPIRITHEALER: u32 = 0x20;
    pub const SPIRITGUIDE: u32 = 0x40;
    pub const INNKEEPER: u32 = 0x80;
    pub const BANKER: u32 = 0x100;
    pub const PETITIONER: u32 = 0x200;
    pub const TABARDDESIGNER: u32 = 0x400;
    pub const BATTLEMASTER: u32 = 0x800;
    pub const AUCTIONEER: u32 = 0x1000;
    pub const STABLEMASTER: u32 = 0x2000;
}

const UNIT_FLAG_NOT_SELECTABLE: u32 = 1 << 25;
const UNIT_FLAG_PACIFIED: u32 = 0x0002_0000;

/// NPC-service range, squared: gray beyond 5.5556 yd (`0xb4b32c`, checked boundary-inclusive at
/// `0x482320`).
const SERVICE_RANGE_SQ: f32 = 30.864;
/// Attack's range, squared: gray beyond a fixed 10.45 yd (`0x80447c`, checked at `0x4826a7`), not
/// the melee reach.
const ATTACK_RANGE_SQ: f32 = 109.2025;
/// Melee interact reach `max(reachA + reachB + 1.333, 5.0)` (`0x80b058`, `0x80a1e8`): gates loot.
const MELEE_OFFSET: f32 = 1.333_33;
const MELEE_FLOOR: f32 = 5.0;
/// A corpse's interact reach, squared: a flat 5 yd (`CanLootNow 0x5ec110`).
const CORPSE_INTERACT_RANGE_SQ: f32 = 25.0;

/// `ObjectType ids`: 3 unit, 4 player, 5 gameobject, 7 corpse. The rest (item, container,
/// dynamic, plain object) is never a pick the classifier reads.
const KIND_UNIT: u8 = 3;
const KIND_PLAYER: u8 = 4;
const KIND_GAMEOBJECT: u8 = 5;
const KIND_CORPSE: u8 = 7;

/// The types whose per-type strategy is constant false (`0x5f2f80` never runs), so the object shows
/// no cursor: GENERIC decoration, the transport family, and the marker set.
const GO_TYPE_GENERIC: i32 = 5;
const GO_TYPE_TRANSPORT: i32 = 11;
const GO_TYPE_MAP_OBJECT: i32 = 14;
const GO_TYPE_MO_TRANSPORT: i32 = 15;
const GO_TYPE_SPELL_FOCUS: i32 = 8;
const GO_TYPE_DUEL_ARBITER: i32 = 16;
const GO_TYPE_FISHINGHOLE: i32 = 25;
const GO_TYPE_AURA_GENERATOR: i32 = 30;
const GO_TYPE_AUCTIONHOUSE: i32 = 20;
const GO_TYPE_CAPTURE_POINT: i32 = 29;
const GO_TYPE_MAX: i32 = 30;
const GO_TYPE_GUARDPOST: i32 = 21;
/// TEXT(9) is a readable book: the magnifier. TRAP(6) inverts the faction term (`0x5f2fc6`).
const GO_TYPE_TEXT: i32 = 9;
const GO_TYPE_TRAP: i32 = 6;
const GO_TYPE_RITUAL: i32 = 18;
const GO_TYPE_MAILBOX: i32 = 19;
const GO_TYPE_28: i32 = 28;
/// FISHINGNODE(17) is the bobber, and CHAIR(7) seats within 3 yd (`0x5f5670`).
const GO_TYPE_FISHINGNODE: i32 = 17;
const GO_TYPE_CHAIR: i32 = 7;
const GO_TYPE_MEETINGSTONE: i32 = 23;

/// `GAMEOBJECT_TYPE_ID`'s interact range, squared: the NPC-service reach standing in for the
/// reference's per-type strategy constant (`usable 0x5f3130`), which is not transcribed.
fn go_interact_range_sq(type_id: i32) -> f32 {
    match type_id {
        GO_TYPE_FISHINGNODE => 100.0 * 100.0,
        GO_TYPE_CHAIR => 9.0,
        _ => SERVICE_RANGE_SQ,
    }
}

/// `GAMEOBJECT_TYPE_ID`, `GAMEOBJECT_FLAGS`, `GAMEOBJECT_DYNAMIC_FLAGS`.
const GO_FLAG_IN_USE_OR_NO_INTERACT: u32 = 0x1;
const GO_FLAG_INTERACT_COND: u32 = 0x4;
const GO_DYNFLAG_ACTIVATE: u32 = 0x1;

/// Whether a GameObject shows an interact cursor at all (`0x5f2f80` plus the per-type slots). The
/// FISHINGNODE override compares our channel object, and MEETINGSTONE's slot replaces the shared
/// gate with the queued area, which this build never learns - so both read as the reference does
/// with no active channel and no queue.
fn go_highlightable(
    guid: u64,
    fields: &ObjectFields,
    reaction: Option<u8>,
    own: &ObjectFields,
) -> bool {
    let type_id = fields.gameobject_type_id();
    if type_id == GO_TYPE_GUARDPOST || !(0..=GO_TYPE_MAX).contains(&type_id) {
        return false;
    }
    if matches!(
        type_id,
        GO_TYPE_GENERIC
            | GO_TYPE_TRANSPORT
            | GO_TYPE_MAP_OBJECT
            | GO_TYPE_MO_TRANSPORT
            | GO_TYPE_SPELL_FOCUS
            | GO_TYPE_DUEL_ARBITER
            | GO_TYPE_FISHINGHOLE
            | GO_TYPE_AURA_GENERATOR
            | GO_TYPE_AUCTIONHOUSE
            | GO_TYPE_CAPTURE_POINT
    ) {
        return false;
    }
    if type_id == GO_TYPE_FISHINGNODE && own.unit_channel_object() != Some(guid) {
        // Our channel object names this bobber (`0x5f6710`); without a channel it never does.
        return false;
    }
    if type_id == GO_TYPE_MEETINGSTONE {
        return true;
    }
    // The faction term (`0x5f3026`): beat hostile. An unresolved reaction passes, so a data gap
    // never blanks the world; TRAP inverts it.
    if type_id == GO_TYPE_TRAP {
        if !reaction.is_none_or(|r| r == Reaction::Hostile as u8) {
            return false;
        }
    } else if !reaction.is_none_or(|r| r > Reaction::Hostile as u8) {
        return false;
    }
    let flags = fields.gameobject_flags();
    if flags & GO_FLAG_IN_USE_OR_NO_INTERACT != 0 {
        return false;
    }
    if flags & GO_FLAG_INTERACT_COND != 0
        && fields.gameobject_dynamic_flags() & GO_DYNFLAG_ACTIVATE == 0
    {
        return false;
    }
    true
}

/// A highlightable GameObject's cursor (`GetCursorMode 0x5f8760`): TEXT and the Mail types by type,
/// any other its lock's data-named cursor or the gear. The lock arm needs the template, which is
/// unanswered here, so it takes the gear.
fn go_cursor_kind(type_id: i32) -> CursorKind {
    match type_id {
        GO_TYPE_TEXT => CursorKind::Inspect,
        GO_TYPE_RITUAL | GO_TYPE_MAILBOX | GO_TYPE_28 => CursorKind::Mail,
        _ => CursorKind::Interact,
    }
}

/// A GameObject's reaction toward us (`0x5f7fd0` → `0x606530`): the template comparator, no
/// faction is neutral (`0x5f8025`), `None` when either row is missing.
fn go_reaction(go_faction: u32, own: &ObjectFields) -> Option<u8> {
    if go_faction == 0 {
        return Some(Reaction::Neutral as u8);
    }
    let go = ui_feed::faction_row(go_faction)?;
    let own_tpl = ui_feed::faction_template(own)?;
    Some(go.reaction_toward(&own_tpl) as u8)
}

/// The QUESTGIVER bit's own gate (`0x5df490`): the cached status is neither NONE nor UNAVAILABLE,
/// and a missing one reads as NONE.
fn questgiver_has_quest(quest_status: Option<u32>) -> bool {
    use benilla_protocol::messages::dialog_status::{NONE, UNAVAILABLE};
    !matches!(quest_status, None | Some(NONE) | Some(UNAVAILABLE))
}

/// The service ladder (`0x482336..0x4824e3`, unrolled): lowest set bit wins. `None` when no tested
/// bit is set, which clears the cursor to Point without reaching the attack leg (`je 0x4826cb`).
fn service_cursor(service: u32, quest_status: Option<u32>) -> Option<CursorKind> {
    use npc_flags::*;
    if service & GOSSIP != 0 || (service & QUESTGIVER != 0 && questgiver_has_quest(quest_status)) {
        Some(CursorKind::Speak)
    } else if service & VENDOR != 0 {
        Some(CursorKind::Pickup)
    } else if service & FLIGHTMASTER != 0 {
        Some(CursorKind::Taxi)
    } else if service & TRAINER != 0 {
        Some(CursorKind::Trainer)
    } else if service & (SPIRITHEALER | SPIRITGUIDE) != 0 {
        Some(CursorKind::Speak)
    } else if service & INNKEEPER != 0 {
        Some(CursorKind::Interact)
    } else if service & BANKER != 0 {
        Some(CursorKind::Buy)
    } else if service & (PETITIONER | TABARDDESIGNER | BATTLEMASTER) != 0 {
        Some(CursorKind::Speak)
    } else if service & AUCTIONEER != 0 {
        Some(CursorKind::Buy)
    } else if service & STABLEMASTER != 0 {
        Some(CursorKind::Speak)
    } else {
        None
    }
}

/// The loot leg's mode, `8 + (keyDown(0) ? 8 : 0)` (`0x48252c`): the triple pouch while the
/// effective auto-loot, `autoLootDefault` XOR the held key, is on. 1.12 ships no such latch, so
/// with it off the key alone decides, as in the reference.
fn loot_cursor(auto_loot: bool, shift_held: bool) -> CursorKind {
    if auto_loot != shift_held {
        CursorKind::LootAll
    } else {
        CursorKind::Pickup
    }
}

/// `CanInteract 0x6067f0` from the local player: bit 25 clear, some `UNIT_NPC_FLAGS`, and both
/// reactions at least neutral. A row that does not resolve reads Neutral, as the app's
/// `ring_reaction` does.
fn can_interact(target: &ObjectFields, own: &ObjectFields) -> bool {
    if target.unit_flags() & UNIT_FLAG_NOT_SELECTABLE != 0 || target.unit_npc_flags() == 0 {
        return false;
    }
    let (Some(t), Some(p)) = (
        ui_feed::faction_template(target),
        ui_feed::faction_template(own),
    ) else {
        return true;
    };
    let neutral = Reaction::Neutral as u8;
    t.reaction_toward(&p) as u8 >= neutral && p.reaction_toward(&t) as u8 >= neutral
}

/// The player's legs of the attack leg (`0x482648`-`0x4826a2`): not dead or a ghost, not mounted,
/// not pacified. The reference's fourth term, control lost (`[0xb4b3e4]`), has no state in the
/// middle layer's mover, so it reads kept.
fn player_attack_legs(own: &ObjectFields) -> bool {
    !own.unit_is_dead()
        && !own.player_is_ghost()
        && own.unit_mount_display_id() == 0
        && own.unit_flags() & UNIT_FLAG_PACIFIED == 0
}

/// Centre-to-centre distance, squared, in yards: the classifier measures the way the reference
/// does, between the two bodies' own positions.
fn dist_sq(from: [f32; 3], to: [f32; 3]) -> f32 {
    let dx = from[0] - to[0];
    let dy = from[1] - to[1];
    let dz = from[2] - to[2];
    dx * dx + dy * dy + dz * dz
}

/// The unit arm (`0x482200`): a dead body's loot, else a service NPC's ladder, else the sword.
fn classify_unit(
    guid: u64,
    fields: &ObjectFields,
    pos: [f32; 3],
    own: &ObjectFields,
    own_pos: [f32; 3],
    shift_held: bool,
    auto_loot: bool,
) -> WorldCursor {
    let d2 = dist_sq(pos, own_pos);
    let reach =
        (fields.unit_combat_reach() + own.unit_combat_reach() + MELEE_OFFSET).max(MELEE_FLOOR);
    let point = WorldCursor::default();
    if fields.unit_is_dead() {
        if fields.unit_lootable() {
            // `CanLootNow 0x5ec110`'s melee reach grays the pouch.
            return WorldCursor {
                kind: loot_cursor(auto_loot, shift_held),
                unable: d2 > reach * reach,
            };
        }
        return point;
    }
    if can_interact(fields, own) {
        let Some(kind) = service_cursor(fields.unit_npc_flags(), net::ui_quest_status(guid)) else {
            return point;
        };
        return WorldCursor {
            kind,
            unable: d2 > SERVICE_RANGE_SQ,
        };
    }
    // Not attackable clears the cursor (`0x4826cb`): Point, never the sword. This is the same
    // arm the click-to-attack path uses, so the cursor and the swing agree.
    if !ui_feed::attackable(guid) || !player_attack_legs(own) {
        return point;
    }
    WorldCursor {
        kind: CursorKind::Attack,
        unable: d2 > ATTACK_RANGE_SQ,
    }
}

/// The corpse classifier (`0x482740`): a lootable body shows the pouch, grayed past 5 yd. The
/// insignia leg needs the `SPELL_EFFECT_SKIN_PLAYER_CORPSE` latch, which no 1.12.1 player holds.
fn classify_corpse(
    fields: &ObjectFields,
    pos: [f32; 3],
    own_pos: [f32; 3],
    shift_held: bool,
    auto_loot: bool,
) -> WorldCursor {
    if !fields.corpse_lootable() {
        return WorldCursor::default();
    }
    WorldCursor {
        kind: loot_cursor(auto_loot, shift_held),
        unable: dist_sq(pos, own_pos) > CORPSE_INTERACT_RANGE_SQ,
    }
}

/// The GameObject arm: an object that is not highlightable clears the cursor, as the reference's
/// handler does.
fn classify_go(
    guid: u64,
    fields: &ObjectFields,
    pos: [f32; 3],
    own: &ObjectFields,
    own_pos: [f32; 3],
) -> WorldCursor {
    let reaction = go_reaction(fields.gameobject_faction(), own);
    if !go_highlightable(guid, fields, reaction, own) {
        return WorldCursor::default();
    }
    WorldCursor {
        kind: go_cursor_kind(fields.gameobject_type_id()),
        // The lock arm of `usable 0x5f3130` needs the template; only the range gate runs here.
        unable: dist_sq(pos, own_pos) > go_interact_range_sq(fields.gameobject_type_id()),
    }
}

/// This frame's world cursor over the host's pick; no pick, or a kind that resolves to nothing,
/// reads Point.
fn classify(pick: u64, shift_held: bool, auto_loot: bool) -> WorldCursor {
    let Some(rec) = net::ui_unit(pick) else {
        return WorldCursor::default();
    };
    let Some(own) = net::ui_self_fields() else {
        return WorldCursor::default();
    };
    let Some(own_pos) = net::ui_player().map(|p| p.pos) else {
        return WorldCursor::default();
    };
    match rec.kind {
        KIND_UNIT | KIND_PLAYER => classify_unit(
            pick,
            &rec.fields,
            rec.pos,
            &own,
            own_pos,
            shift_held,
            auto_loot,
        ),
        KIND_GAMEOBJECT => classify_go(pick, &rec.fields, rec.pos, &own, own_pos),
        KIND_CORPSE => classify_corpse(&rec.fields, rec.pos, own_pos, shift_held, auto_loot),
        _ => WorldCursor::default(),
    }
}

/// [`State`]'s frame inputs, all device or pick state the host forwards raw: what is under the
/// pointer, and whether the shift key is held for the loot leg.
#[derive(Clone, Copy, Default)]
pub(crate) struct Inputs {
    pub(crate) pick: u64,
    pub(crate) shift_held: bool,
    /// A player-UI frame holds the hover: the world arm is only read while the WorldFrame does
    /// (`cursor.rs`'s `PointerOverUi`, which filters the hit through `is_world_frame`).
    pub(crate) over_ui: bool,
}

/// The sticky mode cell (`0xbe2c2c`) and the base last restored on entering the UI (`None` over the
/// world, so re-entry restores).
#[derive(Default)]
pub(crate) struct State {
    inputs: Inputs,
    displayed: WorldCursor,
    memo: Option<WorldCursor>,
}

thread_local! {
    static CURSOR: RefCell<State> = RefCell::new(State::default());
}

pub(crate) fn set_inputs(f: impl FnOnce(&mut Inputs)) {
    CURSOR.with(|s| f(&mut s.borrow_mut().inputs));
}

/// Reset the memo when the VM session changes (glue to world), as the app's `VmMemo` keyed on the
/// session does.
pub(crate) fn forget_vm() {
    CURSOR.with(|s| s.borrow_mut().memo = None);
}

/// Resolve this frame's displayed cursor and name its stem. `repair`, `gift_wrap` and `write` are
/// this frame's reads of the VM's own cells, and `write` is `None` when FrameXML set nothing.
pub(crate) fn stem_for(
    state: &mut State,
    auto_loot: bool,
    repair: bool,
    gift_wrap: bool,
    write: Option<Option<benilla_ui::script::UiCursorMode>>,
) -> &'static str {
    use benilla_ui::script::UiCursorMode;

    // The base cell is mutable and `ResetCursor` restores its value, not Point. While the UI holds
    // a gift wrap the base is Cast(2) (`0x5edea0`), which is why it survives crossing the world.
    let base = if repair {
        WorldCursor {
            kind: CursorKind::Repair,
            unable: false,
        }
    } else if gift_wrap {
        WorldCursor {
            kind: CursorKind::Cast,
            unable: false,
        }
    } else {
        WorldCursor::default()
    };

    // A FrameXML write wins over everything and re-arms the memo, whatever is under the pointer.
    if let Some(write) = write {
        state.displayed = match write {
            Some(UiCursorMode::Buy) => WorldCursor {
                kind: CursorKind::Buy,
                unable: false,
            },
            Some(UiCursorMode::UnableBuy) => WorldCursor {
                kind: CursorKind::Buy,
                unable: true,
            },
            Some(UiCursorMode::Inspect) => WorldCursor {
                kind: CursorKind::Inspect,
                unable: false,
            },
            Some(UiCursorMode::Cast) => WorldCursor {
                kind: CursorKind::Cast,
                unable: false,
            },
            Some(UiCursorMode::CastError) => WorldCursor {
                kind: CursorKind::Cast,
                unable: true,
            },
            Some(UiCursorMode::Point) => WorldCursor::default(),
            // `ResetCursor`: back to the base mode, Repair included.
            None => base,
        };
        state.memo = Some(base);
        return state.displayed.stem();
    }

    if !state.inputs.over_ui {
        state.displayed = classify(state.inputs.pick, state.inputs.shift_held, auto_loot);
        state.memo = None;
        return state.displayed.stem();
    }

    // Over the UI with no write: crossing in, or a base change, restores the base once. This one
    // edge stands for the `ResetCursor()` that ends nearly every reference hover handler.
    if state.memo != Some(base) {
        state.memo = Some(base);
        state.displayed = base;
    }
    state.displayed.stem()
}

/// Resolve and name this frame's cursor stem, reading the VM's own cells. The caller holds the UI
/// session; with no session the host keeps its own pointer, so this is only asked in the world.
pub(crate) fn stem(script: &mut benilla_ui::script::UiScript) -> &'static str {
    let (repair, gift_wrap, write) = (
        script.repair_mode(),
        script.gift_wrap_armed().is_some(),
        script.take_cursor_write(),
    );
    let auto_loot = script.cvar("autoLootDefault").is_some_and(|v| v == "1");
    CURSOR.with(|slot| stem_for(&mut slot.borrow_mut(), auto_loot, repair, gift_wrap, write))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder's order is the reference's unrolled test chain: gossip beats vendor, and only a
    /// questgiver with a cached quest answers Speak.
    #[test]
    fn service_ladder_takes_the_lowest_bit() {
        use benilla_protocol::messages::dialog_status;
        assert_eq!(
            service_cursor(npc_flags::GOSSIP | npc_flags::VENDOR, None),
            Some(CursorKind::Speak)
        );
        assert_eq!(
            service_cursor(npc_flags::VENDOR | npc_flags::TRAINER, None),
            Some(CursorKind::Pickup)
        );
        assert_eq!(service_cursor(npc_flags::QUESTGIVER, None), None);
        assert_eq!(
            service_cursor(npc_flags::QUESTGIVER, Some(dialog_status::AVAILABLE)),
            Some(CursorKind::Speak)
        );
        assert_eq!(
            service_cursor(npc_flags::AUCTIONEER, None),
            Some(CursorKind::Buy)
        );
        assert_eq!(service_cursor(0x4000, None), None);
    }

    /// 1.12 has no auto-loot latch: the shift alone picks the triple pouch.
    #[test]
    fn shift_holds_the_triple_pouch() {
        assert_eq!(loot_cursor(false, false), CursorKind::Pickup);
        assert_eq!(loot_cursor(false, true), CursorKind::LootAll);
        assert_eq!(loot_cursor(true, true), CursorKind::Pickup);
    }

    /// Every mode has its grayed twin except the three the reference never grays.
    #[test]
    fn grayed_twins() {
        let gray = |k: CursorKind| {
            WorldCursor {
                kind: k,
                unable: true,
            }
            .stem()
        };
        assert_eq!(gray(CursorKind::Attack), "UnableAttack");
        assert_eq!(gray(CursorKind::Speak), "UnableSpeak");
        assert_eq!(gray(CursorKind::LootAll), "UnableLootAll");
        assert_eq!(gray(CursorKind::Cast), "UnableCast");
        assert_eq!(gray(CursorKind::Point), "Point");
        assert_eq!(gray(CursorKind::Repair), "Repair");
        assert_eq!(gray(CursorKind::PickLock), "PickLock");
    }

    /// The FrameXML write is the sticky cell's other writer: it wins over the base and re-arms the
    /// restore, and `ResetCursor` (the `None` write) goes back to the base, Repair included.
    #[test]
    fn script_write_wins_and_reset_restores_the_base() {
        let mut s = State::default();
        assert_eq!(
            stem_for(
                &mut s,
                false,
                false,
                false,
                Some(Some(benilla_ui::script::UiCursorMode::Cast))
            ),
            "Cast"
        );
        assert_eq!(s.memo, Some(WorldCursor::default()));
        // A repair latch moves the base, and the reset restores that, not Point.
        assert_eq!(
            stem_for(
                &mut s,
                false,
                true,
                false,
                Some(Some(benilla_ui::script::UiCursorMode::Point))
            ),
            "Point"
        );
        assert_eq!(stem_for(&mut s, false, true, false, Some(None)), "Repair");
    }

    /// Crossing into the UI restores the base once; a second frame with no write holds it, and the
    /// world arm takes over again on the way out.
    #[test]
    fn ui_crossing_restores_the_base_once() {
        let mut s = State::default();
        s.inputs.over_ui = true;
        assert_eq!(stem_for(&mut s, false, false, false, None), "Point");
        s.memo = None;
        s.inputs.over_ui = false;
        s.inputs.pick = 0;
        assert_eq!(stem_for(&mut s, false, false, false, None), "Point");
    }

    /// A GameObject the type strategy refuses never shows a cursor, and the gear grays at the
    /// service range.
    #[test]
    fn gameobject_gates() {
        assert_eq!(go_cursor_kind(GO_TYPE_TEXT), CursorKind::Inspect);
        assert_eq!(go_cursor_kind(GO_TYPE_MAILBOX), CursorKind::Mail);
        assert_eq!(go_cursor_kind(0), CursorKind::Interact);
        assert_eq!(go_interact_range_sq(GO_TYPE_FISHINGNODE), 10_000.0);
        assert_eq!(go_interact_range_sq(GO_TYPE_CHAIR), 9.0);
        assert_eq!(go_interact_range_sq(GO_TYPE_TEXT), SERVICE_RANGE_SQ);
    }
}

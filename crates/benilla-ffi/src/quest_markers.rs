//! Overhead questgiver markers: when the client asks for an NPC's `!`/`?` and the model that
//! draws it.
//!
//! This is a port of the app's `crates/benilla-app/src/quest_markers/{mod,query}.rs` across the
//! FFI boundary: the reference's rules there (`0x607380`, `0x5eb070`, `0x607480`), the same
//! constants and the same per-unit ask key. The server never pushes a status — it only answers the
//! client's `CMSG_QUESTGIVER_STATUS_QUERY` (`QuestHandler.cpp:36-77`) — so every refresh is the
//! client's to trigger ([`sweep`]).
//!
//! - Sweep: a [`self_generation`] change (the reference's field watches) XOR the packet epoch
//!   (`SMSG_SET_FACTION_STANDING`, quest-log progress, a group roster).
//! - Per unit: a new unit or a moved [`unit_ask_key`] (`UNIT_NPC_FLAGS` and the faction template),
//!   torn down first and re-gated on a reaction above Hated/Hostile (`0x607380`).
//! - GameObject: asked only on a sweep, gated on `GAMEOBJECT_FLAGS` bit 2 and a reaction above 1;
//!   its answer is discarded by the reference's handler (`0x5dc9f0`), so it draws nothing.
//!
//! The host only paints: [`bn_quest_marker`] maps a status to the reference's marker `.m2`
//! (`0xc4d9d8` through the status map `0x80c454` = `{0,3,0,2,7,1,6,6}`), which it hangs at the
//! unit's overhead attachment with the reference's `1/L` seat scale and anim-0 bob.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use benilla_formats::{FactionCatalog, FactionTemplate, Reaction};
use benilla_protocol::messages::{ObjectFields, PLAYER_QUEST_LOG_SLOTS, PLAYER_SKILL_SLOTS};

use crate::net;

/// `UNIT_NPC_FLAGS` questgiver service bit.
const NPC_FLAG_QUESTGIVER: u32 = 0x2;
/// `UNIT_NPC_FLAGS` flightmaster bit, the gate on the only `CMSG_TAXINODE_STATUS_QUERY` sender.
const NPC_FLAG_FLIGHTMASTER: u32 = 0x8;
/// `GAMEOBJECT_FLAGS` bit 2, the sweep's whole GameObject gate (`0x5eb0ef`, shift 2).
const GO_FLAG_INTERACT_COND: u32 = 0x4;

/// Asks the dialog status of every questgiver-flagged creature, and re-asks when the answer could
/// have changed. Runs on the game thread from the UI tick, which is where the client's own sweep
/// runs; the asks go out as [`net::Cmd`]s like every other intent.
pub(crate) fn sweep(own: &ObjectFields, factions: Option<&FactionCatalog>) {
    let me = net::ui_self_guid();
    if me == 0 {
        return;
    }
    SWEEP.with(|cell| {
        let mut state = cell.borrow_mut();
        let state = &mut *state;
        let fields = self_generation(own);
        // A new self is a new world: every unit is unjudged and its cached answer gone.
        let mut force = false;
        if state.me != me {
            state.me = me;
            state.judged.clear();
            state.was_dead = false;
            state.my_faction = None;
            force = true;
        }
        let generation = fields ^ (net::ui_quest_epoch() << 32);
        // The full sweep's triggers (`0x5eb3c0`): our revive and our own reaction inputs moving.
        let alive = own.unit_health().unwrap_or(1) != 0;
        let my_faction = own.unit_faction_template();
        let full = (alive && state.was_dead)
            || (my_faction.is_some()
                && state.my_faction.is_some()
                && state.my_faction != my_faction);
        state.was_dead = !alive;
        if my_faction.is_some() {
            state.my_faction = my_faction;
        }
        let swept = force || state.generation != generation || full;
        state.generation = generation;

        let guids = net::ui_unit_guids();
        let alive_set: HashSet<u64> = guids.iter().copied().collect();
        // A guid that left the world drops its cached answer, with or without a self: the
        // reference keeps the status on the object, so it dies with the object.
        net::retain_quest_statuses(&alive_set);
        state.judged.retain(|guid, _| alive_set.contains(guid));

        let self_tpl = template(factions, own);
        for guid in guids {
            let Some(unit) = net::ui_unit(guid) else {
                continue;
            };
            match unit.kind {
                // Typemask exactly `OBJECT|UNIT` (`0x5eb0d3`): a player leaves the sweep here.
                3 => unit_leg(guid, &unit.fields, factions, self_tpl, swept, full, state),
                // `TYPEMASK_GAMEOBJECT` (`0x5eb0da`): sweep only, no ask key, no teardown.
                5 if swept => {
                    if unit.fields.gameobject_flags() & GO_FLAG_INTERACT_COND == 0 {
                        continue;
                    }
                    // A reaction toward us above 1 (`0x5eb101`): a faction-less object reads
                    // Neutral and a missing catalog passes, as in the cursor's gate.
                    if go_hostile(factions, unit.fields.gameobject_faction(), self_tpl) {
                        continue;
                    }
                    net::send_quest_query(guid);
                }
                _ => {}
            }
        }
    });
}

/// One creature (`0x5eb0d3`): teardown, re-gate on reaction, ask.
#[allow(clippy::too_many_arguments)]
fn unit_leg(
    guid: u64,
    fields: &ObjectFields,
    factions: Option<&FactionCatalog>,
    self_tpl: Option<FactionTemplate>,
    swept: bool,
    full: bool,
    state: &mut Sweep,
) {
    let key = unit_ask_key(fields);
    let judged = state.judged.get(&guid).copied();
    // A unit with no key has not been judged since it was created.
    let unjudged = judged.is_none();
    let key_moved = judged != Some(Some(key));
    if !swept && !full && !unjudged && !key_moved {
        return;
    }
    // The unit's reaction toward us (`0x6061e0`, at `0x5eb12a`): Hated or Hostile tears down.
    let hostile = match (template(factions, fields), self_tpl) {
        (Some(t), Some(p)) => t.reaction_toward(&p) == Reaction::Hostile,
        // A missing input reads Neutral, so a cold catalog never blanks a marker.
        _ => false,
    };
    if hostile {
        // A keyless judgement brings the marker back through a `0x607380` when the reaction
        // recovers.
        state.judged.insert(guid, None);
        net::clear_quest_status(guid);
        return;
    }
    state.judged.insert(guid, Some(key));
    // `0x607380`: tear down first (`0x607384`), then re-gate.
    let per_unit = key_moved || full;
    if per_unit {
        net::clear_quest_status(guid);
    }
    let flags = fields.unit_npc_flags();
    // `0x182` goes out from `0x607380` and from the light sweep, so every sweep re-asks.
    if (per_unit || swept) && flags & NPC_FLAG_QUESTGIVER != 0 {
        net::send_quest_query(guid);
    }
    // `0x1aa` has one sender, `0x607380`, so never on a light sweep.
    if per_unit && flags & NPC_FLAG_FLIGHTMASTER != 0 {
        net::send_taxi_query(guid);
    }
}

/// The faction template behind a descriptor, off the loaded catalog.
fn template(factions: Option<&FactionCatalog>, fields: &ObjectFields) -> Option<FactionTemplate> {
    factions?.template(fields.unit_faction_template()?).copied()
}

/// A GameObject's reaction toward us is Hated/Hostile (`0x5eb101` reads `<= 1`).
fn go_hostile(
    factions: Option<&FactionCatalog>,
    go_faction: u32,
    self_tpl: Option<FactionTemplate>,
) -> bool {
    if go_faction == 0 {
        return false; // faction-less reads Neutral
    }
    let Some(go_tpl) = factions.and_then(|c| c.template(go_faction)) else {
        return false; // a missing catalog passes, as in the cursor's gate
    };
    match self_tpl {
        Some(p) => go_tpl.reaction_toward(&p) == Reaction::Hostile,
        None => false,
    }
}

/// The per-unit ask key, the app's [`unit_ask_key`]: `UNIT_NPC_FLAGS`, which carries both watched
/// service bits, and the faction template, the reaction input we can see.
fn unit_ask_key(fields: &ObjectFields) -> u64 {
    u64::from(fields.unit_npc_flags())
        | (u64::from(fields.unit_faction_template().unwrap_or(0)) << 32)
}

/// Folds the self descriptor fields the reference watches into one value: level, the quest log,
/// money, every skill, `PLAYER_FLAGS`, alive or dead, and the equipment and bag guids in place of
/// the reference's `ITEM` watch. Byte for byte the app's `self_generation`.
fn self_generation(fields: &ObjectFields) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut h = 0xcbf2_9ce4_8422_2325u64; // FNV offset
    let mut fold = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(FNV_PRIME);
    };
    fold(u64::from(fields.unit_level().unwrap_or(0)));
    // Health enters as the alive bit: the `UNIT_FIELD_HEALTH` watch (`0x6046f0`) sweeps only on a
    // zero crossing; damage and regen ticks reach neither.
    fold(u64::from(fields.unit_health().unwrap_or(1) == 0));
    fold(u64::from(fields.player_flags()));
    fold(u64::from(fields.player_money().unwrap_or(0)));
    for slot in 0..PLAYER_QUEST_LOG_SLOTS {
        if let Some(s) = fields.player_quest_log(slot) {
            if s.quest_id != 0 {
                fold(u64::from(s.quest_id) ^ (u64::from(s.state) << 32));
            }
        }
    }
    // The reference watches the skill value word (`edx=0x84c`): a rank-up, not a bonus ticking.
    for slot in 0..PLAYER_SKILL_SLOTS {
        if let Some(s) = fields.player_skill(slot) {
            if s.skill_id != 0 {
                fold(u64::from(s.skill_id) ^ (u64::from(s.value) << 32));
            }
        }
    }
    for i in 0..23 {
        if let Some(guid) = fields.player_inv_slot(i) {
            fold(guid);
        }
    }
    h
}

/// [`sweep`]'s memory across frames.
#[derive(Default)]
struct Sweep {
    /// Our own guid this memory belongs to; a change is a new world.
    me: u64,
    /// The last [`self_generation`] XOR the packet epoch; a change here is the sweep.
    generation: u64,
    /// Last frame's alive state and faction template, the two full-sweep edges.
    was_dead: bool,
    my_faction: Option<u32>,
    /// Each judged unit's last [`unit_ask_key`], `None` when it was Hated/Hostile and torn down.
    judged: HashMap<u64, Option<u64>>,
}

thread_local! {
    static SWEEP: RefCell<Sweep> = RefCell::new(Sweep::default());
}

// ---- C ABI ----

/// `Interface\Buttons\TalkToMeGreen.m2`, the flight master's green `!` (file index 4 in the
/// reference's marker table): the app draws it while the node is unknown, in the one overhead slot
/// a quest marker would otherwise use.
const GREEN_MARKER: &str = "Interface\\Buttons\\TalkToMeGreen.m2";

/// The marker model for a dialog status, or `None` where the reference draws nothing (NONE 0,
/// CHAT 2). The statuses are UNAVAILABLE 1, INCOMPLETE 3, REWARD_REP 4, AVAILABLE 5, REWARD_OLD 6
/// and REWARD2 7; statuses 6 and 7 share the question mark, and the blue `!` (index 5) is
/// unreachable in 1.12.1.
fn marker_model(status: u32) -> Option<&'static str> {
    match status {
        1 => Some("Interface\\Buttons\\TalkToMeGrey.m2"),
        3 => Some("Interface\\Buttons\\TalkToMeQuestion_Grey.m2"),
        4 => Some("Interface\\Buttons\\TalkToMeQuestion_LTBlue.m2"),
        5 => Some("Interface\\Buttons\\TalkToMe.m2"),
        6 | 7 => Some("Interface\\Buttons\\TalkToMeQuestionMark.m2"),
        // Not a wire status: the host's green `!` slot.
        BN_MARKER_GREEN => Some(GREEN_MARKER),
        _ => None,
    }
}

/// [`bn_quest_marker`]'s status for the flight master's green `!`. The wire's dialog statuses are
/// 0..=7, so this value cannot collide with one.
pub const BN_MARKER_GREEN: u32 = 0x100;

/// One NPC's last `SMSG_QUESTGIVER_STATUS`, or `u32::MAX` when no answer has landed (or the object
/// left the stream window). The host maps it through [`bn_quest_marker`].
#[no_mangle]
pub extern "C" fn bn_net_quest_status(guid: u64) -> u32 {
    net::ui_quest_status(guid).unwrap_or(u32::MAX)
}

/// The flight master's `SMSG_TAXINODE_STATUS`: `0xFF` no answer yet, `1` known (draw nothing),
/// `0` unknown (the green `!`).
#[no_mangle]
pub extern "C" fn bn_net_taxi_known(guid: u64) -> u32 {
    match net::ui_taxi_known(guid) {
        None => 0xFF,
        Some(true) => 1,
        Some(false) => 0,
    }
}

/// The `.m2` path the reference draws for `status` (or [`BN_MARKER_GREEN`]): returns 1 and writes
/// the NUL-terminated path, 0 when that status draws nothing.
#[no_mangle]
pub extern "C" fn bn_quest_marker(status: u32, out: *mut std::ffi::c_char, cap: u32) -> u32 {
    let Some(path) = marker_model(status) else {
        return 0;
    };
    if out.is_null() || cap == 0 {
        return 0;
    }
    let bytes = path.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    // SAFETY: capacity checked above; the caller owns `cap` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const std::ffi::c_char, out, n);
        *out.add(n) = 0;
    }
    1
}

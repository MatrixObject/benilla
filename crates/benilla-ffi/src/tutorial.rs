//! The tutorial system behind the stock `TutorialFrame.xml`: the FFI port of
//! `crates/benilla-app/src/tutorial.rs` (the reference's `[0x4b5150, 0x4b5a60)`), whose two bit
//! banks the popup reads and whose `TUTORIAL_TRIGGER` event puts the pulsing `?` on the bar.
//!
//! - Both banks come from the same `SMSG_TUTORIAL_FLAGS` (0xFD); until it lands nothing fires.
//!   Bank A is fire-once (`TriggerTutorial` `0x4b5390`): it tests and sets its own bit and raises
//!   `TUTORIAL_TRIGGER(id + 1)`, never sending. Bank B is acknowledged (`0x4b54c0`): it tests, sets
//!   both banks, cancels the id's timer and sends `CMSG_TUTORIAL_FLAG`. So A ⊇ B, and doing the
//!   thing suppresses its tutorial account-wide.
//! - The world-enter bring-up (`0x4b5330`) zeroes both banks and the teardown (`0x4b5380`) flushes
//!   the timers; the enter cascade (`0x4908c0`) then fires Welcome and Questgivers once the local
//!   player exists, which is why a fresh account's first screen is the intro card.
//! - A movement input acknowledges Movement and arms the 10 s Targeting popup (`0x514a8d`), the
//!   锁定目标 card; Movement itself fires on 90 s of no movement input after world enter
//!   (`0x482ff7`).
//! - The four Lua verbs are drained after the VM tick: `FlagTutorial` acknowledges, and
//!   `ClearTutorials`/`ResetTutorials` write both banks and send `CMSG_TUTORIAL_CLEAR`/`_RESET`.
//!
//! Sites the reference has and this lane does not reach, for want of an edge here rather than of
//! the state machine: the mouse-look acknowledge of Cameras (its trigger still comes from the
//! level-4 arm), the window sites (Trainers, Vendors, Travel, Raid Groups), the UI-button sites
//! (Looting, the bag, Grouping, Friends, Chatting, the quest log), Swimming with its breath and
//! fatigue pair, Meeting Stones, and the battleground queue and port.

use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use benilla_ui::script::{ScriptValue, UiScript};

use crate::audio;
use crate::net::{self, Cmd};

/// The popup cue `0x4b5390` plays on both legs (`0x846b68`).
const TUTORIAL_POPUP_SOUND: &str = "TutorialPopup";

/// The internal (0-based) ids of the sites this lane produces, named by their published title.
mod id {
    pub(super) const QUESTGIVERS: u32 = 0x00;
    pub(super) const MOVEMENT: u32 = 0x01;
    pub(super) const TARGETING: u32 = 0x03;
    pub(super) const BACKPACK: u32 = 0x07;
    pub(super) const EQUIPPABLE_ITEMS: u32 = 0x17;
    pub(super) const DEATH: u32 = 0x18;
    pub(super) const RESTED: u32 = 0x19;
    pub(super) const RESTING: u32 = 0x1d;
    pub(super) const HEARTHSTONES: u32 = 0x1e;
    pub(super) const PVP_COMBAT: u32 = 0x1f;
    pub(super) const WELCOME: u32 = 0x29;
    pub(super) const RANGED_WEAPONS: u32 = 0x2b;
    pub(super) const KEYRINGS: u32 = 0x31;
}

/// The level-gated arm of the `SMSG_LEVELUP_INFO` handler (`0x5e38c0`): in the handler's own order,
/// each fires when the new level is at least its threshold.
const LEVEL_TRIGGERS: [(u32, u32); 8] = [
    (3, 0x16),  // Chatting
    (4, 0x0d),  // Trainers
    (4, 0x02),  // Cameras
    (5, 0x26),  // Groups
    (7, 0x15),  // Friends
    (7, 0x25),  // Professions
    (8, 0x20),  // Jumping
    (10, 0x0c), // Learning Talents
];

/// The Targeting popup's delay at its one site (`0x514a8d mov edx,0x2710`).
const TARGETING_DELAY_MS: u32 = 10_000;

/// The Movement popup's silence window (`0x482ff7`: elapsed - stamp - 90000 >= 0), taken as 90 s
/// after world enter with no movement input.
const MOVEMENT_SILENCE: Duration = Duration::from_millis(90_000);

const PLAYER_FLAGS_GHOST: u32 = 0x10;
const PLAYER_FLAGS_RESTING: u32 = 0x20;
const UNIT_FLAG_PVP_ATTACKABLE: u32 = 0x1000;

/// The hearthstone's item entry (`0x1b24`).
const HEARTHSTONE: u32 = 6948;
const INVTYPE_RANGED: u32 = 15;
const CLASS_HUNTER: u8 = 3;
/// The keyring bag and its slot band (`edi == 0xff`, slot ∈ [0x51, 0x70]).
const KEYRING_BAG: u8 = 0xff;
const KEYRING_SLOTS: RangeInclusive<u32> = 0x51..=0x70;

/// A send the drain owes the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Send {
    Flag(u32),
    Clear,
    Reset,
}

/// The self descriptor's last-seen fields, for the edge-triggered sites: the ghost and resting
/// flags, the PvP flag, and the rested table's one live row. The first sight of each arms silently,
/// so the login descriptor never fires.
#[derive(Default)]
struct Watch {
    player_flags: Option<u32>,
    unit_flags: Option<u32>,
    rest_state: Option<u8>,
}

/// The enter-world cascade's memory.
#[derive(Default)]
struct Cascade {
    /// Welcome and Questgivers are owed, once the local player exists.
    armed: bool,
    /// The movement-silence window's start; `None` once spent, and never set again by an input.
    entered_at: Option<Instant>,
    moved: bool,
}

/// The two banks, the timers, and what the frame owes the VM and the wire.
#[derive(Default)]
struct State {
    /// Bank A (`0xb711b8`…): fire-once. `None` until the packet lands.
    fire_once: Option<Vec<u8>>,
    /// Bank B (`0xb711e4`…): acknowledged.
    acknowledged: Option<Vec<u8>>,
    /// The delayed triggers (`TUTORIALTIMER` nodes): `(id, due)`.
    timers: Vec<(u32, Instant)>,
    /// Published ids (`id + 1`) whose cue and event are owed this frame.
    fired: Vec<u32>,
    sends: Vec<Send>,
    /// An `SMSG_TUTORIAL_FLAGS` payload not yet copied into the banks.
    flags: Option<Vec<u8>>,
    /// The last payload the account sent, kept so the world-enter bring-up can refill the banks it
    /// just zeroed — the app's `EnteredWorldMessage` carries the same bytes.
    last_flags: Option<Vec<u8>>,
    /// Our own item pushes since the last feed (`0x491a60`'s sites): `(entry, bag, slot)`.
    pushes: Vec<(u32, u8, u32)>,
    /// The levels `SMSG_LEVELUP_INFO` reported since the last feed.
    levels: Vec<u32>,
    /// The bank the live VM was last told, so a rebuilt VM is told again and an unchanged one is
    /// not re-pushed every frame.
    told: Option<Vec<u8>>,
    watch: Watch,
    cascade: Cascade,
}

/// A bank bit, byte-wise: the client's `id >> 5` dword indexing on little-endian dwords.
fn bank_bit(bank: &[u8], id: u32) -> Option<bool> {
    bank.get((id >> 3) as usize)
        .map(|b| b & (1 << (id & 7)) != 0)
}

fn set_bank_bit(bank: &mut [u8], id: u32) {
    if let Some(b) = bank.get_mut((id >> 3) as usize) {
        *b |= 1 << (id & 7);
    }
}

/// Every bit of both banks, to `0` (the bring-up and the reset) or `0xFF` (the clear).
fn fill_banks(banks: [Option<&mut Vec<u8>>; 2], value: u8) {
    for bank in banks.into_iter().flatten() {
        bank.iter_mut().for_each(|b| *b = value);
    }
}

impl State {
    /// `SMSG_TUTORIAL_FLAGS`: both banks copied from the payload, which is remembered for the next
    /// world enter's bring-up.
    fn apply_flags(&mut self, bytes: Vec<u8>) {
        self.last_flags = Some(bytes.clone());
        self.fire_once = Some(bytes.clone());
        self.acknowledged = Some(bytes);
    }

    /// The world-enter bring-up `0x4b5330` with the teardown `0x4b5380`'s timers: the banks zeroed
    /// if present (their length stays), what the old session owed dropped, and the cascade armed.
    fn world_enter(&mut self, now: Instant) {
        fill_banks([self.fire_once.as_mut(), self.acknowledged.as_mut()], 0);
        self.timers.clear();
        self.fired.clear();
        self.sends.clear();
        self.pushes.clear();
        self.levels.clear();
        self.told = None;
        self.watch = Watch::default();
        self.cascade = Cascade {
            armed: true,
            entered_at: Some(now),
            moved: false,
        };
        // The handshake's payload is the bring-up's own tail: the login path hands it over here, so
        // what the account already acknowledged survives the zeroing.
        if let Some(bytes) = self.last_flags.clone() {
            self.fire_once = Some(bytes.clone());
            self.acknowledged = Some(bytes);
        }
    }

    /// `TriggerTutorial(id, delayMs)` (`0x4b5390`).
    fn trigger(&mut self, id: u32, delay_ms: u32, now: Instant) {
        let Some(bank) = self.fire_once.as_mut() else {
            return; // no bank yet: silent
        };
        match bank_bit(bank, id) {
            Some(false) => set_bank_bit(bank, id),
            _ => return, // already set, or past the bank (the reference would overrun)
        }
        if delay_ms == 0 {
            self.fired.push(id + 1);
        } else {
            self.timers
                .push((id, now + Duration::from_millis(u64::from(delay_ms))));
        }
    }

    /// `SetTutorialFlag(id)` (`0x4b54c0`): bank B's bit gates, both banks set, the id's timer
    /// cancelled, the 0-based id sent. The reference dereferences an absent bank B; here it is a
    /// no-op.
    fn acknowledge(&mut self, id: u32) {
        let Some(bank_b) = self.acknowledged.as_mut() else {
            return;
        };
        if bank_bit(bank_b, id) != Some(false) {
            return;
        }
        self.timers.retain(|(t, _)| *t != id);
        set_bank_bit(bank_b, id);
        if let Some(bank_a) = self.fire_once.as_mut() {
            set_bank_bit(bank_a, id);
        }
        self.sends.push(Send::Flag(id));
    }

    /// `ClearTutorials()`: the timers flushed, every bit of both banks set, the empty clear sent.
    fn clear(&mut self) {
        self.timers.clear();
        fill_banks([self.fire_once.as_mut(), self.acknowledged.as_mut()], 0xFF);
        self.sends.push(Send::Clear);
    }

    /// `ResetTutorials()`: every bit of both banks cleared, the timers kept, the empty reset sent.
    fn reset(&mut self) {
        fill_banks([self.fire_once.as_mut(), self.acknowledged.as_mut()], 0);
        self.sends.push(Send::Reset);
    }

    /// `ProcessTutorialTimers`: every due node fires the same cue and event.
    fn tick(&mut self, now: Instant) {
        let mut due = Vec::new();
        self.timers.retain(|&(id, at)| {
            if at <= now {
                due.push(id + 1);
                false
            } else {
                true
            }
        });
        self.fired.extend(due);
    }

    /// The self descriptor's `0x4b5390` edge sites: the ghost flag (Death), the resting flag
    /// (Resting), the PvP-attackable flag (PvP Combat), and rest state 1 (Rested).
    fn watch_self(&mut self, now: Instant) {
        let Some(fields) = net::ui_self_fields() else {
            return;
        };
        let player_flags = fields.player_flags();
        let unit_flags = fields.unit_flags();
        let rest_state = fields.player_rest_state();
        let rose = |watch: Option<u32>, new: u32, bit: u32| {
            watch.is_some_and(|prev| prev & bit == 0 && new & bit != 0)
        };
        if rose(self.watch.player_flags, player_flags, PLAYER_FLAGS_GHOST) {
            self.trigger(id::DEATH, 0, now);
        }
        if rose(self.watch.player_flags, player_flags, PLAYER_FLAGS_RESTING) {
            self.trigger(id::RESTING, 0, now);
        }
        if rose(self.watch.unit_flags, unit_flags, UNIT_FLAG_PVP_ATTACKABLE) {
            self.trigger(id::PVP_COMBAT, 0, now);
        }
        if let (Some(prev), Some(new)) = (self.watch.rest_state, rest_state) {
            // Only rest state 1 names a tutorial; the other rows carry the sentinel.
            if new != prev && new == 1 {
                self.trigger(id::RESTED, 0, now);
            }
        }
        self.watch.player_flags = Some(player_flags);
        self.watch.unit_flags = Some(unit_flags);
        self.watch.rest_state = rest_state;
    }

    /// The item-received sites (`0x491a60`): Backpack always; Equippable Items for any non-zero
    /// inventory type (the reference's 23-bit mask `0x809200` is not decoded); Hearthstones by
    /// entry; Ranged Weapons for a ranged type on a non-hunter; Keyrings by the slot band.
    fn watch_pushes(&mut self, now: Instant) {
        for (entry, bag, slot) in std::mem::take(&mut self.pushes) {
            self.trigger(id::BACKPACK, 0, now);
            if let Some(info) = net::ui_item_template(entry) {
                if info.inventory_type != 0 {
                    self.trigger(id::EQUIPPABLE_ITEMS, 0, now);
                }
                let hunter =
                    net::ui_self_fields().and_then(|f| f.unit_class()) == Some(CLASS_HUNTER);
                if info.inventory_type == INVTYPE_RANGED && !hunter {
                    self.trigger(id::RANGED_WEAPONS, 0, now);
                }
            }
            if entry == HEARTHSTONE {
                self.trigger(id::HEARTHSTONES, 0, now);
            }
            if bag == KEYRING_BAG && KEYRING_SLOTS.contains(&slot) {
                self.trigger(id::KEYRINGS, 0, now);
            }
        }
    }

    /// The `SMSG_LEVELUP_INFO` arm (`0x5e38c0`): the packet only arrives on a rise, so every
    /// threshold the new level has reached fires, in the handler's own order.
    fn watch_levels(&mut self, now: Instant) {
        for level in std::mem::take(&mut self.levels) {
            for (threshold, id) in LEVEL_TRIGGERS {
                if level >= threshold {
                    self.trigger(id, 0, now);
                }
            }
        }
    }

    /// `0x4908c0`'s two unconditional triggers, once the local player exists, and the Movement
    /// popup's silence window.
    fn run_cascade(&mut self, now: Instant) {
        if self.cascade.armed && net::ui_self_guid() != 0 {
            self.cascade.armed = false;
            self.trigger(id::WELCOME, 0, now);
            self.trigger(id::QUESTGIVERS, 0, now);
        }
        if let Some(at) = self.cascade.entered_at {
            if !self.cascade.moved && now.duration_since(at) >= MOVEMENT_SILENCE {
                self.cascade.entered_at = None;
                self.trigger(id::MOVEMENT, 0, now);
            }
        }
    }

    /// The movement input site (`0x514840`'s forward and side axes): Movement acknowledged, the
    /// 10 s Targeting popup armed. Bank A's bit makes that once-only while a key stays held.
    fn watch_input(&mut self, input: (f32, f32, f32), now: Instant) {
        if input.0.abs() <= 0.1 && input.1.abs() <= 0.1 {
            return;
        }
        self.cascade.moved = true;
        self.cascade.entered_at = None;
        self.acknowledge(id::MOVEMENT);
        self.trigger(id::TARGETING, TARGETING_DELAY_MS, now);
    }
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `SMSG_TUTORIAL_FLAGS`, read on the session thread: the payload waits for the next feed, which
/// is where the banks live.
pub(crate) fn flags_arrived(bytes: Vec<u8>) {
    lock().flags = Some(bytes);
}

/// Our own `SMSG_ITEM_PUSH_RESULT`, queued for the feed's item-received sites.
pub(crate) fn item_received(entry: u32, bag: u8, slot: u32) {
    lock().pushes.push((entry, bag, slot));
}

/// `SMSG_LEVELUP_INFO`, queued for the feed's level arm.
pub(crate) fn level_up(level: u32) {
    lock().levels.push(level);
}

/// The world-enter bring-up, from `bn_ui_world_enter`. Flags that already landed stay queued and
/// are copied in on the next feed, which is the reference's order: bring-up, then whatever the
/// handshake delivered.
pub(crate) fn world_enter() {
    lock().world_enter(Instant::now());
}

/// A fresh VM is owed the bank again, as the app's per-VM memo: called from `bn_ui_load`, which
/// rebuilds it. The banks themselves are account state and outlive the VM.
pub(crate) fn vm_rebuilt() {
    lock().told = None;
}

/// Before the VM tick, the app's `feed_tutorials`: the sites, the timers, then the cue and the
/// event for every id that fired, and the acknowledged bank to the live VM.
pub(crate) fn sync(script: &mut UiScript) {
    let now = Instant::now();
    let (fired, bank) = {
        let mut st = lock();
        if let Some(bytes) = st.flags.take() {
            st.apply_flags(bytes);
        }
        st.watch_pushes(now);
        st.watch_levels(now);
        st.watch_self(now);
        st.run_cascade(now);
        st.watch_input(net::ui_input(), now);
        st.tick(now);
        // The bank the live VM has not been told yet, `None` when it already has it. The outer
        // `Option` is the change, the inner one the bank itself: an absent bank is owed to the VM
        // too, as the app's memo says.
        let bank = if st.told != st.acknowledged {
            st.told = st.acknowledged.clone();
            Some(st.acknowledged.clone())
        } else {
            None
        };
        (std::mem::take(&mut st.fired), bank)
    };
    for published in fired {
        audio::play_kit(TUTORIAL_POPUP_SOUND);
        script.fire_event(
            "TUTORIAL_TRIGGER",
            vec![ScriptValue::Int(i64::from(published))],
        );
    }
    if let Some(bank) = bank {
        script.set_tutorial_bank(bank);
    }
}

/// After the VM tick, the app's `drain_tutorials`: the four verbs' writes, then every send.
pub(crate) fn drain(script: &mut UiScript) {
    for id in script.take_tutorial_flag_requests() {
        lock().acknowledge(id);
    }
    for _ in 0..script.take_tutorial_clears() {
        lock().clear();
    }
    for _ in 0..script.take_tutorial_resets() {
        lock().reset();
    }
    for send in std::mem::take(&mut lock().sends) {
        let cmd = match send {
            Send::Flag(id) => Cmd::TutorialFlag { id },
            Send::Clear => Cmd::TutorialClear,
            Send::Reset => Cmd::TutorialReset,
        };
        net::send_cmd(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Looting's id: one this lane never triggers, so the acknowledge legs are its own.
    const SOME_ID: u32 = 0x06;

    fn banked() -> State {
        let mut st = State::default();
        st.apply_flags(vec![0; 8]);
        st
    }

    /// Bank A fires once per id and never sends; a second trigger of the same id is silent.
    #[test]
    fn the_trigger_fires_once_and_never_sends() {
        let now = Instant::now();
        let mut st = banked();
        st.trigger(id::TARGETING, 0, now);
        st.trigger(id::TARGETING, 0, now);
        assert_eq!(st.fired, vec![id::TARGETING + 1]);
        assert!(st.sends.is_empty());
        assert_eq!(
            bank_bit(st.fire_once.as_ref().unwrap(), id::TARGETING),
            Some(true)
        );
        assert_eq!(
            bank_bit(st.acknowledged.as_ref().unwrap(), id::TARGETING),
            Some(false),
            "firing does not acknowledge: bank B is untouched"
        );
    }

    /// No bank yet means nothing fires, which is the reference's silence before the packet.
    #[test]
    fn nothing_fires_before_the_flags_land() {
        let now = Instant::now();
        let mut st = State::default();
        st.trigger(id::WELCOME, 0, now);
        st.acknowledge(id::MOVEMENT);
        assert!(st.fired.is_empty() && st.sends.is_empty());
    }

    /// A delayed trigger waits for its timer, and acknowledging the id cancels it.
    #[test]
    fn a_delayed_trigger_waits_and_an_acknowledge_cancels_it() {
        let now = Instant::now();
        let due = now + Duration::from_millis(u64::from(TARGETING_DELAY_MS));
        let mut st = banked();
        st.trigger(id::TARGETING, TARGETING_DELAY_MS, now);
        assert!(st.fired.is_empty(), "the popup is on the timer");
        st.tick(now + Duration::from_millis(9_999));
        assert!(st.fired.is_empty(), "not yet due");
        st.tick(due);
        assert_eq!(st.fired, vec![id::TARGETING + 1]);

        let mut st = banked();
        st.trigger(id::TARGETING, TARGETING_DELAY_MS, now);
        st.acknowledge(id::MOVEMENT);
        st.tick(due);
        assert_eq!(
            st.fired,
            vec![id::TARGETING + 1],
            "another id's acknowledge keeps the timer"
        );

        let mut st = banked();
        st.trigger(id::TARGETING, TARGETING_DELAY_MS, now);
        st.acknowledge(id::TARGETING);
        st.tick(due);
        assert!(st.fired.is_empty(), "doing the thing cancelled its popup");
        assert_eq!(st.sends, vec![Send::Flag(id::TARGETING)]);
    }

    /// The acknowledge sets both banks, once per id, and owes the wire the 0-based id.
    #[test]
    fn the_acknowledge_sets_both_banks_and_sends_once() {
        let now = Instant::now();
        let mut st = banked();
        st.trigger(SOME_ID, 0, now);
        st.acknowledge(SOME_ID);
        st.acknowledge(SOME_ID);
        assert_eq!(st.sends, vec![Send::Flag(SOME_ID)]);
        assert_eq!(
            bank_bit(st.fire_once.as_ref().unwrap(), SOME_ID),
            Some(true)
        );
        assert_eq!(
            bank_bit(st.acknowledged.as_ref().unwrap(), SOME_ID),
            Some(true)
        );
    }

    /// `ClearTutorials` fills both banks and `ResetTutorials` empties them; each sends once.
    #[test]
    fn clear_and_reset_write_both_banks() {
        let mut st = banked();
        set_bank_bit(st.fire_once.as_mut().unwrap(), id::MOVEMENT);
        st.clear();
        assert_eq!(st.fire_once, Some(vec![0xFF; 8]));
        assert_eq!(st.acknowledged, Some(vec![0xFF; 8]));
        assert_eq!(st.sends, vec![Send::Clear]);
        assert!(st.timers.is_empty(), "the clear flushes the timers");

        let mut st = banked();
        st.reset();
        assert_eq!(st.fire_once, Some(vec![0; 8]));
        assert_eq!(st.acknowledged, Some(vec![0; 8]));
        assert_eq!(st.sends, vec![Send::Reset]);
    }

    /// The world-enter bring-up zeroes the banks it already has and keeps their length; the flags
    /// that land afterwards win, which is the reference's order.
    #[test]
    fn world_enter_zeroes_and_the_packet_fills() {
        let now = Instant::now();
        let mut st = banked();
        set_bank_bit(st.fire_once.as_mut().unwrap(), id::WELCOME);
        st.world_enter(now);
        assert_eq!(st.fire_once, Some(vec![0; 8]));
        assert!(st.cascade.armed, "the cascade is owed a fresh session");
        st.apply_flags(vec![0b10; 8]);
        assert_eq!(
            bank_bit(st.fire_once.as_ref().unwrap(), id::MOVEMENT),
            Some(true)
        );
    }

    /// A movement input acknowledges Movement and arms the Targeting popup once, and the silence
    /// window is spent.
    #[test]
    fn the_movement_input_arms_targeting_once() {
        let now = Instant::now();
        let mut st = banked();
        st.world_enter(now);
        for _ in 0..3 {
            st.watch_input((1.0, 0.0, 0.0), now);
        }
        assert_eq!(st.sends, vec![Send::Flag(id::MOVEMENT)]);
        assert_eq!(st.timers.len(), 1, "held keys do not stack timers");
        assert!(st.cascade.entered_at.is_none(), "the window is spent");
        st.tick(now + Duration::from_millis(u64::from(TARGETING_DELAY_MS)));
        assert_eq!(st.fired, vec![id::TARGETING + 1]);
    }

    /// An id past the bank is a no-op here, where the reference would overrun it.
    #[test]
    fn an_id_past_the_bank_is_silent() {
        let now = Instant::now();
        let mut st = State::default();
        st.apply_flags(vec![0; 1]);
        st.trigger(8, 0, now);
        assert!(st.fired.is_empty());
        st.trigger(7, 0, now);
        assert_eq!(st.fired, vec![8]);
    }
}

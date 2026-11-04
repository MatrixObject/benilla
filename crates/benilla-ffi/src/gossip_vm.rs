//! The gossip menu's VM side, ported from `crates/benilla-app/src/ui_gossip`.
//!
//! `SMSG_GOSSIP_MESSAGE` latches the menu open and owes a `CMSG_NPC_TEXT_QUERY` for its greeting,
//! `SMSG_NPC_TEXT_UPDATE` resolves it, and the stock `GossipFrame` is shown only once the greeting
//! is drawn — the reference's `0x4e2010` writes the greeting and fires `GOSSIP_SHOW` on one path,
//! and `0x4e2068` returns with neither while its text query is in flight.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use benilla_protocol::messages::{select_greeting, GossipOption, NpcTextBlock, QuestOption};
use benilla_ui::script::{GossipMenu, GossipOptionView, GossipQuestRow, UiScript};

/// The reference's icon-type table at `0x84b7ac`, verbatim; 11-13 are real `gossip` aliases and
/// `auctioneer` names art 1.12 never shipped.
const GOSSIP_ICON_TYPES: [&str; 14] = [
    "gossip",
    "vendor",
    "taxi",
    "trainer",
    "healer",
    "binder",
    "banker",
    "petition",
    "tabard",
    "battlemaster",
    "auctioneer",
    "gossip",
    "gossip",
    "gossip",
];

/// The 1-based option's icon type; an index past the table reads `"gossip"`, which is what the
/// reference's missing bounds check does.
fn gossip_icon_type(icon: u8) -> &'static str {
    GOSSIP_ICON_TYPES
        .get(icon as usize)
        .copied()
        .unwrap_or("gossip")
}

/// The open menu, plus the NPC-text records by id, which outlive it.
#[derive(Default)]
pub(crate) struct GossipState {
    npc: u64,
    text_id: u32,
    /// This open's drawn greeting; `None` while its text query is out.
    greeting: Option<String>,
    options: Vec<GossipOption>,
    quests: Vec<QuestOption>,
    records: HashMap<u32, Vec<NpcTextBlock>>,
    /// A record with no line for the NPC's gender: the reference's `0x4e216e` logs
    /// "Missing gossip text!" and runs the close core.
    missing_text: bool,
    /// The same close core reached by `SMSG_GOSSIP_COMPLETE` and by the frame's own close verb;
    /// [`feed`] fires `GOSSIP_CLOSED` once per close.
    owe_closed: bool,
    /// The menu the VM last showed, so a redraw fires nothing.
    shown: Option<GossipMenu>,
    /// The name the VM last showed beside it: the stock frame titles itself with
    /// `UnitName("npc")` (`GossipFrame.lua:33`), so the name's landing alone re-fires the show.
    shown_name: Option<String>,
}

impl GossipState {
    /// `SMSG_GOSSIP_MESSAGE` (`0x4e26e0`): latch the menu and draw its greeting from the cache;
    /// `true` when the caller owes the `CMSG_NPC_TEXT_QUERY`. A `text_id` of 0 stays pending —
    /// `DBCache::Get` never queries id 0.
    fn open(
        &mut self,
        npc: u64,
        text_id: u32,
        options: Vec<GossipOption>,
        quests: Vec<QuestOption>,
        gender: u8,
    ) -> bool {
        self.npc = npc;
        self.text_id = text_id;
        self.options = options;
        self.quests = quests;
        self.greeting = None;
        if text_id == 0 {
            return false;
        }
        if self.records.contains_key(&text_id) {
            self.resolve_greeting(gender);
            self.end_missing_text();
            false
        } else {
            true
        }
    }

    /// `SMSG_NPC_TEXT_UPDATE`: cache the record and resolve the menu waiting on it. An open menu's
    /// greeting is not re-drawn: the reference's cache callback re-enters only for the query it
    /// registered.
    fn text_arrived(&mut self, text_id: u32, blocks: Vec<NpcTextBlock>, gender: u8) {
        self.records.insert(text_id, blocks);
        if self.npc != 0 && self.text_id == text_id && self.greeting.is_none() {
            self.resolve_greeting(gender);
            self.end_missing_text();
        }
    }

    /// The missing-text path's close (`0x4e216e` → `0x4e1fa0(0)`).
    fn end_missing_text(&mut self) {
        if std::mem::take(&mut self.missing_text) {
            self.close();
        }
    }

    fn resolve_greeting(&mut self, gender: u8) {
        match draw_greeting(&self.records, self.text_id, gender) {
            Some(line) => self.greeting = Some(line),
            None => self.missing_text = true,
        }
    }

    /// The menu's close core, `0x4e1fa0` with a zero guid: the cell clears and `GOSSIP_CLOSED` is
    /// owed. The records stay — NPC text is static per id.
    fn close(&mut self) {
        if self.npc == 0 {
            return;
        }
        self.npc = 0;
        self.text_id = 0;
        self.greeting = None;
        self.options.clear();
        self.quests.clear();
        self.missing_text = false;
        self.owe_closed = true;
    }

    /// The reference's text-pending latch (`[0xbbb670]`): while it holds nothing fires and every
    /// select is refused.
    fn text_pending(&self) -> bool {
        self.npc != 0 && self.greeting.is_none()
    }

    /// The Lua-facing menu, `None` with no menu open. [`feed`] never calls it while pending.
    fn snapshot(&self) -> Option<GossipMenu> {
        if self.npc == 0 {
            return None;
        }
        Some(GossipMenu {
            greeting: self.greeting.clone()?,
            // Active or available by the wire icon: the reference's `{3,4}` test at `0x4e2430`.
            quests: self
                .quests
                .iter()
                .map(|q| GossipQuestRow {
                    title: q.title.clone(),
                    level: q.level,
                    active: matches!(q.icon, 3 | 4),
                })
                .collect(),
            options: self
                .options
                .iter()
                .map(|o| GossipOptionView {
                    label: o.message.clone(),
                    icon_type: gossip_icon_type(o.icon).into(),
                })
                .collect(),
        })
    }
}

/// Draw one record's greeting for the NPC's gender. The reference rolls random bits into the
/// mantissa of 1.0 (`(rand & 0x7fffff) | 0x3f800000`); this crate has no random source, so the
/// roll's bits come off the clock through the same mask.
fn draw_greeting(
    records: &HashMap<u32, Vec<NpcTextBlock>>,
    text_id: u32,
    gender: u8,
) -> Option<String> {
    let blocks = records.get(&text_id)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let roll = f32::from_bits((nanos & 0x7f_ffff) | 0x3f80_0000);
    select_greeting(blocks, gender, roll).map(str::to_string)
}

fn state() -> &'static Mutex<GossipState> {
    static S: OnceLock<Mutex<GossipState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(GossipState::default()))
}

fn lock() -> std::sync::MutexGuard<'static, GossipState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// The squared interaction distance (`INTERACTION_DISTANCE`, 5 yd): past it the client's
/// own close core runs, so walking away shuts the frame without a click.
const INTERACTION_RANGE_SQ: f32 = 25.0;

/// Close the open menu when the player has left the NPC's interaction range, or when the
/// NPC has left the stream (the reference answers "gone" the same way). Returns whether it
/// closed; the owed `GOSSIP_CLOSED` then fires in [`feed`] on the same tick.
pub(crate) fn close_out_of_range() -> bool {
    let npc = lock().npc;
    if npc == 0 {
        return false;
    }
    let Some(own) = crate::net::ui_player().map(|p| p.pos) else {
        return false;
    };
    let far = match crate::net::ui_unit_pose(npc) {
        None => true,
        Some(pos) => {
            let d = [pos[0] - own[0], pos[1] - own[1], pos[2] - own[2]];
            d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > INTERACTION_RANGE_SQ
        }
    };
    if far {
        lock().close();
    }
    far
}

/// Our own name is the greeting's `$N`; the reference substitutes it client-side, and this crate
/// has no macro engine, so the one macro every 1.12 greeting uses is expanded here.
fn expand_name(text: &str, name: &str) -> String {
    text.replace("$N", name)
}

/// `SMSG_GOSSIP_MESSAGE`. `gender` is the NPC's `UNIT_FIELD_BYTES_0` byte 2; a guid that is not a
/// streamed unit reads 0, which is the male column. Returns whether the caller owes the text query.
pub(crate) fn open(
    npc: u64,
    text_id: u32,
    options: Vec<GossipOption>,
    quests: Vec<QuestOption>,
    gender: u8,
) -> bool {
    lock().open(npc, text_id, options, quests, gender)
}

/// `SMSG_NPC_TEXT_UPDATE`.
pub(crate) fn text_arrived(text_id: u32, blocks: Vec<NpcTextBlock>, gender: u8) {
    lock().text_arrived(text_id, blocks, gender);
}

/// `SMSG_GOSSIP_COMPLETE` (`0x4e2800` → the core with a zero guid).
pub(crate) fn complete() {
    lock().close();
}

/// Drop the menu with the session.
pub(crate) fn clear_session() {
    let mut st = lock();
    st.npc = 0;
    st.greeting = None;
    st.options.clear();
    st.quests.clear();
}

/// The open menu's NPC, `0` when none is open.
pub(crate) fn npc() -> u64 {
    lock().npc
}

/// Whether a select is refused because the greeting's query is still in flight.
pub(crate) fn text_pending() -> bool {
    lock().text_pending()
}

/// The 1-based option row's wire record.
pub(crate) fn option_at(pos: u32) -> Option<GossipOption> {
    lock().options.get(pos.checked_sub(1)? as usize).cloned()
}

/// The 1-based quest row's wire record.
pub(crate) fn quest_at(pos: u32) -> Option<QuestOption> {
    lock().quests.get(pos.checked_sub(1)? as usize).cloned()
}

/// Push the menu into the VM and fire its events: the show on a change, the close the core owes.
/// Returns whether this tick changed the frame (opened or closed).
pub(crate) fn feed(script: &mut UiScript) -> bool {
    let mut st = lock();
    let mut changed = false;
    if st.owe_closed {
        st.owe_closed = false;
        st.shown = None;
        st.shown_name = None;
        script.set_gossip(None);
        script.fire_event("GOSSIP_CLOSED", Vec::new());
        changed = true;
    }
    // While the text query is out fire nothing and keep the last snapshot: the reference returns at
    // `0x4e2068` with no write and no event, so its frame keeps what it last showed.
    if st.text_pending() {
        return changed;
    }
    if st.npc == 0 {
        st.shown = None;
        st.shown_name = None;
        return changed;
    }
    let mut fresh = match st.snapshot() {
        Some(menu) => menu,
        // A menu with no greeting and no query out: nothing to show.
        None => return changed,
    };
    if fresh.greeting.contains("$N") {
        let names = crate::net::ui_names();
        let player = names
            .get(&crate::net::ui_self_guid())
            .cloned()
            .unwrap_or_default();
        fresh.greeting = expand_name(&fresh.greeting, &player);
    }
    let npc_name = crate::net::ui_creature_name(st.npc);
    if st.shown.as_ref() == Some(&fresh) && st.shown_name == npc_name {
        return changed;
    }
    st.shown = Some(fresh.clone());
    st.shown_name = npc_name;
    script.set_gossip(Some(fresh));
    script.fire_event("GOSSIP_SHOW", Vec::new());
    changed = true;
    changed
}

/// Drain the frame's own close verb: `CloseGossip` (`0x4e2b20`) is the core with a zero guid and
/// sends no packet. [`feed`], running after this, fires the owed `GOSSIP_CLOSED`.
pub(crate) fn drain_close(script: &mut UiScript) {
    if script.take_gossip_close() {
        lock().close();
        feed(script);
    }
}

//! Network middle layer: realmd logon + world session on a worker thread.
//!
//! The reference client's networking lives in `benilla-app` (Bevy); this
//! module carries the same `benilla-protocol` session over a plain TCP socket
//! so the UE host can log in, list characters and enter the world without
//! linking the app. Everything below is blocking socket work on its own
//! thread: the game thread only reads the shared status / event queue, which
//! keeps frame times flat.
//!
//! The UE side declares these as `extern "C"`:
//!   bn_net_connect(host, user, pass) -> 1 when the worker started
//!   bn_net_status() -> stage code (see ST_*)
//!   bn_net_char_count() / bn_net_char(i, out)
//!   bn_net_enter(guid)
//!   bn_net_send(kind, a, b, text)
//!   bn_net_event(buf, cap) -> bytes popped (0 = empty)
//!   bn_net_disconnect()

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use benilla_protocol::messages::{
    AttackSwingError, CastOutcome, Object, ObjectFields, ObjectType, ServerPacket,
};
use benilla_protocol::WorldSession;

/// Stage codes returned by `bn_net_status`.
pub const ST_IDLE: u32 = 0;
pub const ST_CONNECTING: u32 = 1;
pub const ST_LOGON_OK: u32 = 2;
pub const ST_WORLD_CONNECTING: u32 = 3;
/// Authenticated world link; the roster has been read (`bn_net_char_*`).
pub const ST_CHARS: u32 = 4;
pub const ST_ENTERING: u32 = 5;
pub const ST_IN_WORLD: u32 = 6;
/// Terminal failure; the last event line says why.
pub const ST_FAILED: u32 = 100;

/// One `SMSG_CHAR_ENUM` entry as the host renders it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetChar {
    pub guid: u64,
    /// NUL-terminated UTF-8 (the client's own roster names are ASCII).
    pub name: [c_char; 32],
    pub race: u8,
    pub class: u8,
    pub gender: u8,
    pub level: u8,
    /// `AreaTable.dbc` zone id and the map id the character logged out on.
    pub map: u32,
    pub zone: u32,
    /// `ItemDisplayInfo.dbc` ids in the enum's equipment order (head .. tabard);
    /// 0 = empty slot. This is what the dress-up pass needs to show the gear
    /// on the character-select model.
    pub equip_display: [u32; 19],
    /// `InventoryType` per slot, same order.
    pub equip_inv: [u8; 19],
}

impl Default for BnNetChar {
    fn default() -> Self {
        Self {
            guid: 0,
            name: [0; 32],
            race: 0,
            class: 0,
            gender: 0,
            level: 0,
            map: 0,
            zone: 0,
            equip_display: [0; 19],
            equip_inv: [0; 19],
        }
    }
}

/// The logged-in player's own descriptor state, rebuilt from the object stream.
///
/// Positions are raw WoW yards (X east, Y north, Z up); the host converts to
/// its own axes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetPlayer {
    pub guid: u64,
    /// The character's `SMSG_CHAR_ENUM` map id (0 Eastern Kingdoms, 1 Kalimdor,
    /// other values are instanced maps the host cannot stream).
    pub map: u32,
    pub level: u32,
    pub health: u32,
    pub max_health: u32,
    pub mana: u32,
    pub max_mana: u32,
    pub xp: u32,
    pub next_xp: u32,
    pub money: u32,
    pub target: u64,
    pub mount: u32,
    pub combo: u8,
    pub dead: u8,
    /// Number of non-empty `PLAYER_QUEST_LOG` slots, and the slots themselves
    /// (`quest_state` is `QUEST_STATE_COMPLETE`/`FAIL`, 0 in progress).
    pub quest_count: u32,
    pub quest_id: [u32; 20],
    pub quest_state: [u8; 20],
    /// Live `UNIT_FIELD_AURA` slots (buffs 0-31, debuffs 32-47), ascending
    /// slot: spell id, stack count and the descriptor slot the aura sits in.
    pub aura_count: u32,
    pub aura_slot: [u8; 48],
    pub aura_spell: [u32; 48],
    pub aura_stacks: [u8; 48],
    pub pos: [f32; 3],
    pub facing: f32,
}

impl Default for BnNetPlayer {
    fn default() -> Self {
        Self {
            guid: 0,
            map: 0,
            level: 0,
            health: 0,
            max_health: 0,
            mana: 0,
            max_mana: 0,
            xp: 0,
            next_xp: 0,
            money: 0,
            target: 0,
            mount: 0,
            combo: 0,
            dead: 0,
            quest_count: 0,
            quest_id: [0; 20],
            quest_state: [0; 20],
            aura_count: 0,
            aura_slot: [0; 48],
            aura_spell: [0; 48],
            aura_stacks: [0; 48],
            pos: [0.0; 3],
            facing: 0.0,
        }
    }
}

/// One object from the world stream, as the host may want to draw it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetUnit {
    pub guid: u64,
    /// [`ObjectType`] as a byte: 1 item, 2 container, 3 unit, 4 player,
    /// 5 gameobject, 6 dynamicobject, 7 corpse, 0 plain object.
    pub kind: u8,
    pub level: u32,
    pub display_id: i32,
    pub health: u32,
    pub max_health: u32,
    pub target: u64,
    pub mount: u32,
    pub dead: u8,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub facing: f32,
    /// Player appearance (kind 4 only; zero for creatures): `UNIT_FIELD_BYTES_0`
    /// race/gender and the `PLAYER_BYTES` skin/face/hair/color plus `PLAYER_BYTES_2`
    /// facial hair, which drive the CharSections atlas.
    pub race: u8,
    pub sex: u8,
    pub skin: u8,
    pub face: u8,
    pub hair: u8,
    pub hair_color: u8,
    pub facial: u8,
    /// Public worn equipment resolved to `ItemDisplayInfo` ids in
    /// `SMSG_CHAR_ENUM` equipment order (head..tabard); 0 = empty / unknown
    /// (`Item.dbc` maps `PLAYER_VISIBLE_ITEM_<slot>_0` entries to displays).
    pub visible_display: [u32; 19],
    /// `OBJECT_FIELD_ENTRY`: the item entry or the creature template entry.
    pub entry: u32,
    /// Creature template rank: 0 normal, 1 elite, 2 rare-elite, 3 world boss,
    /// 4 rare. Filled from the creature-query cache (0 until it answers).
    pub rank: u32,
    /// `CreatureType.dbc` id (0 until the template answers).
    pub creature_type: u32,
    /// NUL-terminated UTF-8 creature name; empty until the query answers.
    pub name: [c_char; 64],
}

impl Default for BnNetUnit {
    fn default() -> Self {
        Self {
            guid: 0,
            kind: 0,
            level: 0,
            display_id: 0,
            health: 0,
            max_health: 0,
            target: 0,
            mount: 0,
            dead: 0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            facing: 0.0,
            race: 0,
            sex: 0,
            skin: 0,
            face: 0,
            hair: 0,
            hair_color: 0,
            facial: 0,
            visible_display: [0; 19],
            entry: 0,
            rank: 0,
            creature_type: 0,
            name: [0; 64],
        }
    }
}

/// One item template, cached from `SMSG_ITEM_QUERY_SINGLE_RESPONSE` (1.12 ships
/// no Item.dbc, so the server response is the only source of names/quality).
struct ItemTemplate {
    name: String,
    display_id: u32,
    quality: u32,
    inventory_type: u32,
    item_level: u32,
    required_level: u32,
    sell_price: u32,
    buy_price: u32,
    stackable: u32,
    class: u32,
    subclass: u32,
    bag_family: u32,
    max_durability: u32,
}

/// `Bag` sentinel of an equipped-slot row: inventory slot 0..18 is the player's
/// own equipment array, the same 0xFF the client passes to `CMSG_USE_ITEM`.
pub const BAG_EQUIPPED: u8 = 0xFF;

/// One item template as the host reads it (`bn_net_item_info`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetItemInfo {
    pub entry: u32,
    pub display_id: u32,
    pub quality: u32,
    pub inventory_type: u32,
    pub item_level: u32,
    pub required_level: u32,
    pub sell_price: u32,
    pub buy_price: u32,
    pub stackable: u32,
    pub class: u32,
    pub subclass: u32,
    pub bag_family: u32,
    pub max_durability: u32,
    /// NUL-terminated UTF-8 template name; empty until the query answers.
    pub name: [c_char; 96],
}

impl Default for BnNetItemInfo {
    fn default() -> Self {
        Self {
            entry: 0,
            display_id: 0,
            quality: 0,
            inventory_type: 0,
            item_level: 0,
            required_level: 0,
            sell_price: 0,
            buy_price: 0,
            stackable: 0,
            class: 0,
            subclass: 0,
            bag_family: 0,
            max_durability: 0,
            name: [0; 96],
        }
    }
}

/// One occupied inventory slot: where it sits, which item object fills it, and
/// the template fields the host draws with.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetInvItem {
    pub guid: u64,
    pub entry: u32,
    /// [`BAG_EQUIPPED`], 0 (backpack) or an equipped-bag slot 19..22. Feed this
    /// straight back to `bn_net_send` kinds 11/12.
    pub bag: u8,
    /// Equipment index (0..18) or slot inside the bag.
    pub slot: u8,
    pub count: u32,
    pub durability: u32,
    pub max_durability: u32,
    pub info: BnNetItemInfo,
}

impl Default for BnNetInvItem {
    fn default() -> Self {
        Self {
            guid: 0,
            entry: 0,
            bag: 0,
            slot: 0,
            count: 0,
            durability: 0,
            max_durability: 0,
            info: BnNetItemInfo::default(),
        }
    }
}

/// One creature template from `SMSG_CREATURE_QUERY_RESPONSE`: the name and rank
/// the target frame needs (elite/boss styling), plus the type word.
#[derive(Clone)]
struct CreatureTemplateLite {
    name: String,
    subname: String,
    creature_type: u32,
    rank: u32,
}

/// One quest template from `SMSG_QUEST_QUERY_RESPONSE` (the quest log's detail
/// view: the descriptors only carry ids and counters).
struct QuestTemplateLite {
    level: u32,
    zone: i32,
    money: i32,
    flags: u32,
    title: String,
    objectives: String,
    details: String,
}

/// One quest template as the host reads it (`bn_net_quest_info`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetQuestInfo {
    pub quest_id: u32,
    pub level: u32,
    pub zone: i32,
    pub money: i32,
    pub flags: u32,
    /// NUL-terminated UTF-8; pipe characters are flattened to '/' on the event
    /// line, the struct itself keeps the original text.
    pub title: [c_char; 96],
    pub objectives: [c_char; 512],
    pub details: [c_char; 512],
}

impl Default for BnNetQuestInfo {
    fn default() -> Self {
        Self {
            quest_id: 0,
            level: 0,
            zone: 0,
            money: 0,
            flags: 0,
            title: [0; 96],
            objectives: [0; 512],
            details: [0; 512],
        }
    }
}

/// One occupied action-bar slot (`SMSG_ACTION_BUTTONS`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetAction {
    pub slot: u8,
    /// `ActionButtonType`: 0x00 spell, 0x40 macro, 0x80 item.
    pub kind: u8,
    pub action: u32,
}

impl Default for BnNetAction {
    fn default() -> Self {
        Self {
            slot: 0,
            kind: 0,
            action: 0,
        }
    }
}

/// One loot-window row (`SMSG_LOOT_RESPONSE` items).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetLootItem {
    pub slot: u8,
    /// `LootSlotType`: 0 lootable, 1 rolling, 2 master, 3 locked, 4 owner.
    pub slot_type: u8,
    pub item_id: u32,
    pub count: u32,
    /// `item_template.display_id`; the host resolves the icon through
    /// `ItemDisplayInfo.dbc`.
    pub display_info_id: u32,
}

impl Default for BnNetLootItem {
    fn default() -> Self {
        Self {
            slot: 0,
            slot_type: 0,
            item_id: 0,
            count: 0,
            display_info_id: 0,
        }
    }
}

/// The open loot window (0 rows / guid 0 = closed).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetLoot {
    pub guid: u64,
    pub gold: u32,
    pub count: u32,
    pub items: [BnNetLootItem; 16],
}

impl Default for BnNetLoot {
    fn default() -> Self {
        Self {
            guid: 0,
            gold: 0,
            count: 0,
            items: [BnNetLootItem::default(); 16],
        }
    }
}

/// One questgiver menu row (`SMSG_GOSSIP_MESSAGE` quests / `SMSG_QUESTGIVER_QUEST_LIST`);
/// `icon` is a `dialog_status` value (5 available, 3 incomplete, 4/6/7 reward).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetGiverQuest {
    pub quest_id: u32,
    pub icon: u32,
    pub level: u32,
    pub title: [c_char; 96],
}

impl Default for BnNetGiverQuest {
    fn default() -> Self {
        Self {
            quest_id: 0,
            icon: 0,
            level: 0,
            title: [0; 96],
        }
    }
}

/// One selectable reward on a questgiver panel (`ItemDisplayInfo` id included
/// so the host can paint the icon).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetGiverReward {
    pub item_id: u32,
    pub count: u32,
    pub display_id: u32,
}

impl Default for BnNetGiverReward {
    fn default() -> Self {
        Self {
            item_id: 0,
            count: 0,
            display_id: 0,
        }
    }
}

/// The open questgiver window: mode 0 closed, 1 quest menu, 2 accept panel,
/// 3 progress panel, 4 reward panel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetGiver {
    pub npc: u64,
    pub mode: u32,
    pub quest_id: u32,
    pub auto_finish: u32,
    /// Signed: negative when the quest costs money.
    pub money: i32,
    /// `SMSG_QUESTGIVER_REQUEST_ITEMS.is_complete`: the reward panel may be used.
    pub can_complete: u32,
    pub quest_count: u32,
    pub quests: [BnNetGiverQuest; 24],
    /// The reward choices (`QuestGiverChoices` / `QUEST_REWARD_CHOICES_COUNT` 6).
    pub choice_count: u32,
    pub choices: [BnNetGiverReward; 6],
    pub title: [c_char; 96],
    pub body: [c_char; 1024],
    pub objectives: [c_char; 512],
}

impl Default for BnNetGiver {
    fn default() -> Self {
        Self {
            npc: 0,
            mode: 0,
            quest_id: 0,
            auto_finish: 0,
            money: 0,
            can_complete: 0,
            quest_count: 0,
            quests: [BnNetGiverQuest::default(); 24],
            choice_count: 0,
            choices: [BnNetGiverReward::default(); 6],
            title: [0; 96],
            body: [0; 1024],
            objectives: [0; 512],
        }
    }
}

/// Copy `src` into a NUL-terminated fixed buffer (truncating on a char boundary).
fn put_str<const N: usize>(dst: &mut [c_char; N], src: &str) {
    let mut cut = src.len().min(N - 1);
    while cut > 0 && !src.is_char_boundary(cut) {
        cut -= 1;
    }
    for (i, b) in src.as_bytes()[..cut].iter().enumerate() {
        dst[i] = *b as c_char;
    }
}

/// An observed cast in progress (`SMSG_SPELL_START` .. `SMSG_SPELL_GO`).
struct CastState {
    spell_id: u32,
    started: Instant,
    duration_ms: u32,
    target: u64,
}

/// One active cast bar as the host draws it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetCast {
    pub caster: u64,
    pub spell_id: u32,
    pub duration_ms: u32,
    /// Time already spent, clamped to `duration_ms`.
    pub elapsed_ms: u32,
    pub target: u64,
}

impl Default for BnNetCast {
    fn default() -> Self {
        Self {
            caster: 0,
            spell_id: 0,
            duration_ms: 0,
            elapsed_ms: 0,
            target: 0,
        }
    }
}

/// One tracked object: its descriptor snapshot plus the last known position.
struct UnitRec {
    kind: u8,
    fields: ObjectFields,
    pos: [f32; 3],
    facing: f32,
}

/// Commands the game thread hands to the session thread.
enum Cmd {
    Enter(u64),
    Chat(String),
    Attack(u64),
    Target(u64),
    Loot(u64),
    /// `CMSG_AUTOSTORE_LOOT_ITEM`: take one loot-window row by slot.
    LootItem(u8),
    /// `CMSG_LOOT_MONEY`: take the corpse's coin pile.
    LootMoney,
    /// `CMSG_LOOT_RELEASE`: close the open loot window.
    LootRelease(u64),
    /// `CMSG_REPOP_REQUEST`: release the spirit after death.
    Repop,
    /// `CMSG_GOSSIP_HELLO`: open the NPC's gossip/quest menu.
    GiverHello(u64),
    /// `CMSG_QUESTGIVER_QUERY_QUEST`: the accept panel for one quest.
    QuestQueryGiver { npc: u64, quest: u32 },
    /// `CMSG_QUESTGIVER_REQUEST_REWARD`: the progress panel's Continue, opens the
    /// offer-reward panel.
    QuestRequestReward { npc: u64, quest: u32 },
    /// `CMSG_QUESTGIVER_CHOOSE_REWARD`: hand in and take reward `reward`.
    QuestReward { npc: u64, quest: u32, reward: u32 },
    GameObjUse(u64),
    QuestAccept { npc: u64, quest: u32 },
    QuestComplete { npc: u64, quest: u32 },
    QuestQuery(u32),
    /// Cast `spell` at `target` (0 = self), answered by `SMSG_CAST_RESULT`.
    Cast { spell: u32, target: u64 },
    /// `CMSG_USE_ITEM`: bag 0xFF = equipped slot; `spell` is the item spell slot.
    UseItem { bag: u8, slot: u8, spell: u8 },
    /// `CMSG_AUTOEQUIP_ITEM`: the server picks the destination equipment slot.
    AutoEquip { bag: u8, slot: u8 },
    /// `CMSG_SWAP_INV_ITEM`: two slots of the player's own equipment/pack array.
    SwapInv { src: u8, dst: u8 },
    Logout,
}

struct Shared {
    status: AtomicU32,
    /// The guid handed to `player_login`; `UpdateObject` entries for it feed
    /// `player`.
    self_guid: AtomicU64,
    events: Mutex<VecDeque<String>>,
    chars: Mutex<Vec<BnNetChar>>,
    /// Every tracked object in the stream window (the client's own object
    /// manager, reduced to what the FFI exposes).
    units: Mutex<HashMap<u64, UnitRec>>,
    player: Mutex<Option<BnNetPlayer>>,
    /// Worn-item entry -> `ItemDisplayInfo` id, filled from the server's
    /// `SMSG_ITEM_QUERY_SINGLE_RESPONSE` (1.12 ships no Item.dbc, so this
    /// template cache is the only entry->display mapping the client has).
    item_displays: Mutex<HashMap<u32, u32>>,
    /// Entries already asked for, so a stream of updates never re-queries.
    item_asked: Mutex<HashSet<u32>>,
    /// Entries seen on a player but not yet sent as a query.
    item_queue: Mutex<Vec<u32>>,
    /// Full templates from the query answers, keyed by entry.
    item_templates: Mutex<HashMap<u32, ItemTemplate>>,
    /// Quest log templates, keyed by quest id.
    quests: Mutex<HashMap<u32, QuestTemplateLite>>,
    /// Quest ids already asked for (the log re-sends descriptors constantly).
    quest_asked: Mutex<HashSet<u32>>,
    /// The spell book (learned ids, in `SMSG_INITIAL_SPELLS` order).
    spells: Mutex<Vec<u32>>,
    /// Occupied action-bar slots: (slot, kind, action).
    actions: Mutex<Vec<(u8, u8, u32)>>,
    /// Casts in flight by caster guid (self included), from SPELL_START until
    /// SPELL_GO / SPELL_FAILED_OTHER.
    casts: Mutex<HashMap<u64, CastState>>,
    /// Creature templates from `SMSG_CREATURE_QUERY_RESPONSE`, keyed by entry:
    /// name, rank (elite/boss) and creature type - the target frame's data.
    creatures: Mutex<HashMap<u32, CreatureTemplateLite>>,
    /// Creature entries already asked for.
    creature_asked: Mutex<HashSet<u32>>,
    /// The open loot window (`SMSG_LOOT_RESPONSE` .. release/error).
    loot: Mutex<BnNetLoot>,
    /// The open questgiver window (gossip menu / details / reward).
    giver: Mutex<BnNetGiver>,
}

impl Shared {
    fn set(&self, status: u32) {
        self.status.store(status, Ordering::SeqCst);
    }

    fn status(&self) -> u32 {
        self.status.load(Ordering::SeqCst)
    }

    fn event(&self, line: impl Into<String>) {
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        // The host drains this every frame; the cap only guards a host that
        // stopped polling while the server keeps talking.
        if q.len() >= 1024 {
            q.pop_front();
        }
        q.push_back(line.into());
    }

    fn drain_events(&self, cap: usize) -> String {
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        while let Some(line) = q.pop_front() {
            if out.len() + line.len() + 1 > cap {
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        status: AtomicU32::new(ST_IDLE),
        self_guid: AtomicU64::new(0),
        events: Mutex::new(VecDeque::new()),
        chars: Mutex::new(Vec::new()),
        units: Mutex::new(HashMap::new()),
        player: Mutex::new(None),
        item_displays: Mutex::new(HashMap::new()),
        item_asked: Mutex::new(HashSet::new()),
        item_queue: Mutex::new(Vec::new()),
        item_templates: Mutex::new(HashMap::new()),
        quests: Mutex::new(HashMap::new()),
        quest_asked: Mutex::new(HashSet::new()),
        spells: Mutex::new(Vec::new()),
        actions: Mutex::new(Vec::new()),
        casts: Mutex::new(HashMap::new()),
        creatures: Mutex::new(HashMap::new()),
        creature_asked: Mutex::new(HashSet::new()),
        loot: Mutex::new(BnNetLoot::default()),
        giver: Mutex::new(BnNetGiver::default()),
    })
}

fn commands() -> &'static Mutex<Option<Sender<Cmd>>> {
    static CMD: OnceLock<Mutex<Option<Sender<Cmd>>>> = OnceLock::new();
    CMD.get_or_init(|| Mutex::new(None))
}

fn send_cmd(cmd: Cmd) -> bool {
    let guard = commands().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(tx) => tx.send(cmd).is_ok(),
        None => false,
    }
}

fn copy_name<const N: usize>(dst: &mut [c_char; N], name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (i, b) in bytes[..n].iter().enumerate() {
        dst[i] = *b as c_char;
    }
    dst[n] = 0;
}

// ---- worker ----

fn worker(host: String, user: String, pass: String, rx: std::sync::mpsc::Receiver<Cmd>) {
    let sh = shared();
    sh.set(ST_CONNECTING);
    sh.event(format!("connect|{host}"));

    let mut logon = match benilla_protocol::logon(&host, &user, &pass, 0) {
        Ok(l) => l,
        Err(e) => {
            sh.event(format!("error|logon|{e}"));
            sh.set(ST_FAILED);
            return;
        }
    };
    sh.set(ST_LOGON_OK);
    sh.event(format!("logon|ok|{} realms", logon.realms.len()));

    // Pick the realm the player most likely means: the first with characters,
    // else the first listed (the client's own RealmList picks by name).
    let Some(realm) = logon
        .realms
        .iter()
        .find(|r| r.characters > 0)
        .or_else(|| logon.realms.first())
        .cloned()
    else {
        sh.event("error|realms|no realms on this account");
        sh.set(ST_FAILED);
        return;
    };
    sh.event(format!("realm|{}|{}|{} chars", realm.name, realm.address, realm.characters));

    sh.set(ST_WORLD_CONNECTING);
    let mut session = match WorldSession::connect(&realm.address, &user, logon.session_key) {
        Ok(s) => s,
        Err(e) => {
            sh.event(format!("error|world|{e}"));
            sh.set(ST_FAILED);
            return;
        }
    };
    // The held realmd socket is only needed for realm-list refreshes; free it.
    logon.close();

    let chars = match session.char_enum() {
        Ok(c) => c,
        Err(e) => {
            sh.event(format!("error|char_enum|{e}"));
            sh.set(ST_FAILED);
            return;
        }
    };
    {
        let mut out = sh.chars.lock().unwrap_or_else(|e| e.into_inner());
        out.clear();
        for c in &chars {
            let mut row = BnNetChar {
                guid: c.guid,
                race: c.race,
                class: c.class,
                gender: c.gender,
                level: c.level,
                map: c.map,
                zone: c.zone,
                ..Default::default()
            };
            copy_name(&mut row.name, &c.name);
            for (i, item) in c.equipment.iter().enumerate().take(19) {
                row.equip_display[i] = item.display_id;
                row.equip_inv[i] = item.inventory_type;
            }
            out.push(row);
        }
    }
    sh.set(ST_CHARS);
    sh.event(format!("chars|{}", chars.len()));

    // Wait for the host to choose a character. The socket stays authenticated;
    // no read deadline is set, so an idle roster does not time out.
    loop {
        match rx.recv() {
            Ok(Cmd::Enter(guid)) => {
                sh.self_guid.store(guid, Ordering::SeqCst);
                sh.units
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                *sh.player.lock().unwrap_or_else(|e| e.into_inner()) = None;
                sh.set(ST_ENTERING);
                sh.event(format!("enter|{guid}"));
                match session.player_login(guid) {
                    Ok(()) => {
                        sh.set(ST_IN_WORLD);
                        sh.event("world|entered");
                        run_world(&mut session, &rx, sh);
                    }
                    Err(e) => {
                        sh.event(format!("error|player_login|{e}"));
                        sh.set(ST_FAILED);
                    }
                }
                return;
            }
            Ok(Cmd::Logout) | Err(_) => {
                sh.event("closed|roster");
                sh.set(ST_IDLE);
                return;
            }
            Ok(_) => {}   // world commands before entering are meaningless
        }
    }
}

/// Pump the world stream: one `recv` with a short deadline per loop so queued
/// commands (chat, attack, loot) go out promptly even when the server is quiet.
fn run_world(session: &mut WorldSession, rx: &std::sync::mpsc::Receiver<Cmd>, sh: &Shared) {
    let _ = session.set_read_timeout(Some(std::time::Duration::from_millis(200)));
    loop {
        // Commands first: sending is cheap and must not wait a server packet.
        loop {
            match rx.try_recv() {
                Ok(Cmd::Chat(msg)) => {
                    if let Err(e) = session.send_chat(&msg) {
                        sh.event(format!("error|chat|{e}"));
                    }
                }
                Ok(Cmd::Attack(guid)) => {
                    let _ = session.attack_swing(guid);
                }
                Ok(Cmd::Target(guid)) => {
                    let _ = session.set_selection(guid);
                }
                Ok(Cmd::Loot(guid)) => {
                    let _ = session.loot(guid);
                }
                Ok(Cmd::LootItem(slot)) => {
                    let _ = session.autostore_loot_item(slot);
                }
                Ok(Cmd::LootMoney) => {
                    let _ = session.loot_money();
                }
                Ok(Cmd::LootRelease(guid)) => {
                    let _ = session.loot_release(guid);
                }
                Ok(Cmd::Repop) => {
                    if let Err(e) = session.repop_request() {
                        sh.event(format!("error|repop|{e}"));
                    }
                }
                Ok(Cmd::GiverHello(npc)) => {
                    let _ = session.questgiver_hello(npc);
                }
                Ok(Cmd::QuestQueryGiver { npc, quest }) => {
                    let _ = session.questgiver_query_quest(npc, quest);
                }
                Ok(Cmd::QuestRequestReward { npc, quest }) => {
                    let _ = session.questgiver_request_reward(npc, quest);
                }
                Ok(Cmd::QuestReward { npc, quest, reward }) => {
                    let _ = session.questgiver_choose_reward(npc, quest, reward);
                }
                Ok(Cmd::GameObjUse(guid)) => {
                    let _ = session.gameobj_use(guid);
                }
                Ok(Cmd::QuestAccept { npc, quest }) => {
                    let _ = session.questgiver_accept_quest(npc, quest);
                }
                Ok(Cmd::QuestComplete { npc, quest }) => {
                    let _ = session.questgiver_complete_quest(npc, quest);
                }
                Ok(Cmd::QuestQuery(quest)) => {
                    let _ = session.quest_query(quest);
                }
                Ok(Cmd::Cast { spell, target }) => {
                    let dest = (target != 0).then_some(target);
                    match session.cast_spell(spell, dest) {
                        Ok(()) => sh.event(format!(
                            "cast|{spell}|{}",
                            if target != 0 { target } else { 0 }
                        )),
                        Err(e) => sh.event(format!("error|cast|{e}")),
                    }
                }
                Ok(Cmd::UseItem { bag, slot, spell }) => {
                    if let Err(e) = session.use_item(bag, slot, spell) {
                        sh.event(format!("error|use_item|{e}"));
                    }
                }
                Ok(Cmd::AutoEquip { bag, slot }) => {
                    if let Err(e) = session.auto_equip_item(bag, slot) {
                        sh.event(format!("error|equip|{e}"));
                    }
                }
                Ok(Cmd::SwapInv { src, dst }) => {
                    if let Err(e) = session.swap_inv_item(src, dst) {
                        sh.event(format!("error|swap|{e}"));
                    }
                }
                Ok(Cmd::Logout) | Err(TryRecvError::Disconnected) => {
                    let _ = session.logout(std::time::Duration::from_secs(2));
                    sh.event("closed|world");
                    sh.set(ST_IDLE);
                    return;
                }
                Ok(Cmd::Enter(_)) => {}
                Err(TryRecvError::Empty) => break,
            }
        }

        match session.recv() {
            Ok(packet) => {
                handle_packet(&packet, sh);
                flush_item_queries(session, sh);
                queue_self_quests(session, sh);
                queue_creature_names(session, sh);
            }
            Err(e) => {
                // A read deadline is the normal quiet path; anything else ends
                // the session (the server dropped us, or the link broke).
                let quiet = e
                    .downcast_ref::<std::io::Error>()
                    .map(|io| {
                        matches!(
                            io.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        )
                    })
                    .unwrap_or(false);
                if quiet {
                    continue;
                }
                sh.event(format!("error|recv|{e}"));
                sh.set(ST_FAILED);
                return;
            }
        }
    }
}

/// [`ObjectType`] as the byte the host sees in [`BnNetUnit::kind`].
fn object_kind(t: ObjectType) -> u8 {
    match t {
        ObjectType::Object => 0,
        ObjectType::Item => 1,
        ObjectType::Container => 2,
        ObjectType::Unit => 3,
        ObjectType::Player => 4,
        ObjectType::GameObject => 5,
        ObjectType::DynamicObject => 6,
        ObjectType::Corpse => 7,
    }
}

/// Cap on tracked objects: the stream window is a few hundred at most, so this
/// only stops a pathological server from growing the map forever.
const MAX_UNITS: usize = 4096;

/// Queue `CMSG_ITEM_QUERY_SINGLE` for every worn item entry that has not been
/// asked for yet. 1.12 has no Item.dbc, so the display id only arrives in the
/// server's template response; the query is ask-once per entry.
fn queue_visible_items(f: &ObjectFields, sh: &Shared) {
    let mut asked = sh.item_asked.lock().unwrap_or_else(|e| e.into_inner());
    let mut queue = sh.item_queue.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..19u8 {
        let entry = f.player_visible_item_entry(i).unwrap_or(0);
        if entry != 0 && asked.insert(entry) {
            queue.push(entry);
        }
    }
}

/// Send the queued item queries (one per player appearance change at most).
fn flush_item_queries(session: &mut WorldSession, sh: &Shared) {
    let pending: Vec<u32> = {
        let mut queue = sh.item_queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    };
    for entry in pending {
        let _ = session.item_query(entry, 0);
    }
}

/// Ask for the template of every item object in the player's own equipment,
/// backpack and equipped bags. The item guid -> entry lookup is the object
/// table; the caller already holds `units`.
fn queue_self_items(f: &ObjectFields, units: &HashMap<u64, UnitRec>, sh: &Shared) {
    let mut asked = sh.item_asked.lock().unwrap_or_else(|e| e.into_inner());
    let mut queue = sh.item_queue.lock().unwrap_or_else(|e| e.into_inner());
    let mut ask = |guid: u64| {
        let Some(rec) = units.get(&guid) else {
            return;
        };
        let entry = rec.fields.object_entry().unwrap_or(0);
        if entry != 0 && asked.insert(entry) {
            queue.push(entry);
        }
    };
    for i in 0..23u8 {
        let Some(guid) = f.player_inv_slot(i) else {
            continue;
        };
        if guid == 0 {
            continue;
        }
        if i >= 19 {
            // Equipped bag: the bag object carries its contents in
            // `CONTAINER_FIELD_SLOT_1`, which the server streams in the update
            // block rather than in the player's pack slots.
            ask(guid);
            if let Some(bag) = units.get(&guid) {
                let n = bag.fields.container_num_slots().unwrap_or(0).min(36);
                for s in 0..n {
                    if let Some(item) = bag.fields.container_slot(s as u8) {
                        if item != 0 {
                            ask(item);
                        }
                    }
                }
            }
        } else {
            ask(guid);
        }
    }
    for i in 0..16u8 {
        if let Some(guid) = f.player_pack_slot(i) {
            if guid != 0 {
                ask(guid);
            }
        }
    }
}

/// Ask the server for a template of every quest in the log we have not seen
/// yet, so titles/objectives exist without opening the quest log.
fn queue_self_quests(session: &mut WorldSession, sh: &Shared) {
    let ids: Vec<u32> = {
        let player = sh.player.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = player.as_ref() else {
            return;
        };
        p.quest_id[..(p.quest_count as usize).min(20)].to_vec()
    };
    if ids.is_empty() {
        return;
    }
    let mut asked = sh.quest_asked.lock().unwrap_or_else(|e| e.into_inner());
    for id in ids {
        if id != 0 && asked.insert(id) {
            let _ = session.quest_query(id);
        }
    }
}

/// Ask for the creature templates seen in the stream window: the target frame
/// needs the name and rank (elite/boss). Two per packet keeps the burst small
/// while a town full of NPCs streams in.
fn queue_creature_names(session: &mut WorldSession, sh: &Shared) {
    let mut asked: Vec<(u32, u64)> = Vec::new();
    {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = sh
            .creature_asked
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cache = sh.creatures.lock().unwrap_or_else(|e| e.into_inner());
        for (guid, rec) in units.iter() {
            if rec.kind != 3 {
                continue;
            }
            let entry = rec
                .fields
                .raw_fields()
                .find(|(index, _)| *index == 3)
                .map(|(_, value)| value)
                .unwrap_or(0);
            if entry == 0 || cache.contains_key(&entry) || !seen.insert(entry) {
                continue;
            }
            asked.push((entry, *guid));
            if asked.len() >= 2 {
                break;
            }
        }
    }
    for (entry, guid) in asked {
        let _ = session.creature_query(entry, guid);
    }
}

/// Build the item info row for one entry (empty when the query has not
/// answered yet).
fn item_info_row(entry: u32, templates: &HashMap<u32, ItemTemplate>) -> BnNetItemInfo {
    let mut row = BnNetItemInfo {
        entry,
        ..Default::default()
    };
    if let Some(t) = templates.get(&entry) {
        row.display_id = t.display_id;
        row.quality = t.quality;
        row.inventory_type = t.inventory_type;
        row.item_level = t.item_level;
        row.required_level = t.required_level;
        row.sell_price = t.sell_price;
        row.buy_price = t.buy_price;
        row.stackable = t.stackable;
        row.class = t.class;
        row.subclass = t.subclass;
        row.bag_family = t.bag_family;
        row.max_durability = t.max_durability;
        copy_name(&mut row.name, &t.name);
    }
    row
}

/// Every occupied slot of the player's own inventory, in client order:
/// equipment 0..18, backpack, then the four equipped bags.
fn inventory_rows(sh: &Shared) -> Vec<BnNetInvItem> {
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let templates = sh
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<BnNetInvItem> = Vec::new();
    if self_guid == 0 {
        return out;
    }
    let Some(player) = units.get(&self_guid) else {
        return out;
    };
    let f = &player.fields;

    let mut push = |bag: u8, slot: u8, guid: u64| {
        let Some(rec) = units.get(&guid) else {
            return;
        };
        let entry = rec.fields.object_entry().unwrap_or(0);
        if entry == 0 {
            return;
        }
        let mut row = BnNetInvItem {
            guid,
            entry,
            bag,
            slot,
            count: rec.fields.item_stack_count().unwrap_or(1),
            durability: rec.fields.item_durability().unwrap_or(0),
            max_durability: rec.fields.item_max_durability().unwrap_or(0),
            ..Default::default()
        };
        row.max_durability = row
            .max_durability
            .max(templates.get(&entry).map(|t| t.max_durability).unwrap_or(0));
        row.info = item_info_row(entry, &templates);
        out.push(row);
    };

    for i in 0..19u8 {
        if let Some(guid) = f.player_inv_slot(i) {
            if guid != 0 {
                push(BAG_EQUIPPED, i, guid);
            }
        }
    }
    for i in 0..16u8 {
        if let Some(guid) = f.player_pack_slot(i) {
            if guid != 0 {
                push(0, i, guid);
            }
        }
    }
    for bag_slot in 19..23u8 {
        let Some(bag_guid) = f.player_inv_slot(bag_slot) else {
            continue;
        };
        if bag_guid == 0 {
            continue;
        }
        let Some(bag) = units.get(&bag_guid) else {
            continue;
        };
        let n = bag.fields.container_num_slots().unwrap_or(0).min(36);
        for s in 0..n {
            if let Some(item) = bag.fields.container_slot(s as u8) {
                if item != 0 {
                    push(bag_slot, s as u8, item);
                }
            }
        }
    }
    out
}

/// Fold one `SMSG_UPDATE_OBJECT` entry into the tracked object table.
fn apply_object(obj: &Object, sh: &Shared) {
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let mut units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    match obj {
        Object::Values { guid, mask } => {
            if let Some(rec) = units.get_mut(guid) {
                rec.fields.merge(mask.clone());
                if rec.kind == 4 {
                    queue_visible_items(mask, sh);
                }
            } else if units.len() < MAX_UNITS {
                units.insert(
                    *guid,
                    UnitRec {
                        kind: if *guid == self_guid { 4 } else { 0 },
                        fields: mask.clone(),
                        pos: [0.0; 3],
                        facing: 0.0,
                    },
                );
            }
        }
        Object::Movement { guid, movement } => {
            if let Some(rec) = units.get_mut(guid) {
                if let Some((v, o)) = movement.position {
                    rec.pos = [v.x, v.y, v.z];
                    rec.facing = o;
                }
            }
        }
        Object::Create {
            guid,
            object_type,
            movement,
            mask,
        } => {
            let (pos, facing) = match movement.position {
                Some((v, o)) => ([v.x, v.y, v.z], o),
                None => ([0.0; 3], 0.0),
            };
            units.insert(
                *guid,
                UnitRec {
                    kind: object_kind(*object_type),
                    fields: mask.clone(),
                    pos,
                    facing,
                },
            );
            if *object_type == ObjectType::Player {
                queue_visible_items(mask, sh);
            }
        }
        Object::OutOfRange { guids } => {
            for g in guids {
                units.remove(g);
            }
        }
        // Proximity hints only affect the client's update cadence.
        Object::Near { .. } => {}
    }
}

/// Rebuild the host's player snapshot after its descriptor changed.
fn refresh_player(sh: &Shared) {
    let guid = sh.self_guid.load(Ordering::SeqCst);
    if guid == 0 {
        return;
    }
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rec) = units.get(&guid) else {
        return;
    };
    let f = &rec.fields;
    let power_type = f.unit_power_type();
    // The map id comes from the roster row (the world stream carries no map
    // field); a manual `player_login` without a roster match stays 0.
    let map = sh
        .chars
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|c| c.guid == guid)
        .map(|c| c.map)
        .unwrap_or(0);
    let mut p = BnNetPlayer {
        guid,
        map,
        level: f.unit_level().unwrap_or(0),
        health: f.unit_health().unwrap_or(0),
        max_health: f.unit_max_health().unwrap_or(0),
        mana: f.unit_power(power_type).unwrap_or(0),
        max_mana: f.unit_max_power(power_type).unwrap_or(0),
        xp: f.player_xp().unwrap_or(0),
        next_xp: f.player_next_level_xp().unwrap_or(0),
        money: f.player_money().unwrap_or(0),
        target: f.unit_target().unwrap_or(0),
        mount: f.unit_mount_display_id(),
        combo: f.player_combo_points().unwrap_or(0),
        dead: f.unit_is_dead() as u8,
        pos: rec.pos,
        facing: rec.facing,
        ..Default::default()
    };
    let mut slot = 0usize;
    for i in 0..20u8 {
        let Some(q) = f.player_quest_log(i) else {
            continue;
        };
        if q.quest_id == 0 {
            continue;
        }
        p.quest_id[slot] = q.quest_id;
        p.quest_state[slot] = q.state;
        slot += 1;
    }
    p.quest_count = slot as u32;
    // Live buffs/debuffs, straight off the descriptor block: the host draws the
    // buff bar from these, and the spell ids resolve to icons through Spell.dbc.
    let mut auras = 0usize;
    for a in f.unit_auras() {
        if auras >= p.aura_slot.len() {
            break;
        }
        p.aura_slot[auras] = a.slot;
        p.aura_spell[auras] = a.spell_id;
        p.aura_stacks[auras] = a.stacks;
        auras += 1;
    }
    p.aura_count = auras as u32;
    // The item objects behind the pack/bag guids may have just arrived (or
    // changed); make sure their templates are asked for.
    queue_self_items(f, &units, sh);
    drop(units);
    *sh.player.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
}

fn handle_packet(packet: &ServerPacket, sh: &Shared) {
    match packet {
        ServerPacket::MessageChat(msg) => {
            // chat|<type>|<sender>|<text>: the host feeds its chat frame. Types
            // follow the client's ChatType ids (0x00 say, 0x0A system, ...).
            let who = msg
                .sender_name
                .clone()
                .unwrap_or_else(|| msg.sender_guid.to_string());
            sh.event(format!("chat|{}|{}|{}", msg.chat_type, who, msg.text));
        }
        // The world stream is very chatty; object updates are the bulk, so they
        // are folded into the tracked table rather than echoed as events.
        ServerPacket::UpdateObject { objects } => {
            for obj in objects {
                apply_object(obj, sh);
            }
            refresh_player(sh);
        }
        ServerPacket::DestroyObject { guid } => {
            sh.units
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(guid);
            refresh_player(sh);
        }
        ServerPacket::ItemQueryResponse { entry, info } => {
            if let Some(info) = info {
                sh.item_displays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(*entry, info.display_info_id);
                sh.item_templates
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        *entry,
                        ItemTemplate {
                            name: info.name.clone(),
                            display_id: info.display_info_id,
                            quality: info.quality,
                            inventory_type: info.inventory_type,
                            item_level: info.item_level,
                            required_level: info.required_level,
                            sell_price: info.sell_price,
                            buy_price: info.buy_price,
                            stackable: info.stackable,
                            class: info.class,
                            subclass: info.subclass,
                            bag_family: info.bag_family,
                            max_durability: info.max_durability,
                        },
                    );
            }
        }
        // Creature templates: the target frame's name, rank (elite/boss) and
        // creature type come from here; 1.12 answers with `info = None` for an
        // unknown entry, cached as an empty row so it is not re-asked.
        ServerPacket::CreatureQueryResponse { entry, info } => {
            let row = match info {
                Some(i) => CreatureTemplateLite {
                    name: i.name.clone(),
                    subname: i.subname.clone(),
                    creature_type: i.creature_type,
                    rank: i.rank,
                },
                None => CreatureTemplateLite {
                    name: String::new(),
                    subname: String::new(),
                    creature_type: 0,
                    rank: 0,
                },
            };
            sh.creatures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*entry, row);
        }
        // The "You receive …" line; `bag`/`slot` locate the new row so the host
        // can refresh an inventory panel.
        ServerPacket::ItemPushResult(p) => {
            sh.event(format!(
                "item|push|{}|{}|{}|{}|{}|{}",
                p.item_entry,
                p.count,
                p.bag_slot,
                p.item_slot,
                u8::from(p.from_npc),
                u8::from(p.created)
            ));
        }
        // A refused equip/use/swap; the host shows the client's own message.
        ServerPacket::InventoryChangeFailure {
            reason,
            required_level,
            item_guid,
            ..
        } => {
            sh.event(format!(
                "inv|fail|{}|{}|{}",
                reason,
                required_level.unwrap_or(0),
                item_guid
            ));
        }
        // ---- combat feedback: the numbers the reference floats over units ----
        ServerPacket::AttackerState(s) => {
            // `displayed()` is the reference gate (0x625e40): a dodged or
            // parried on-next-swing ability gets neither log line nor number.
            if s.displayed() {
                sh.event(format!(
                    "dmg|melee|{}|{}|{}|{}|{}|{}|{}",
                    s.victim, s.attacker, s.damage, s.victim_state, s.hit_info, s.absorb, s.resist
                ));
            }
        }
        ServerPacket::SpellDamageLog(s) => {
            sh.event(format!(
                "dmg|spell|{}|{}|{}|{}|{}|{}|{}|{}",
                s.target,
                s.attacker,
                s.damage,
                s.school,
                u8::from(s.hit_info & 0x2 != 0), // crit bit, SpellDefines.h:179
                s.absorb,
                s.resist,
                u8::from(s.periodic)
            ));
        }
        ServerPacket::SpellHealLog(s) => {
            sh.event(format!(
                "heal|{}|{}|{}|{}",
                s.target,
                s.healer,
                s.amount,
                u8::from(s.critical)
            ));
        }
        ServerPacket::SpellEnergizeLog(s) => {
            sh.event(format!("power|{}|{}|{}", s.target, s.power, s.amount));
        }
        ServerPacket::AttackSwingError(e) => {
            let code = match e {
                AttackSwingError::NotInRange => 1,
                AttackSwingError::BadFacing => 2,
                AttackSwingError::DeadOrUnattackable => 3,
            };
            sh.event(format!("attack|error|{code}"));
        }
        ServerPacket::XpGain(x) => {
            sh.event(format!(
                "xp|{}|{}|{}",
                x.total,
                x.base,
                u8::from(x.kill)
            ));
        }
        ServerPacket::LevelUp(info) => {
            sh.event(format!(
                "levelup|{}|{}|{}",
                info.level, info.health, info.powers[0]
            ));
        }
        // The quest log detail template; the log itself only carries ids, so
        // this is the only source of titles/objectives.
        ServerPacket::QuestQueryResponse(t) => {
            sh.quests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    t.quest_id,
                    QuestTemplateLite {
                        level: t.level,
                        zone: t.zone_or_sort,
                        money: t.money,
                        flags: t.flags,
                        title: t.title.clone(),
                        objectives: t.objectives_text.clone(),
                        details: t.details.clone(),
                    },
                );
            // Pipes would break the host's field split; quest titles have none
            // in practice, but flatten defensively.
            sh.event(format!(
                "quest|info|{}|{}|{}",
                t.quest_id,
                t.level,
                t.title.replace('|', "/")
            ));
        }
        // The spell book: learned ids in server order; rank-ups supersede the
        // old id in place.
        ServerPacket::InitialSpells { spell_ids, .. } => {
            let mut book = sh.spells.lock().unwrap_or_else(|e| e.into_inner());
            book.clear();
            for id in spell_ids {
                let id = *id as u32;
                if !book.contains(&id) {
                    book.push(id);
                }
            }
        }
        ServerPacket::LearnedSpell { spell_id } => {
            let mut book = sh.spells.lock().unwrap_or_else(|e| e.into_inner());
            let id = *spell_id as u32;
            if !book.contains(&id) {
                book.push(id);
            }
        }
        ServerPacket::RemovedSpell { spell_id } => {
            let id = *spell_id as u32;
            sh.spells
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|s| *s != id);
        }
        ServerPacket::SupercededSpell {
            old_spell_id,
            new_spell_id,
        } => {
            let old = *old_spell_id as u32;
            let new = *new_spell_id as u32;
            let mut book = sh.spells.lock().unwrap_or_else(|e| e.into_inner());
            book.retain(|s| *s != old);
            if !book.contains(&new) {
                book.push(new);
            }
        }
        ServerPacket::ActionButtons { buttons } => {
            let mut slots: Vec<(u8, u8, u32)> =
                buttons.iter().map(|b| (b.slot, b.kind, b.action)).collect();
            slots.sort_by_key(|s| s.0);
            *sh.actions.lock().unwrap_or_else(|e| e.into_inner()) = slots;
        }
        // The loot window: RESPONSE opens it (gold + rows), RELEASE/ERROR close
        // it, REMOVED drops a taken row. Names for the rows come from the same
        // item-template cache as the bag panel, so the entries are queued here.
        ServerPacket::LootResponse {
            guid,
            gold,
            items,
            ..
        } => {
            let mut window = BnNetLoot {
                guid: *guid,
                gold: *gold,
                ..Default::default()
            };
            {
                let mut asked = sh.item_asked.lock().unwrap_or_else(|e| e.into_inner());
                let mut queue = sh.item_queue.lock().unwrap_or_else(|e| e.into_inner());
                for it in items.iter().take(window.items.len()) {
                    window.items[window.count as usize] = BnNetLootItem {
                        slot: it.slot,
                        slot_type: it.slot_type,
                        item_id: it.item_id,
                        count: it.count,
                        display_info_id: it.display_info_id,
                    };
                    window.count += 1;
                    if it.item_id != 0 && asked.insert(it.item_id) {
                        queue.push(it.item_id);
                    }
                }
            }
            let count = window.count;
            *sh.loot.lock().unwrap_or_else(|e| e.into_inner()) = window;
            sh.event(format!("loot|open|{}|{}|{}", guid, gold, count));
        }
        ServerPacket::LootError { guid, error } => {
            *sh.loot.lock().unwrap_or_else(|e| e.into_inner()) = BnNetLoot::default();
            sh.event(format!("loot|error|{}|{}", guid, error));
        }
        ServerPacket::LootReleaseResponse { .. } => {
            *sh.loot.lock().unwrap_or_else(|e| e.into_inner()) = BnNetLoot::default();
            sh.event("loot|close".to_string());
        }
        ServerPacket::LootRemoved { slot } => {
            let mut window = sh.loot.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = BnNetLoot {
                guid: window.guid,
                gold: window.gold,
                ..Default::default()
            };
            for i in 0..window.count as usize {
                if window.items[i].slot != *slot {
                    out.items[out.count as usize] = window.items[i];
                    out.count += 1;
                }
            }
            *window = out;
        }
        ServerPacket::LootMoneyNotify { amount } => {
            sh.event(format!("loot|money|{}", amount));
        }
        ServerPacket::LootClearMoney => {
            sh.loot.lock().unwrap_or_else(|e| e.into_inner()).gold = 0;
        }
        // Questgiver window. Gossip and the quest-list packet both open the
        // menu; details/reward/progress replace it with a single quest, and a
        // completed hand-in just closes it.
        ServerPacket::GossipMessage { npc, quests, .. } => {
            let mut giver = BnNetGiver {
                npc: *npc,
                mode: 1,
                ..Default::default()
            };
            for (i, q) in quests.iter().take(giver.quests.len()).enumerate() {
                giver.quests[i] = BnNetGiverQuest {
                    quest_id: q.quest_id,
                    icon: q.icon,
                    level: q.level,
                    title: [0; 96],
                };
                put_str(&mut giver.quests[i].title, &q.title);
            }
            giver.quest_count = quests.len().min(giver.quests.len()) as u32;
            let count = giver.quest_count;
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = giver;
            sh.event(format!("giver|open|{}|{}", npc, count));
        }
        ServerPacket::GossipComplete => {
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = BnNetGiver::default();
            sh.event("giver|close".to_string());
        }
        ServerPacket::QuestGiverQuestList(list) => {
            let mut giver = BnNetGiver {
                npc: list.npc,
                mode: 1,
                ..Default::default()
            };
            put_str(&mut giver.body, &list.greeting);
            for (i, q) in list.quests.iter().take(giver.quests.len()).enumerate() {
                giver.quests[i] = BnNetGiverQuest {
                    quest_id: q.quest_id,
                    icon: q.icon,
                    level: q.level,
                    title: [0; 96],
                };
                put_str(&mut giver.quests[i].title, &q.title);
            }
            giver.quest_count = list.quests.len().min(giver.quests.len()) as u32;
            let count = giver.quest_count;
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = giver;
            sh.event(format!("giver|open|{}|{}", list.npc, count));
        }
        ServerPacket::QuestGiverDetails(d) => {
            let mut giver = BnNetGiver {
                npc: d.npc,
                mode: 2,
                quest_id: d.quest_id,
                auto_finish: d.auto_finish,
                money: d.money,
                ..Default::default()
            };
            put_str(&mut giver.title, &d.title);
            put_str(&mut giver.body, &d.details);
            put_str(&mut giver.objectives, &d.objectives);
            giver.choice_count = d.choices.len().min(giver.choices.len()) as u32;
            for (i, c) in d.choices.iter().take(giver.choices.len()).enumerate() {
                giver.choices[i] = BnNetGiverReward {
                    item_id: c.item_id,
                    count: c.count,
                    display_id: c.display_id,
                };
            }
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = giver;
            sh.event(format!("giver|details|{}", d.quest_id));
        }
        ServerPacket::QuestGiverOfferReward(r) => {
            let mut giver = BnNetGiver {
                npc: r.npc,
                mode: 4,
                quest_id: r.quest_id,
                auto_finish: r.auto_finish,
                money: r.money,
                can_complete: 1,
                ..Default::default()
            };
            put_str(&mut giver.title, &r.title);
            put_str(&mut giver.body, &r.offer_text);
            giver.choice_count = r.choices.len().min(giver.choices.len()) as u32;
            for (i, c) in r.choices.iter().take(giver.choices.len()).enumerate() {
                giver.choices[i] = BnNetGiverReward {
                    item_id: c.item_id,
                    count: c.count,
                    display_id: c.display_id,
                };
            }
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = giver;
            sh.event(format!("giver|reward|{}", r.quest_id));
        }
        ServerPacket::QuestGiverRequestItems(r) => {
            let mut giver = BnNetGiver {
                npc: r.npc,
                mode: 3,
                quest_id: r.quest_id,
                money: r.required_money as i32,
                can_complete: u32::from(r.is_complete),
                ..Default::default()
            };
            put_str(&mut giver.title, &r.title);
            put_str(&mut giver.body, &r.request_text);
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = giver;
            sh.event(format!("giver|progress|{}", r.quest_id));
        }
        ServerPacket::QuestGiverComplete(c) => {
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = BnNetGiver::default();
            sh.event(format!("giver|complete|{}|{}|{}", c.quest_id, c.xp, c.money));
        }
        ServerPacket::QuestGiverInvalid { .. } => {
            sh.event("giver|invalid".to_string());
        }
        ServerPacket::QuestGiverFailed { .. } => {
            sh.event("giver|failed".to_string());
        }
        // The server's verdict on our own cast: failures are reported to the
        // host as `castfail|spell|reason|arg` (reason = SpellCastResult), while
        // a success needs no event (the SpellStart/SpellGo pair follows).
        ServerPacket::CastResult { spell_id, outcome } => match outcome {
            CastOutcome::Ok => {}
            CastOutcome::Failed { reason, arg } => sh.event(format!(
                "castfail|{}|{}|{}",
                spell_id,
                reason,
                arg.unwrap_or(0)
            )),
        },
        // Cast bars: START opens one, GO / FAILED closes it; pushback moves the
        // start later, which is exactly how the client shrinks the fill.
        ServerPacket::SpellStart(s) => {
            if s.caster != 0 && s.cast_time_ms > 0 {
                sh.casts.lock().unwrap_or_else(|e| e.into_inner()).insert(
                    s.caster,
                    CastState {
                        spell_id: s.spell_id,
                        started: Instant::now(),
                        duration_ms: s.cast_time_ms,
                        target: s.targets.unit_target.unwrap_or(0),
                    },
                );
            }
        }
        ServerPacket::SpellGo(g) => {
            sh.casts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&g.caster);
            // A completed own cast starts the spell's recovery timer. The
            // duration is a Spell.dbc column (recovery_ms / category recovery),
            // so `0` tells the host to resolve it from the catalogue.
            if g.caster == sh.self_guid.load(Ordering::SeqCst) {
                sh.event(format!("cooldown|{}|0", g.spell_id));
            }
        }
        // Server-pushed cooldowns (school lockout, pet, item procs, GM reset)
        // carry explicit durations; 0 ms falls back to the DBC recovery.
        ServerPacket::SpellCooldownList { caster, cooldowns } => {
            if *caster == 0 || *caster == sh.self_guid.load(Ordering::SeqCst) {
                for (spell_id, ms) in cooldowns {
                    sh.event(format!("cooldown|{}|{}", spell_id, ms));
                }
            }
        }
        ServerPacket::CooldownEvent { spell_id, caster } => {
            if *caster == sh.self_guid.load(Ordering::SeqCst) {
                sh.event(format!("cooldown|{}|0", spell_id));
            }
        }
        ServerPacket::ClearCooldown { spell_id, caster } => {
            if *caster == sh.self_guid.load(Ordering::SeqCst) {
                sh.event(format!("cooldownclear|{}", spell_id));
            }
        }
        ServerPacket::CooldownCheat { caster } => {
            if *caster == sh.self_guid.load(Ordering::SeqCst) {
                sh.event("cooldownclear|0".to_string());
            }
        }
        ServerPacket::SpellFailedOther { caster, .. } => {
            sh.casts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(caster);
        }
        ServerPacket::SpellDelayed { caster, delay_ms } => {
            let mut casts = sh.casts.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = casts.get_mut(caster) {
                c.started += Duration::from_millis(*delay_ms as u64);
            }
        }
        ServerPacket::LogoutComplete
        | ServerPacket::UpdateWorldState { .. }
        | ServerPacket::QuestUpdateComplete { .. }
        | ServerPacket::QuestUpdateAddKill { .. }
        | ServerPacket::QuestUpdateAddItem { .. }
        | ServerPacket::QuestLogFull => {
            sh.event(format!("pkt|{}", packet.name()));
        }
        _ => {}
    }
}

// ---- C ABI ----

fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().ok().map(|s| s.to_owned())
}

/// Start a login on a worker thread. `host` is the realmd address ("host" or
/// "host:port"), `user`/`pass` the account. Returns 1 when the worker started.
#[no_mangle]
pub extern "C" fn bn_net_connect(
    host: *const c_char,
    user: *const c_char,
    pass: *const c_char,
) -> u32 {
    let (Some(host), Some(user), Some(pass)) = (cstr(host), cstr(user), cstr(pass)) else {
        return 0;
    };
    {
        let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return 0;   // a session is already running
        }
        let (tx, rx) = channel();
        *guard = Some(tx);
        std::thread::Builder::new()
            .name("bn-net".into())
            .spawn(move || {
                worker(host, user, pass, rx);
                let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
                *guard = None;
            })
            .ok();
    }
    1
}

#[no_mangle]
pub extern "C" fn bn_net_status() -> u32 {
    shared().status()
}

#[no_mangle]
pub extern "C" fn bn_net_char_count() -> u32 {
    shared()
        .chars
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u32
}

/// Fill `out` with the i-th roster entry. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_net_char(index: u32, out: *mut BnNetChar) -> u32 {
    if out.is_null() {
        return 0;
    }
    let chars = shared().chars.lock().unwrap_or_else(|e| e.into_inner());
    let Some(row) = chars.get(index as usize) else {
        return 0;
    };
    unsafe { *out = *row };
    1
}

/// Queue `player_login` for a roster guid (after `bn_net_char_count` > 0).
#[no_mangle]
pub extern "C" fn bn_net_enter(guid: u64) -> u32 {
    send_cmd(Cmd::Enter(guid)) as u32
}

/// Kernel for every in-world action:
///   kind 1 = chat (text), 2 = attack (a=guid), 3 = target (a=guid),
///   4 = loot (a=guid), 5 = use gameobject (a=guid),
///   6 = accept quest (a=npc, b=quest), 7 = complete quest (a=npc, b=quest),
///   8 = quest query (b=quest), 9 = logout,
///   11 = use item (b = bag<<8 | slot, a = spell slot),
///   12 = auto-equip (b = bag<<8 | slot),
///   13 = swap inventory slots (a = src, b = dst).
/// A bag byte of 0xFF means the player's own equipment array
/// ([`BAG_EQUIPPED`]). Returns 1 when the command was queued.
#[no_mangle]
pub extern "C" fn bn_net_send(kind: u32, a: u64, b: u32, text: *const c_char) -> u32 {
    let cmd = match kind {
        1 => match cstr(text) {
            Some(t) => Cmd::Chat(t),
            None => return 0,
        },
        2 => Cmd::Attack(a),
        3 => Cmd::Target(a),
        4 => Cmd::Loot(a),
        5 => Cmd::GameObjUse(a),
        6 => Cmd::QuestAccept { npc: a, quest: b },
        7 => Cmd::QuestComplete { npc: a, quest: b },
        8 => Cmd::QuestQuery(b),
        9 => Cmd::Logout,
        // 10 = cast at self (a = spell id); targeted casts go through
        // `bn_net_cast`, which can carry the full 64-bit guid.
        10 => Cmd::Cast { spell: a as u32, target: 0 },
        11 => Cmd::UseItem {
            bag: ((b >> 8) & 0xff) as u8,
            slot: (b & 0xff) as u8,
            spell: (a & 0xff) as u8,
        },
        12 => Cmd::AutoEquip {
            bag: ((b >> 8) & 0xff) as u8,
            slot: (b & 0xff) as u8,
        },
        13 => Cmd::SwapInv {
            src: (a & 0xff) as u8,
            dst: (b & 0xff) as u8,
        },
        // 14 = autostore one loot row by slot, 15 = take the coins, 16 = close.
        14 => Cmd::LootItem(a as u8),
        15 => Cmd::LootMoney,
        16 => Cmd::LootRelease(a),
        // 17 = release the spirit (the dead screen's button).
        17 => Cmd::Repop,
        // 18 = gossip hello (a = npc), 19 = query a quest's accept panel
        // (a = npc, b = quest), 20 = accept (a = npc, b = quest),
        // 21 = complete/hand-in (a = npc, b = quest), 22 = choose reward
        // (a = npc, b = quest | reward << 16), 23 = request the reward panel
        // (a = npc, b = quest).
        18 => Cmd::GiverHello(a),
        19 => Cmd::QuestQueryGiver {
            npc: a,
            quest: b & 0xffff,
        },
        20 => Cmd::QuestAccept {
            npc: a,
            quest: b & 0xffff,
        },
        21 => Cmd::QuestComplete {
            npc: a,
            quest: b & 0xffff,
        },
        22 => Cmd::QuestReward {
            npc: a,
            quest: b & 0xffff,
            reward: b >> 16,
        },
        23 => Cmd::QuestRequestReward {
            npc: a,
            quest: b & 0xffff,
        },
        _ => return 0,
    };
    send_cmd(cmd) as u32
}

/// The open loot window as the host draws it. Returns 1 while a window is open
/// (`guid` set), 0 otherwise.
#[no_mangle]
pub extern "C" fn bn_net_loot(out: *mut BnNetLoot) -> u32 {
    if out.is_null() {
        return 0;
    }
    let window = *shared().loot.lock().unwrap_or_else(|e| e.into_inner());
    let open = window.guid != 0;
    unsafe {
        *out = window;
    }
    open as u32
}

/// The open questgiver window. Returns its mode (0 closed, 1 menu,
/// 2 accept panel, 3 reward/progress panel).
#[no_mangle]
pub extern "C" fn bn_net_giver(out: *mut BnNetGiver) -> u32 {
    if out.is_null() {
        return 0;
    }
    let giver = *shared().giver.lock().unwrap_or_else(|e| e.into_inner());
    let mode = giver.mode;
    unsafe {
        *out = giver;
    }
    mode
}

/// Cast `spell` at `target` (0 = self). Answers arrive as `cast|spell|target`
/// or `error|cast|...` event lines.
#[no_mangle]
pub extern "C" fn bn_net_cast(spell: u32, target: u64) -> u32 {
    send_cmd(Cmd::Cast { spell, target }) as u32
}

/// Pop queued event lines ("kind|field|...") into `buf`, NUL-terminated.
/// Returns the byte count (0 when the queue is empty or `cap` is too small).
#[no_mangle]
pub extern "C" fn bn_net_event(buf: *mut c_char, cap: u32) -> u32 {
    if buf.is_null() || cap == 0 {
        return 0;
    }
    let text = shared().drain_events(cap as usize - 1);
    if text.is_empty() {
        return 0;
    }
    let bytes = text.as_bytes();
    let n = bytes.len().min(cap as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, n);
        *buf.add(n) = 0;
    }
    n as u32
}

/// Close the session (also used on world teardown).
#[no_mangle]
pub extern "C" fn bn_net_disconnect() -> u32 {
    send_cmd(Cmd::Logout) as u32
}

/// Fill `out` with the logged-in player's descriptor state. Returns 1 when a
/// snapshot exists (i.e. the world stream has delivered the player object).
#[no_mangle]
pub extern "C" fn bn_net_player(out: *mut BnNetPlayer) -> u32 {
    if out.is_null() {
        return 0;
    }
    let sh = shared();
    let player = sh.player.lock().unwrap_or_else(|e| e.into_inner());
    match player.as_ref() {
        Some(p) => {
            unsafe { *out = *p };
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub extern "C" fn bn_net_self_guid() -> u64 {
    shared().self_guid.load(Ordering::SeqCst)
}

/// Number of objects currently tracked from the world stream.
#[no_mangle]
pub extern "C" fn bn_net_unit_count() -> u32 {
    shared()
        .units
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u32
}

/// Fill `out` with one tracked object. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_net_unit(index: u32, out: *mut BnNetUnit) -> u32 {
    if out.is_null() {
        return 0;
    }
    let units = shared().units.lock().unwrap_or_else(|e| e.into_inner());
    // Deterministic order: guids sorted, so the host can page through a stable
    // view while its own hash order is irrelevant.
    let mut guids: Vec<u64> = units.keys().copied().collect();
    guids.sort_unstable();
    let Some(guid) = guids.get(index as usize).copied() else {
        return 0;
    };
    let rec = units.get(&guid).expect("guid from the same map");
    let row = unit_row(guid, rec);
    unsafe { *out = row };
    1
}

/// One tracked object by guid (the host's target lookup: no sort, one hit).
#[no_mangle]
pub extern "C" fn bn_net_unit_by_guid(guid: u64, out: *mut BnNetUnit) -> u32 {
    if out.is_null() || guid == 0 {
        return 0;
    }
    let units = shared().units.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rec) = units.get(&guid) else {
        return 0;
    };
    let row = unit_row(guid, rec);
    unsafe { *out = row };
    1
}

fn unit_row(guid: u64, rec: &UnitRec) -> BnNetUnit {
    let f = &rec.fields;
    // Player appearance bytes; creatures leave all of this zero.
    let bytes0 = f.unit_bytes_0().unwrap_or(0);
    let pb = f.player_bytes().unwrap_or(0);
    let pb2 = f.player_bytes_2().unwrap_or(0);
    // `OBJECT_FIELD_ENTRY` = descriptor index 3. A creature's template answers
    // the target frame's name/rank; a player has no such row.
    let entry = f
        .raw_fields()
        .find(|(index, _)| *index == 3)
        .map(|(_, value)| value)
        .unwrap_or(0);
    let template = shared()
        .creatures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry)
        .cloned();
    BnNetUnit {
        guid,
        kind: rec.kind,
        level: f.unit_level().unwrap_or(0),
        display_id: f.unit_displayid().unwrap_or(0),
        health: f.unit_health().unwrap_or(0),
        max_health: f.unit_max_health().unwrap_or(0),
        target: f.unit_target().unwrap_or(0),
        mount: f.unit_mount_display_id(),
        dead: f.unit_is_dead() as u8,
        x: rec.pos[0],
        y: rec.pos[1],
        z: rec.pos[2],
        facing: rec.facing,
        race: (bytes0 & 0xff) as u8,
        sex: ((bytes0 >> 16) & 0xff) as u8,
        skin: (pb & 0xff) as u8,
        face: ((pb >> 8) & 0xff) as u8,
        hair: ((pb >> 16) & 0xff) as u8,
        hair_color: ((pb >> 24) & 0xff) as u8,
        facial: (pb2 & 0xff) as u8,
        // Equipment slots are only broadcast for players; each visible entry is
        // an item template the server answers with its display id.
        visible_display: {
            let displays = shared()
                .item_displays
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::array::from_fn(|i| {
                if rec.kind != 4 {
                    return 0;
                }
                let entry = f.player_visible_item_entry(i as u8).unwrap_or(0);
                displays.get(&entry).copied().unwrap_or(0)
            })
        },
        // Template entry (OBJECT_FIELD_ENTRY) plus the creature name/rank once
        // the query answers; players have no creature template, so both stay 0.
        entry,
        rank: template.as_ref().map(|t| t.rank).unwrap_or(0),
        creature_type: template.as_ref().map(|t| t.creature_type).unwrap_or(0),
        name: {
            let mut buf = [0 as c_char; 64];
            if let Some(t) = &template {
                for (dst, src) in buf.iter_mut().zip(t.name.as_bytes().iter().take(63)) {
                    *dst = *src as c_char;
                }
            }
            buf
        },
    }
}

/// Drop the tracked objects, e.g. after leaving the world.
#[no_mangle]
pub extern "C" fn bn_net_units_clear() -> u32 {
    let sh = shared();
    sh.units
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    *sh.player.lock().unwrap_or_else(|e| e.into_inner()) = None;
    1
}

/// Fill `out` with one quest template (title, objectives, details). Returns 0
/// until the server answers the log's query.
#[no_mangle]
pub extern "C" fn bn_net_quest_info(quest_id: u32, out: *mut BnNetQuestInfo) -> u32 {
    if out.is_null() || quest_id == 0 {
        return 0;
    }
    let sh = shared();
    let quests = sh.quests.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = quests.get(&quest_id) else {
        return 0;
    };
    let mut row = BnNetQuestInfo {
        quest_id,
        level: q.level,
        zone: q.zone,
        money: q.money,
        flags: q.flags,
        ..Default::default()
    };
    copy_name(&mut row.title, &q.title);
    copy_name(&mut row.objectives, &q.objectives);
    copy_name(&mut row.details, &q.details);
    unsafe { *out = row };
    1
}

/// Learned spells in book order.
#[no_mangle]
pub extern "C" fn bn_net_spell_count() -> u32 {
    shared()
        .spells
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u32
}

/// The spell id at `index`; 0 when out of range.
#[no_mangle]
pub extern "C" fn bn_net_spell(index: u32) -> u32 {
    let sh = shared();
    let book = sh.spells.lock().unwrap_or_else(|e| e.into_inner());
    book.get(index as usize).copied().unwrap_or(0)
}

/// Occupied action-bar slots, sorted by slot index.
#[no_mangle]
pub extern "C" fn bn_net_action_count() -> u32 {
    shared()
        .actions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u32
}

/// One action-bar slot (`slot`, `kind` 0 spell/0x40 macro/0x80 item, `action`).
#[no_mangle]
pub extern "C" fn bn_net_action(index: u32, out: *mut BnNetAction) -> u32 {
    if out.is_null() {
        return 0;
    }
    let sh = shared();
    let slots = sh.actions.lock().unwrap_or_else(|e| e.into_inner());
    let Some((slot, kind, action)) = slots.get(index as usize) else {
        return 0;
    };
    let row = BnNetAction {
        slot: *slot,
        kind: *kind,
        action: *action,
    };
    unsafe { *out = row };
    1
}

/// An active cast by `caster` (0 = the local player). Returns 1 with the
/// elapsed/duration split the host fills a cast bar with, 0 when idle. Stale
/// entries (missed GO) expire after their duration plus a second.
#[no_mangle]
pub extern "C" fn bn_net_cast_state(caster: u64, out: *mut BnNetCast) -> u32 {
    if out.is_null() {
        return 0;
    }
    let sh = shared();
    let guid = if caster != 0 {
        caster
    } else {
        sh.self_guid.load(Ordering::SeqCst)
    };
    let mut casts = sh.casts.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = casts.get(&guid) else {
        return 0;
    };
    let elapsed = c.started.elapsed();
    let dur = c.duration_ms as u64;
    if elapsed.as_millis() as u64 > dur + 1000 {
        casts.remove(&guid);
        return 0;
    }
    let row = BnNetCast {
        caster: guid,
        spell_id: c.spell_id,
        duration_ms: c.duration_ms,
        elapsed_ms: (elapsed.as_millis() as u64).min(dur) as u32,
        target: c.target,
    };
    unsafe { *out = row };
    1
}

/// Occupied inventory slots (equipment 0..18, backpack, equipped bags).
#[no_mangle]
pub extern "C" fn bn_net_inv_count() -> u32 {
    inventory_rows(shared()).len() as u32
}

/// Fill up to `cap` inventory rows; returns how many were written. The row's
/// `bag`/`slot` feed `bn_net_send` kinds 11/12 unchanged.
#[no_mangle]
pub extern "C" fn bn_net_inv(out: *mut BnNetInvItem, cap: u32) -> u32 {
    if out.is_null() || cap == 0 {
        return 0;
    }
    let rows = inventory_rows(shared());
    let n = rows.len().min(cap as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(rows.as_ptr(), out, n);
    }
    n as u32
}

/// Fill `out` with one item template (name, quality, slot family). Returns 1
/// once the server has answered the entry's query.
#[no_mangle]
pub extern "C" fn bn_net_item_info(entry: u32, out: *mut BnNetItemInfo) -> u32 {
    if out.is_null() || entry == 0 {
        return 0;
    }
    let sh = shared();
    let templates = sh
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(_) = templates.get(&entry) else {
        return 0;
    };
    let row = item_info_row(entry, &templates);
    unsafe { *out = row };
    1
}

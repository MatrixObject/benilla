//! Network middle layer: realmd logon + world session on a worker thread.
//!
//! The client's networking lives in the app crate; this
//! module carries the same `benilla-protocol` session over a plain TCP socket
//! so the UE host can log in, list characters and enter the world without
//! linking the app. Everything below is blocking socket work on its own
//! thread: the game thread only reads the shared status / event queue, which
//! keeps frame times flat.
//!
//! The UE side declares these as `extern "C"`:
//! bn_net_connect(host, user, pass) -> 1 when the worker started
//! bn_net_status -> stage code (see ST_*)
//! bn_net_char_count / bn_net_char(i, out)
//! bn_net_enter(guid)
//! bn_net_send(kind, a, b, text)
//! bn_net_group(out) -> 1 while in a group / invite pending
//! bn_net_event(buf, cap) -> bytes popped (0 = empty)
//! bn_net_disconnect

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use benilla_protocol::messages::opcode as MOVE_OP;
use benilla_protocol::messages::{
    AttackSwingError, AttackerState, CastOutcome, CharCreateReq, ItemInfo, MonsterMoveFacing,
    Object, ObjectFields, ObjectType, ServerPacket, BAG_PLAYER_INVENTORY, SLOT_PACK_FIRST,
};
// The server-authored path walk: benilla's own sampler, shared with the Bevy client.
use benilla_protocol::motion::{monster_move_spline, Spline};
use benilla_protocol::RealmInfo;
use benilla_protocol::WorldSession;
use benilla_ui::script::ScriptValue;

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
    /// `ItemDisplayInfo.dbc` ids in the enum's equipment order (head.. tabard);
    /// 0 = empty slot. This is what the dress-up pass needs to show the gear
    /// on the character-select model.
    pub equip_display: [u32; 19],
    /// `InventoryType` per slot, same order.
    pub equip_inv: [u8; 19],
    /// `PLAYER_BYTES`/`PLAYER_BYTES_2` customization: the CharSections
    /// compositor's input, so a character-select preview can dress the body.
    pub skin: u8,
    pub face: u8,
    pub hair_style: u8,
    pub hair_color: u8,
    pub facial_hair: u8,
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
            skin: 0,
            face: 0,
            hair_style: 0,
            hair_color: 0,
            facial_hair: 0,
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
    /// `PLAYER_FLAGS & 0x10`: released as a spirit. The reference's FFXDeath gate and the
    /// body's CharProc nodes; `dead` alone also covers a corpse that has not released.
    pub ghost: u8,
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
    /// Primary stats after buffs, 0 Str, 1 Agi, 2 Sta, 3 Int, 4 Spi; plus the
    /// melee numbers the character sheet shows.
    pub stat: [u32; 5],
    pub armor: i32,
    pub attack_power: i32,
    pub damage_min: f32,
    pub damage_max: f32,
    pub attack_time: u32,
    pub pos: [f32; 3],
    pub facing: f32,
}

/// One `CMD_REALM_LIST` row as the host draws the server picker.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetRealm {
    /// `host:port` of the world server, as the client would dial it.
    pub address: [c_char; 128],
    /// NUL-terminated UTF-8 realm name.
    pub name: [c_char; 64],
    /// This account's character count there, shown as `"(3)"`.
    pub characters: u8,
    /// The realm type (a `Cfg_Configs.dbc` row index, not a fixed enum).
    pub realm_type: u32,
    /// The realm flags the client holds (0x01 invalid, 0x02 offline, 0x04,
    /// plus the 0x20/0x40/0x80 Recommended/New/Full bits).
    pub flags: u8,
}

impl Default for BnNetRealm {
    fn default() -> Self {
        Self {
            address: [0; 128],
            name: [0; 64],
            characters: 0,
            realm_type: 0,
            flags: 0,
        }
    }
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
            ghost: 0,
            quest_count: 0,
            quest_id: [0; 20],
            quest_state: [0; 20],
            aura_count: 0,
            aura_slot: [0; 48],
            aura_spell: [0; 48],
            aura_stacks: [0; 48],
            stat: [0; 5],
            armor: 0,
            attack_power: 0,
            damage_min: 0.0,
            damage_max: 0.0,
            attack_time: 0,
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
    /// The unit's own gait, as the native client picks it from the server's
    /// `SMSG_MONSTER_MOVE`: AnimationData id 0 Stand, 4 Walk, 5 Run. `run_mode`
    /// is `SPLINEFLAG_RUNMODE` (clear = `MOVEFLAG_WALK_MODE`), and only the
    /// spline's live window plays a gait; once it ends the unit stands.
    pub anim: u32,
    /// `UNIT_NPC_FLAGS`: the services this unit offers, in the 1.12 numbering
    /// (`crates/benilla-app/src/target/cursor_mode.rs`): 0x1 gossip, 0x2 quest
    /// giver, 0x4 vendor, 0x8 flightmaster, 0x80 innkeeper, 0x4000 repair. The
    /// client gates `CanMerchantRepair` (`0x4fadb0`) on the repair bit and the
    /// merchant window on the vendor bit. Past NPC-service range the server
    /// refuses `CMSG_LIST_INVENTORY` with `SMSG_SELL_ITEM`/`CANT_FIND_VENDOR`
    /// (vmangos "not found or you can't interact with him", 3D distance).
    pub npc_flags: u32,
    /// A dead unit whose `UNIT_DYNAMIC_FLAGS` bit 0 is set: the corpse still holds
    /// loot, so the reference's corpse sparkle is armed (`0x5d6e30`/`0x600640`,
    /// "HARDCODED Loot Art" at attach `0x13`) until the server clears the bit.
    pub lootable: u8,
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
            anim: 0,
            npc_flags: 0,
            lootable: 0,
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
    /// `item_template.startquest`: a quest starter takes the quest arm of `UseContainerItem`
    /// ahead of the equip one (`0x4fa3bd`), so an equippable quest item never auto-equips.
    start_quest: u32,
    /// `item_template.material`: the swing and draw sound's key.
    material: u32,
    /// `item_template.sheath`: where a stowed version of this item hangs.
    sheath: u32,
    /// The whole response row: what the VM's item-tooltip builder reads beyond the
    /// fields above (stats, damage, spells, bonding, requirements), as the
    /// reference keeps one item cache for every consumer (`0x5d8b00`).
    info: Box<ItemInfo>,
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
pub(crate) struct CreatureTemplateLite {
    pub(crate) name: String,
    pub(crate) subname: String,
    pub(crate) creature_type: u32,
    pub(crate) rank: u32,
    pub(crate) civilian: bool,
    pub(crate) racial_leader: bool,
    /// `CreatureTemplate` `type_flags`; bit 0x8 is `DO_NOT_PLAY_WOUND_ANIM`, which refuses the
    /// wound flinch and the parry (`0x60ea9f`, `0x60ec1f`).
    pub(crate) type_flags: u32,
}

/// One quest objective from `SMSG_QUEST_QUERY_RESPONSE`: the template arrays the
/// quest log's leaderboard reads.
#[derive(Clone, Default)]
pub(crate) struct QuestObjectiveLite {
    /// A creature entry, else a GameObject entry with bit `0x8000_0000`.
    pub creature_or_go: u32,
    pub required_count: u32,
    pub item_id: u32,
    pub item_count: u32,
    /// The objective's override text (empty when the name comes from the cache).
    pub text: String,
}

/// One quest template from `SMSG_QUEST_QUERY_RESPONSE` (the quest log's detail
/// view: the descriptors only carry ids and counters).
#[derive(Clone)]
pub(crate) struct QuestTemplateLite {
    pub level: u32,
    pub zone: i32,
    /// `QuestInfo.dbc`'s row id, which the quest log's tag name resolves through.
    pub quest_type: u32,
    pub money: i32,
    pub flags: u32,
    pub title: String,
    pub objectives: String,
    pub details: String,
    pub end_text: String,
    pub src_item_id: u32,
    pub reward_spell: u32,
    pub objective_rows: Vec<QuestObjectiveLite>,
    pub rewards: [(u32, u32); 4],
    pub choices: [(u32, u32); 6],
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

/// One row of a vendor's stock as the host draws it (`SMSG_LIST_INVENTORY`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetVendorRow {
    /// 1-based list position on the wire (not a `CMSG_BUY_ITEM` argument).
    pub slot: u32,
    /// `item_template` entry; resolve the icon through `bn_net_item_info`.
    pub entry: u32,
    /// `item_template.display_id` as the server reports it (0 = unknown).
    pub display_id: u32,
    /// Remaining stock (0 = sold out); `u32::MAX` = unlimited.
    pub count: u32,
    /// Buy price in copper, reputation-discounted by the server.
    pub price: u32,
    /// Max durability of the item on purchase (0 = no durability).
    pub max_durability: u32,
    /// Copies one purchase grants (`item_template.buy_count`).
    pub buy_count: u32,
}

impl Default for BnNetVendorRow {
    fn default() -> Self {
        Self {
            slot: 0,
            entry: 0,
            display_id: 0,
            count: 0,
            price: 0,
            max_durability: 0,
            buy_count: 0,
        }
    }
}

/// The open vendor/shop window (0 rows / guid 0 = closed).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetVendor {
    /// `guid != 0` = a stock list is showing for this vendor.
    pub guid: u64,
    pub count: u32,
    pub rows: [BnNetVendorRow; 128],
}

impl Default for BnNetVendor {
    fn default() -> Self {
        Self {
            guid: 0,
            count: 0,
            rows: [BnNetVendorRow::default(); 128],
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

/// An observed cast in progress (`SMSG_SPELL_START`.. `SMSG_SPELL_GO`).
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

/// Party roster width the host lays out (self excluded: the HUD already draws
/// the player's own frame).
pub const BN_PARTY_SLOTS: usize = 4;

/// One `SMSG_GROUP_LIST` row as the host draws it (`SMSG_PARTY_MEMBER_STATS`
/// patches the live fields in place).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetPartyMember {
    pub guid: u64,
    /// NUL-terminated UTF-8; the client's roster names are ASCII.
    pub name: [c_char; 64],
    pub level: u32,
    /// `AreaTable.dbc` zone id from the last full stats update.
    pub zone: u32,
    /// 0/1: the list row says the member is present (`listed_online`).
    pub online: u32,
    pub leader: u32,
    pub dead: u32,
    pub health: u32,
    pub max_health: u32,
    pub power: u32,
    pub max_power: u32,
    /// 0 mana, 1 rage, 2 focus, 3 energy.
    pub power_type: u32,
}

impl Default for BnNetPartyMember {
    fn default() -> Self {
        Self {
            guid: 0,
            name: [0; 64],
            level: 0,
            zone: 0,
            online: 0,
            leader: 0,
            dead: 0,
            health: 0,
            max_health: 0,
            power: 0,
            max_power: 0,
            power_type: 0,
        }
    }
}

/// The live group: roster plus the pending-invite popup state.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BnNetParty {
    /// Listed members, self excluded (0..=4).
    pub count: u32,
    pub in_group: u32,
    /// 0 none, 1 party, 2 raid (the wire's 0/1 plus one).
    pub group_type: u32,
    /// 1 while an invite popup is waiting for accept/decline.
    pub invite: u32,
    pub inviter: [c_char; 64],
    pub members: [BnNetPartyMember; BN_PARTY_SLOTS],
}

impl Default for BnNetParty {
    fn default() -> Self {
        Self {
            count: 0,
            in_group: 0,
            group_type: 0,
            invite: 0,
            inviter: [0; 64],
            members: [BnNetPartyMember::default(); BN_PARTY_SLOTS],
        }
    }
}

/// One tracked object: its descriptor snapshot plus the last known position.
struct UnitRec {
    kind: u8,
    fields: ObjectFields,
    pos: [f32; 3],
    facing: f32,
    /// The live `SMSG_MONSTER_MOVE` spline this unit is walking, evaluated by benilla's own
    /// [`Spline`] (the same sampler the Bevy client uses); `None` at rest.
    spline: Option<Spline>,
}

/// The unit's present pose: benilla's spline sample while one exists, else the
/// last streamed position.
///
/// Sampling even after the walk's duration has run is deliberate: `sample` clamps
/// its fraction to 1.0, so a finished spline reports its ENDPOINT and the unit
/// stays there. Gating on `running` instead fell back to `rec.pos`, which
/// `apply_monster_move` had set to the spline's START - so every creature snapped
/// back to where it began the moment it stopped (the "回到原点"/stutter report).
fn effective_pose(rec: &UnitRec) -> ([f32; 3], f32) {
    let now = std::time::Instant::now();
    match &rec.spline {
        Some(s) => {
            let (pos, facing, _pitch) = s.sample(now);
            (pos, facing.unwrap_or(rec.facing))
        }
        None => (rec.pos, rec.facing),
    }
}

/// Begin a [`Spline`] on `guid` from a `SMSG_MONSTER_MOVE`, through benilla's own
/// [`monster_move_spline`]; a stop or a degenerate path settles the unit on `start`.
#[allow(clippy::too_many_arguments)]
fn apply_monster_move(
    guid: u64,
    start: [f32; 3],
    path: &[[f32; 3]],
    facing: &MonsterMoveFacing,
    stop: bool,
    spline_id: u32,
    duration_ms: u32,
    flying: bool,
    run_mode: bool,
    sh: &Shared,
) {
    let mut units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    // Resolve the final facing first (immutable borrows end before get_mut).
    let face = match facing {
        MonsterMoveFacing::Angle(a) => Some(*a),
        MonsterMoveFacing::Spot(p) => Some((p[1] - start[1]).atan2(p[0] - start[0])),
        MonsterMoveFacing::Target(g) => units.get(g).map(|t| {
            let (tp, _) = effective_pose(t);
            (tp[1] - start[1]).atan2(tp[0] - start[0])
        }),
        MonsterMoveFacing::None => None,
    };
    let Some(rec) = units.get_mut(&guid) else {
        return;
    };
    let path: Vec<[f32; 3]> = if path.is_empty() {
        vec![start]
    } else {
        path.to_vec()
    };
    match monster_move_spline(path, spline_id, stop, duration_ms, flying, run_mode, None) {
        Some(spline) => {
            rec.pos = spline.points[0];
            if let Some(f) = face {
                rec.facing = f;
            }
            rec.spline = Some(spline);
        }
        None => {
            // A stop / zero duration / one-point path: no walk. Keep where the last
            // walk ENDED - a stop normally carries the final point, but a zeroed
            // `start` (seen on stop packets) used to slam the unit to the world
            // origin, which read as "creatures teleport back the moment they halt".
            let ended = rec.spline.as_ref().and_then(|s| s.points.last().copied());
            rec.pos = if start == [0.0, 0.0, 0.0] {
                ended.unwrap_or(rec.pos)
            } else {
                start
            };
            rec.spline = None;
            if let Some(f) = face {
                rec.facing = f;
            }
        }
    }
}

/// Commands the game thread hands to the session thread.
pub(crate) enum Cmd {
    Enter(u64),
    Chat(String),
    Attack(u64),
    /// `CMSG_QUESTGIVER_STATUS_QUERY` (`0x182`): the NPC's overhead `!`/`?`, asked by
    /// `quest_markers::sweep` off the client's own rules.
    QuestgiverStatusQuery(u64),
    /// `CMSG_TAXINODE_STATUS_QUERY` (`0x1aa`): whether we know a flight master's node; the
    /// green `!` follows an unknown one. Sent only from the per-unit re-gate (`0x6073e8`).
    TaxiNodeStatusQuery(u64),
    /// `CMSG_ATTACKSTOP`: the auto-attack toggle's off arm (`0x6131a0`).
    StopAttack,
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
    /// `CMSG_RECLAIM_CORPSE`: come back at the corpse (the corpse guid is ignored).
    ReclaimCorpse(u64),
    /// `CMSG_RESURRECT_RESPONSE`: answer a resurrection offer.
    ResurrectResponse {
        caster: u64,
        accept: bool,
    },
    /// `CMSG_SPIRIT_HEALER_ACTIVATE`: take the spirit healer's resurrection.
    SpiritHealer(u64),
    /// `CMSG_SELF_RES`: spend `PLAYER_SELF_RES_SPELL` (a soulstone, reincarnation).
    SelfRes,
    /// `CMSG_GOSSIP_SELECT_OPTION`: take gossip option `index` (0-based wire order).
    GossipSelect {
        npc: u64,
        index: u32,
    },
    /// `CMSG_NPC_TEXT_QUERY`: the greeting a gossip menu's `text_id` was latched on.
    NpcTextQuery {
        text_id: u32,
        npc: u64,
    },
    /// `CMSG_QUESTGIVER_QUERY_QUEST` from a gossip quest row.
    GossipQuest {
        npc: u64,
        quest: u32,
    },
    /// `CMSG_QUESTLOG_REMOVE_QUEST`: abandon the quest in log slot `slot` (0-based).
    QuestAbandon {
        slot: u8,
    },
    /// `CMSG_SET_ACTION_BUTTON`: write (or clear, `packed == 0`) one bar slot.
    SetAction {
        slot: u8,
        packed: u32,
    },
    /// `CMSG_SET_ACTIONBAR_TOGGLES`.
    ActionBarToggles(u8),
    /// `CMSG_TOGGLE_PVP`.
    TogglePvp,
    /// `CMSG_TOGGLE_HELM`.
    ToggleHelm,
    /// `CMSG_TOGGLE_CLOAK`.
    ToggleCloak,
    /// `CMSG_ITEM_QUERY_SINGLE` for a template the UI asked about.
    ItemQuery(u32),
    /// `CMSG_LEARN_TALENT`: a `Talent.dbc` row and the 0-based rank to learn up to.
    LearnTalent {
        talent_id: u32,
        rank: u32,
    },
    /// A chat line on any lane; `target` is the whisper target or channel name.
    ChatKind {
        kind: u32,
        target: Option<String>,
        msg: String,
    },
    /// `CMSG_QUESTGIVER_HELLO`: the quest menu of a unit flagged QUESTGIVER.
    GiverHello(u64),
    /// `CMSG_GOSSIP_HELLO`: the gossip menu, which vmangos answers for any unit
    /// carrying `UNIT_NPC_FLAGS` gossip (`QuestHandler.cpp:104`).
    GossipHello(u64),
    /// `CMSG_QUESTGIVER_QUERY_QUEST`: the accept panel for one quest.
    QuestQueryGiver {
        npc: u64,
        quest: u32,
    },
    /// `CMSG_QUESTGIVER_REQUEST_REWARD`: the progress panel's Continue, opens the
    /// offer-reward panel.
    QuestRequestReward {
        npc: u64,
        quest: u32,
    },
    /// `CMSG_QUESTGIVER_CHOOSE_REWARD`: hand in and take reward `reward`.
    QuestReward {
        npc: u64,
        quest: u32,
        reward: u32,
    },
    GameObjUse(u64),
    QuestAccept {
        npc: u64,
        quest: u32,
    },
    QuestComplete {
        npc: u64,
        quest: u32,
    },
    QuestQuery(u32),
    /// Cast `spell` at `target` (0 = self), answered by `SMSG_CAST_RESULT`.
    Cast {
        spell: u32,
        target: u64,
    },
    /// `CMSG_CANCEL_CAST`: stop the spell being cast (the id is informational).
    CancelCast(u32),
    /// `CMSG_CANCEL_AURA`: cancel one of our own buffs by spell id.
    CancelAura(u32),
    /// `CMSG_USE_ITEM`: bag 0xFF = equipped slot; `spell` is the item spell slot.
    UseItem {
        bag: u8,
        slot: u8,
        spell: u8,
    },
    /// Auto-equip the item at this wire position. Sent as `CMSG_AUTOEQUIP_ITEM`'s equivalent swap
    /// (see the dispatch arm): this server refuses that packet, so the destination is picked here.
    /// The bag byte is the wire's: 0xFF for the player's own array (equipment 0..18, pack 23..38),
    /// 19..22 for a worn bag's contents.
    AutoEquip {
        bag: u8,
        slot: u8,
    },
    /// `CMSG_SWAP_INV_ITEM`: two slots of the player's own equipment/pack array.
    SwapInv {
        src: u8,
        dst: u8,
    },
    /// `CMSG_SWAP_ITEM`: one item between two wire positions. The wire takes the destination
    /// first (`Server/Packets/Item.cpp:30-36`), which is why the fields are ordered so.
    SwapItem {
        dst_bag: u8,
        dst_slot: u8,
        src_bag: u8,
        src_slot: u8,
    },
    /// `CMSG_SPLIT_ITEM`: `count` of the source stack into the destination slot.
    SplitItem {
        src_bag: u8,
        src_slot: u8,
        dst_bag: u8,
        dst_slot: u8,
        count: u8,
    },
    /// `CMSG_AUTOSTORE_BAG_ITEM`: put the item in a container, the server picking the seat.
    AutoStore {
        src_bag: u8,
        src_slot: u8,
        dst_bag: u8,
    },
    /// `CMSG_DESTROYITEM`: `DeleteCursorItem`'s item at a wire position; `count` 0 is the whole
    /// stack and the server sends no reply.
    DestroyItem {
        bag: u8,
        slot: u8,
        count: u8,
    },
    /// `CMSG_SET_AMMO`: load the ranged attack's ammo by template entry; it stays in its bag.
    SetAmmo(u32),
    /// `CMSG_GROUP_INVITE`: invite a player by name.
    GroupInvite(String),
    /// `CMSG_GROUP_ACCEPT`: take the pending invite.
    GroupAccept,
    /// `CMSG_GROUP_DECLINE`: refuse the pending invite (the host only closes
    /// its popup; the server also notifies the inviter).
    GroupDecline,
    /// `CMSG_GROUP_DISBAND`: leave or disband our group.
    GroupDisband,
    /// `CMSG_LIST_INVENTORY`: open a vendor's stock list.
    VendorList(u64),
    /// `CMSG_BUY_ITEM`: buy `count` stacks of the template entry from the vendor.
    VendorBuy {
        vendor: u64,
        entry: u32,
        count: u8,
    },
    /// `CMSG_SELL_ITEM`: sell `count` (0 = whole stack) of our item to the vendor.
    VendorSell {
        vendor: u64,
        item_guid: u64,
        count: u8,
    },
    /// `CMSG_BUY_ITEM_IN_SLOT`: the merchant cursor's drop into a named seat — `bag_guid` is the
    /// container's, or the player's own for the backpack and the equipment slots (`0x4c78fd`).
    VendorBuyInSlot {
        vendor: u64,
        entry: u32,
        bag_guid: u64,
        bag_slot: u8,
        count: u8,
    },
    /// `CMSG_BUYBACK_ITEM`: buy back a previously sold item from the vendor.
    VendorBuyback {
        vendor: u64,
        slot: u32,
    },
    /// `CMSG_REPAIR_ITEM`: repair `item_guid` (0 = everything) at the vendor.
    VendorRepair {
        vendor: u64,
        item_guid: u64,
    },
    /// `CMSG_CHAR_CREATE` at the character park; the roster is re-asked on
    /// success (the server streams a fresh `SMSG_CHAR_ENUM` first).
    CharCreateReq(CharCreateReq),
    /// `CMSG_CHAR_DELETE` at the character park.
    CharDelete(u64),
    /// `CMSG_CHAR_RENAME` for a character the server flagged for it.
    CharRename {
        guid: u64,
        name: String,
    },
    /// Re-ask the roster (after a create/delete/rename, or on re-entry).
    RefreshRoster,
    /// Pick realm `index` and world-connect on its address, replacing the
    /// default auto-pick; answers with a fresh roster at `ST_CHARS`.
    RealmPick(u32),
    /// Log out to the character park on the same link (`SMSG_LOGOUT_COMPLETE`);
    /// the socket stays authenticated, so a roster ask re-enters without
    /// re-authenticating.
    LogoutToRoster,
    Logout,
}

/// `ITEM_PUSH`'s bag selector: the reference's `bag` byte is the 1.12 wire bag (255 = the backpack,
/// 19..=22 an equipped bag), and `ITEM_PUSH` numbers the bar buttons 0..=23 (`0x491bb5`).
const ITEM_PUSH_KEYRING: i64 = -2;
/// The wire slots `OnItemPush` treats as the keyring (`0x491bc3`/`0x491bc8`, 0x51..=0x70), the
/// descriptor's full 32 positions (`ui_loot`'s `PUSH_KEYRING_SLOTS`).
const PUSH_KEYRING_SLOTS: std::ops::RangeInclusive<u32> = 81..=112;

/// `ITEM_PUSH`'s `arg1`, as `ui_loot::push_container` builds it: a wire bag other than 255 gives
/// `bag + 1` (20..=23, the ids the bar buttons carry), a keyring slot `-2`, anything else 0, the
/// backpack.
fn push_container(bag: u8, slot: u32) -> i64 {
    if bag != BAG_PLAYER_INVENTORY {
        return i64::from(bag) + 1;
    }
    if PUSH_KEYRING_SLOTS.contains(&slot) {
        return ITEM_PUSH_KEYRING;
    }
    0
}

/// A `SMSG_ITEM_PUSH_RESULT` for our own character waiting on its item template, so the UI tick
/// can fire `ITEM_PUSH` with the icon path. `OnItemPush 0x491a60` defers the event the same way: on
/// a cache miss (`0x491a93`) it re-enters from the item-cache callback (`0x491ee0`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingItemPush {
    pub entry: u32,
    /// `ITEM_PUSH`'s `arg1`, see [`push_container`].
    pub container: i64,
    /// Frames the UI tick has waited for the template, the reference's callback with no timeout.
    pub tries: u16,
}

struct Shared {
    status: AtomicU32,
    /// The guid handed to `player_login`; `UpdateObject` entries for it feed
    /// `player`.
    self_guid: AtomicU64,
    /// 0 = mover claim still pending, 1 = set_active_mover + worldport_ack sent.
    /// Only flips to 1 once the server has streamed our own UpdateObject
    /// (apply_object checks); claiming before that is discarded by vmangos.
    mover_ready: AtomicU32,
    events: Mutex<VecDeque<String>>,
    chars: Mutex<Vec<BnNetChar>>,
    /// Every tracked object in the stream window (the client's own object
    /// manager, reduced to what the FFI exposes).
    units: Mutex<HashMap<u64, UnitRec>>,
    /// Units auto-attacking, from `SMSG_ATTACKSTART` to `SMSG_ATTACKSTOP`: the reference's
    /// `[unit+0xc48]`, which picks the victim's CombatWound over StandWound (`0x60ea70`).
    engaged: Mutex<HashSet<u64>>,
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
    /// Entries the server answered with no record, so a row can stop waiting on its template.
    item_unknown: Mutex<HashSet<u32>>,
    /// `ChrClasses`' relic-slot flag for our class (`UnitHasRelicSlot`, `0x519e50`): the RANGED
    /// seat takes a relic for Paladin, Shaman and Druid and a ranged weapon otherwise. Written by
    /// the UI pass that loads the catalog, read by the equip command's slot pick.
    relic_slot: AtomicBool,
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
    /// The open loot window (`SMSG_LOOT_RESPONSE`.. release/error).
    loot: Mutex<BnNetLoot>,
    /// The open questgiver window (gossip menu / details / reward).
    giver: Mutex<BnNetGiver>,
    /// The live group roster plus the pending-invite popup state.
    party: Mutex<BnNetParty>,
    /// The open vendor/shop window (`SMSG_LIST_INVENTORY`.. closed).
    vendor: Mutex<BnNetVendor>,
    /// The account's `CMD_REALM_LIST` rows; the host's server picker reads
    /// these, and `bn_net_realm_pick` world-connects on the chosen one.
    realms: Mutex<Vec<RealmInfo>>,
    /// Bumped whenever `realms` is replaced, so the glue host knows to re-read
    /// its list (the rows themselves are fetched by index).
    realms_epoch: AtomicU64,
    /// Bumped whenever the roster in `chars` is replaced.
    chars_epoch: AtomicU64,
    /// The last menu-mode failure the glue host turns into a status dialog.
    menu_error: Mutex<Option<String>>,
    /// The last `CMSG_CHAR_CREATE` reply `(name, code)` for the glue host.
    create_reply: Mutex<Option<(String, u8)>>,
    /// The last `CMSG_CHAR_DELETE` reply `(guid, code)` for the glue host.
    delete_reply: Mutex<Option<(u64, u8)>>,
    /// The last `CMSG_CHAR_RENAME` reply `(guid, name, code)` for the glue host.
    rename_reply: Mutex<Option<(u64, String, u8)>>,
    /// The logon credentials `bn_net_connect` started with, kept so
    /// `bn_net_reconnect` can re-log on without the host re-passing them.
    creds: Mutex<Option<(String, String, String)>>,
    /// The last logon's SRP6 session key, so `RealmPick`'s world connect
    /// re-authenticates on the same key. All-zero when no logon completed.
    session_key: Mutex<[u8; 40]>,
    /// The unit the local player picked (`CMSG_SET_SELECTION`). The server never
    /// echoes a selection back, so without this the UI cannot show a target.
    selection: AtomicU64,
    /// Player names from `SMSG_NAME_QUERY_RESPONSE`, keyed by guid.
    names: Mutex<HashMap<u64, String>>,
    /// Player guids a `CMSG_NAME_QUERY` was already sent for.
    names_asked: Mutex<HashSet<u64>>,
    /// A wire chat line waiting to be fired as its `CHAT_MSG_*` event on the UI tick.
    chat_events: Mutex<VecDeque<(String, Vec<ScriptValue>)>>,
    /// Our own pushes waiting on their item templates, in wire order: `ITEM_PUSH` fires once the
    /// icon is resolvable, so the stock bag buttons animate when the server says the item landed.
    item_pushes: Mutex<VecDeque<PendingItemPush>>,
    /// The pending resurrection offer's caster and its popup flags
    /// (`SMSG_RESURRECT_REQUEST`), 0 when none.
    resurrect_from: AtomicU64,
    resurrect_sickness: AtomicU32,
    resurrect_has_timer: AtomicU32,
    /// The spirit healer asking to confirm the XP loss
    /// (`SMSG_SPIRIT_HEALER_CONFIRM`), 0 when none.
    spirit_healer: AtomicU64,
    /// `SMSG_CORPSE_RECLAIM_DELAY` in ms, 0 until the server sends one.
    corpse_delay_ms: AtomicU32,
    /// Whether our own guid is in melee auto-attack, from
    /// `SMSG_ATTACKSTART`/`SMSG_ATTACKSTOP`.
    self_attacking: AtomicU32,
    /// The guid our auto-attack was started on, 0 when none: `AttackTarget`'s
    /// toggle needs it to tell "start" from "stop" (`0x6131a0`).
    attack_target: AtomicU64,
    /// The latched melee refusal (`0x5ecdb0`): code 1 out of range, 2 bad facing,
    /// with the time its next line is due. A landed swing clears it (`0x5ea800`);
    /// each shown line re-arms it four seconds later (`0x5ec9b6`).
    swing_refusal: Mutex<Option<(u8, std::time::Instant)>>,
    /// Our body's main-hand swing clip ([`swing_anim_main`]), resolved when the
    /// server starts our attack and again on a local swing press. The reference
    /// re-reads the armed main hand at each swing; the weapon cannot change
    /// without an equip, which stops and restarts the attack latch.
    self_swing_anim: AtomicU32,
    /// The last `SMSG_QUESTGIVER_STATUS` per NPC guid: the overhead `!`/`?` the marker lane
    /// draws. The server only ever answers a query (`QuestHandler.cpp:36-77`), so this holds the
    /// answer to the client's own ask, mirroring benilla's `QuestGiver` store.
    quest_status: Mutex<HashMap<u64, u32>>,
    /// The last `SMSG_TAXINODE_STATUS` per flight master guid: `false` draws the green `!`.
    taxi_known: Mutex<HashMap<u64, bool>>,
    /// Bumped by the packets whose answers can move a marker without touching our descriptor
    /// block (`SMSG_SET_FACTION_STANDING`, the quest-update packets, `SMSG_GROUP_LIST`); the
    /// sweep re-asks every giver when it moves, the app's `QuestGiver::bump_reask`.
    quest_epoch: AtomicU64,
    /// Latest player pose fed by the host (UE pawn) so the world worker can
    /// echo it back to the server on every heartbeat. None before the host
    /// starts its tick, (pos, yaw) afterward.
    last_pose: Mutex<Option<([f32; 3], f32)>>,
    /// Host's latest directional input: (forward, strafe, turn) each -1..1.
    /// Positive forward = W, negative = S. Positive strafe = D, negative = A.
    /// Positive turn = right, negative = left.
    last_input: Mutex<(f32, f32, f32)>,
}

impl Shared {
    fn set(&self, status: u32) {
        self.status.store(status, Ordering::SeqCst);
    }

    fn status(&self) -> u32 {
        self.status.load(Ordering::SeqCst)
    }

    fn event(&self, line: impl Into<String>) {
        let line = line.into();
        let mut q = self.events.lock().unwrap_or_else(|e| e.into_inner());
        // The host drains this every frame; the cap only guards a host that
        // stopped polling while the server keeps talking.
        if q.len() >= 1024 {
            q.pop_front();
        }
        q.push_back(line);
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

/// Read the shared roster under its lock: `f` maps the rows to its own shape.
/// The shared state is created lazily, so this always yields `Some`.
pub(crate) fn with_chars<R>(f: impl FnOnce(&[BnNetChar]) -> R) -> Option<R> {
    let sh = shared();
    let rows = sh.chars.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&rows))
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        status: AtomicU32::new(ST_IDLE),
        self_guid: AtomicU64::new(0),
        mover_ready: AtomicU32::new(0),
        events: Mutex::new(VecDeque::new()),
        chars: Mutex::new(Vec::new()),
        units: Mutex::new(HashMap::new()),
        engaged: Mutex::new(HashSet::new()),
        player: Mutex::new(None),
        item_displays: Mutex::new(HashMap::new()),
        item_asked: Mutex::new(HashSet::new()),
        item_queue: Mutex::new(Vec::new()),
        item_templates: Mutex::new(HashMap::new()),
        item_unknown: Mutex::new(HashSet::new()),
        relic_slot: AtomicBool::new(false),
        quests: Mutex::new(HashMap::new()),
        quest_asked: Mutex::new(HashSet::new()),
        spells: Mutex::new(Vec::new()),
        actions: Mutex::new(Vec::new()),
        casts: Mutex::new(HashMap::new()),
        creatures: Mutex::new(HashMap::new()),
        creature_asked: Mutex::new(HashSet::new()),
        loot: Mutex::new(BnNetLoot::default()),
        giver: Mutex::new(BnNetGiver::default()),
        party: Mutex::new(BnNetParty::default()),
        vendor: Mutex::new(BnNetVendor::default()),
        realms: Mutex::new(Vec::new()),
        realms_epoch: AtomicU64::new(0),
        chars_epoch: AtomicU64::new(0),
        menu_error: Mutex::new(None),
        create_reply: Mutex::new(None),
        delete_reply: Mutex::new(None),
        rename_reply: Mutex::new(None),
        creds: Mutex::new(None),
        session_key: Mutex::new([0u8; 40]),
        selection: AtomicU64::new(0),
        names: Mutex::new(HashMap::new()),
        names_asked: Mutex::new(HashSet::new()),
        chat_events: Mutex::new(VecDeque::new()),
        item_pushes: Mutex::new(VecDeque::new()),
        resurrect_from: AtomicU64::new(0),
        resurrect_sickness: AtomicU32::new(0),
        resurrect_has_timer: AtomicU32::new(0),
        spirit_healer: AtomicU64::new(0),
        corpse_delay_ms: AtomicU32::new(0),
        self_attacking: AtomicU32::new(0),
        attack_target: AtomicU64::new(0),
        swing_refusal: Mutex::new(None),
        self_swing_anim: AtomicU32::new(16),
        quest_status: Mutex::new(HashMap::new()),
        taxi_known: Mutex::new(HashMap::new()),
        quest_epoch: AtomicU64::new(0),
        last_pose: Mutex::new(None),
        last_input: Mutex::new((0.0f32, 0.0f32, 0.0f32)),
    })
}

fn commands() -> &'static Mutex<Option<Sender<Cmd>>> {
    static CMD: OnceLock<Mutex<Option<Sender<Cmd>>>> = OnceLock::new();
    CMD.get_or_init(|| Mutex::new(None))
}

pub(crate) fn send_cmd(cmd: Cmd) -> bool {
    let guard = commands().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(tx) => tx.send(cmd).is_ok(),
        None => false,
    }
}

// ---- snapshots for the UI feed ----
//
// The UI feed runs on the game thread while the session thread keeps writing the
// shared state, so every accessor clones what it needs under its own lock and
// returns; no lock is held while the Lua VM runs.

/// One tracked object as the unit frames need it.
pub(crate) struct UiUnitRec {
    pub kind: u8,
    pub fields: ObjectFields,
    pub pos: [f32; 3],
    pub facing: f32,
}

/// One tracked object's rendered feet: the same pose `bn_net_unit` answers, the live spline
/// included. The dll's own per-frame world readers (the selection ring, the nameplates) seat on it
/// so they never disagree with what the host is drawing.
pub(crate) fn ui_unit_pose(guid: u64) -> Option<[f32; 3]> {
    if guid == 0 {
        return None;
    }
    let sh = shared();
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let rec = units.get(&guid)?;
    Some(effective_pose(rec).0)
}

/// One tracked object by guid, cloned.
pub(crate) fn ui_unit(guid: u64) -> Option<UiUnitRec> {
    if guid == 0 {
        return None;
    }
    let sh = shared();
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let rec = units.get(&guid)?;
    Some(UiUnitRec {
        kind: rec.kind,
        fields: rec.fields.clone(),
        pos: rec.pos,
        facing: rec.facing,
    })
}

pub(crate) fn ui_self_guid() -> u64 {
    shared().self_guid.load(Ordering::SeqCst)
}

pub(crate) fn ui_selection() -> u64 {
    shared().selection.load(Ordering::SeqCst)
}

pub(crate) fn ui_player() -> Option<BnNetPlayer> {
    *shared().player.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn ui_names() -> HashMap<u64, String> {
    shared()
        .names
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Push one line onto the host's event queue (`bn_net_event`), for the middle layer's
/// own signals: the drained UI intents, the errors the VM cannot show by itself.
pub(crate) fn note(line: impl Into<String>) {
    shared().event(line);
}

/// Queue a `CHAT_MSG_*` event the network thread read off the wire, for [`take_chat_events`] to
/// fire on the UI tick. Bounded: a UI that is not draining cannot grow the queue without limit.
pub(crate) fn push_chat_event(name: &'static str, args: Vec<ScriptValue>) {
    let sh = shared();
    let mut q = sh.chat_events.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= 256 {
        q.pop_front();
    }
    q.push_back((name.to_string(), args));
}

/// Drain every queued chat event; called from the UI tick, which owns the Lua VM.
pub(crate) fn take_chat_events() -> Vec<(String, Vec<ScriptValue>)> {
    let sh = shared();
    let mut q = sh.chat_events.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_empty() {
        return Vec::new();
    }
    q.drain(..).collect()
}

/// Drain the pushes waiting on their item templates, in wire order.
pub(crate) fn take_item_pushes() -> Vec<PendingItemPush> {
    let sh = shared();
    let mut q = sh.item_pushes.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_empty() {
        return Vec::new();
    }
    q.drain(..).collect()
}

/// Put back the pushes still waiting, in front of anything queued since, keeping wire order.
pub(crate) fn keep_item_pushes(mut still: Vec<PendingItemPush>) {
    if still.is_empty() {
        return;
    }
    still.reverse();
    let sh = shared();
    let mut q = sh.item_pushes.lock().unwrap_or_else(|e| e.into_inner());
    for p in still {
        q.push_front(p);
    }
}

/// The roster row's name for `guid` (our own character's name arrives this way).
pub(crate) fn ui_roster_name(guid: u64) -> Option<String> {
    with_chars(|rows| {
        rows.iter().find(|c| c.guid == guid).map(|c| {
            unsafe { CStr::from_ptr(c.name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    })
    .flatten()
}

pub(crate) fn ui_creature(entry: u32) -> Option<CreatureTemplateLite> {
    shared()
        .creatures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry)
        .cloned()
}

/// A creature's name resolved through its guid's template entry: the client
/// keeps creature names in a record cache keyed by entry, not a guid table
/// (`NameCache::resolve_unit`), so `UnitName("target")` reads it as soon as
/// `SMSG_CREATURE_QUERY_RESPONSE` answers - `UNKNOWNOBJECT` until then.
pub(crate) fn ui_creature_name(guid: u64) -> Option<String> {
    let entry = ui_unit(guid)?.fields.object_entry().filter(|&e| e != 0)?;
    ui_creature(entry)
        .map(|c| c.name)
        .filter(|name| !name.is_empty())
}

pub(crate) fn ui_quests() -> HashMap<u32, QuestTemplateLite> {
    shared()
        .quests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, q)| (*id, q.clone()))
        .collect()
}

pub(crate) fn ui_item_info(entry: u32) -> BnNetItemInfo {
    let templates = shared()
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    item_info_row(entry, &templates)
}

/// The template facts a bag slot's use fork reads (`0x4fa3b9`): its class and its `StartQuest`,
/// `None` while the query is out — the reference's uncached template, which `0x5d9ff0` reads as
/// not equippable and the dispatcher's gates end silently.
pub(crate) fn ui_item_use_template(entry: u32) -> Option<(u32, u32)> {
    shared()
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry)
        .map(|t| (t.class, t.start_quest))
}

/// One cached template's whole row, the item-tooltip builder's source (`0x5d8b00`'s cache
/// lookup); `None` while the query is out.
pub(crate) fn ui_item_template(entry: u32) -> Option<ItemInfo> {
    shared()
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry)
        .map(|t| (*t.info).clone())
}

/// Every cached template's entry id: the UI feed's priming pass pushes all of them into a fresh
/// VM, as the reference's cache is complete from the moment the client holds it.
pub(crate) fn ui_cached_template_ids() -> Vec<u32> {
    shared()
        .item_templates
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect()
}

/// Whether the server answered an item query for this entry with no record; such a row stops
/// waiting on its template (`ui_loot`'s failed-query deviation).
pub(crate) fn ui_item_unknown(entry: u32) -> bool {
    shared()
        .item_unknown
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&entry)
}

/// Record whether our class's RANGED seat is a relic slot, from the `ChrClasses` catalog the UI
/// pass loads; the equip command reads it to pick a destination slot.
pub(crate) fn set_relic_slot(on: bool) {
    shared().relic_slot.store(on, Ordering::Relaxed);
}

pub(crate) fn ui_spells() -> Vec<u32> {
    shared()
        .spells
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn ui_actions() -> Vec<(u8, u8, u32)> {
    shared()
        .actions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn ui_loot() -> BnNetLoot {
    *shared().loot.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn ui_giver() -> BnNetGiver {
    *shared().giver.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn ui_vendor() -> BnNetVendor {
    *shared().vendor.lock().unwrap_or_else(|e| e.into_inner())
}

/// `CloseMerchant()` (`0x4fadf0`) sends no packet, but the open vendor is what makes a bag
/// right-click a *sale* — so the close has to clear the middle layer's stock window too, or the
/// host would keep re-pushing its rows into a window the UI thinks is shut.
pub(crate) fn ui_vendor_closed() {
    let mut win = shared().vendor.lock().unwrap_or_else(|e| e.into_inner());
    win.guid = 0;
    win.count = 0;
}

/// The pending resurrection offer, as the death dialog's state.
pub(crate) struct UiResurrect {
    pub caster: u64,
    pub sickness: bool,
    pub has_timer: bool,
}

pub(crate) fn ui_resurrect() -> Option<UiResurrect> {
    let sh = shared();
    let caster = sh.resurrect_from.load(Ordering::SeqCst);
    (caster != 0).then(|| UiResurrect {
        caster,
        sickness: sh.resurrect_sickness.load(Ordering::SeqCst) != 0,
        has_timer: sh.resurrect_has_timer.load(Ordering::SeqCst) != 0,
    })
}

pub(crate) fn ui_spirit_healer() -> u64 {
    shared().spirit_healer.load(Ordering::SeqCst)
}

pub(crate) fn ui_corpse_delay_ms() -> u32 {
    shared().corpse_delay_ms.load(Ordering::SeqCst)
}

/// Whether our own guid is in melee auto-attack (`SMSG_ATTACKSTART`).
pub(crate) fn ui_self_attacking() -> bool {
    shared().self_attacking.load(Ordering::SeqCst) != 0
}

/// The guid we last started auto-attack on, 0 when none.
pub(crate) fn ui_attack_target() -> u64 {
    shared().attack_target.load(Ordering::SeqCst)
}

/// `SelectTarget` (`0x493540`): the selection is client memory, and `CMSG_SET_SELECTION`
/// is only its wire echo, so record it at the call — a script's `TargetUnit(…)` followed by
/// `AttackTarget()` then reads the new selection in the same frame.
pub(crate) fn select(guid: u64) {
    shared().selection.store(guid, Ordering::SeqCst);
    let _ = send_cmd(Cmd::Target(guid));
}

/// `StopAttack` (`0x5ecac0`): clear our attack state at once and send `CMSG_ATTACKSTOP`
/// only when we were attacking, the reference's own guard. The server's `SMSG_ATTACKSTOP`
/// echo is then only confirmation.
pub(crate) fn stop_attack() {
    let sh = shared();
    let engaged = sh.self_attacking.swap(0, Ordering::SeqCst) != 0;
    sh.attack_target.store(0, Ordering::SeqCst);
    if engaged {
        let _ = send_cmd(Cmd::StopAttack);
    }
}

/// Latch a melee refusal (`0x5ecdb0`): due now, so the next tick shows it.
pub(crate) fn latch_swing_refusal(code: u8) {
    let sh = shared();
    *sh.swing_refusal.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((code, std::time::Instant::now()));
}

/// `0x5ecdb0(0)`: a landed swing clears the latch.
pub(crate) fn clear_swing_refusal() {
    let sh = shared();
    *sh.swing_refusal.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The refusal line due now, re-armed four seconds later (`0x5ec950`/`0x5ec9b6`).
pub(crate) fn take_due_swing_refusal() -> Option<u8> {
    let sh = shared();
    let mut latch = sh.swing_refusal.lock().unwrap_or_else(|e| e.into_inner());
    let (code, due) = latch.as_mut()?;
    let now = std::time::Instant::now();
    if now < *due {
        return None;
    }
    *due = now + std::time::Duration::from_millis(0xfa0);
    Some(*code)
}

/// Our body's main-hand swing clip, resolved at the last attack start.
pub(crate) fn self_swing_anim() -> u32 {
    shared().self_swing_anim.load(Ordering::SeqCst)
}

/// Re-resolve [`self_swing_anim`] from the equipped main hand (the local swing
/// press asks for it too, so a training swing is the equipped weapon's clip).
pub(crate) fn refresh_self_swing_anim() {
    let sh = shared();
    store_self_swing_anim(sh, self_main_swing(sh));
}

/// Store the resolved clip; a change is announced with the inputs that decided
/// it, so a swing that drops to unarmed under a drawn weapon names its own
/// cause in the host log (`swing|self|<clip>|<entry>|<class>|<subclass>`).
fn store_self_swing_anim(sh: &Shared, clip: u16) {
    let clip = u32::from(clip);
    if sh.self_swing_anim.swap(clip, Ordering::SeqCst) == clip {
        return;
    }
    let hand = self_main_hand(sh);
    sh.event(format!(
        "swing|self|{}|{}|{}|{}",
        clip, hand.worn, hand.class, hand.subclass
    ));
}

/// Our own descriptor as the update carried it.
pub(crate) fn ui_self_fields() -> Option<ObjectFields> {
    let guid = ui_self_guid();
    ui_unit(guid).map(|u| u.fields)
}

/// The creature visual the server rides us on (`UNIT_FIELD_MOUNTDISPLAYID`),
/// 0 standing.
pub(crate) fn ui_self_mount_display() -> u32 {
    ui_self_fields().map_or(0, |f| f.unit_mount_display_id())
}

/// Our own cast in flight: `(spell id, elapsed seconds, duration seconds)`,
/// straight off the `SMSG_SPELL_START` the server sent.
pub(crate) fn ui_self_cast() -> Option<(u32, f32, f32)> {
    let guid = ui_self_guid();
    if guid == 0 {
        return None;
    }
    let sh = shared();
    let casts = sh.casts.lock().unwrap_or_else(|e| e.into_inner());
    let cast = casts.get(&guid)?;
    Some((
        cast.spell_id,
        cast.started.elapsed().as_secs_f32(),
        cast.duration_ms as f32 / 1000.0,
    ))
}

/// The live group roster, cloned.
pub(crate) fn ui_party() -> BnNetParty {
    *shared().party.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every occupied inventory slot, in client order.
pub(crate) fn ui_inventory() -> Vec<BnNetInvItem> {
    inventory_rows(shared())
}

/// The player's own bag containers: the backpack (id 0, 16 slots, no bag item)
/// then each equipped bag slot 19..=22 as containers 1..=4. `entry` is the bag
/// item's template entry (0 for the backpack) and `guid` its item object, which
/// the reference's watcher diffs to fire `BAG_CLOSED` (`0x4f923d`).
pub(crate) fn ui_bags() -> Vec<(i64, u32, u32, u64)> {
    let sh = shared();
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<(i64, u32, u32, u64)> = Vec::new();
    if self_guid == 0 {
        return out;
    }
    let Some(player) = units.get(&self_guid) else {
        return out;
    };
    let f = &player.fields;
    out.push((0, 16, 0, 0));
    for bag_slot in 19..23u8 {
        let Some(guid) = f.player_inv_slot(bag_slot) else {
            continue;
        };
        if guid == 0 {
            continue;
        }
        let Some(rec) = units.get(&guid) else {
            continue;
        };
        let n = rec.fields.container_num_slots().unwrap_or(0).min(36);
        out.push((
            i64::from(bag_slot) - 18,
            n,
            rec.fields.object_entry().unwrap_or(0),
            guid,
        ));
    }
    out
}

/// Every tracked object's guid, sorted.
pub(crate) fn ui_unit_guids() -> Vec<u64> {
    let units = shared().units.lock().unwrap_or_else(|e| e.into_inner());
    let mut guids: Vec<u64> = units.keys().copied().collect();
    guids.sort_unstable();
    guids
}

/// Every tracked unit's `UNIT_FIELD_TARGET`, for the token resolver's chains.
pub(crate) fn ui_held_targets() -> HashMap<u64, u64> {
    let units = shared().units.lock().unwrap_or_else(|e| e.into_inner());
    units
        .iter()
        .filter_map(|(guid, rec)| rec.fields.unit_target().map(|t| (*guid, t)))
        .collect()
}

/// [`Shared::quest_status`]'s current answer for one guid.
pub(crate) fn ui_quest_status(guid: u64) -> Option<u32> {
    let statuses = shared()
        .quest_status
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    statuses.get(&guid).copied()
}

/// [`Shared::taxi_known`]'s current answer for one guid (absent = no answer yet).
pub(crate) fn ui_taxi_known(guid: u64) -> Option<bool> {
    let known = shared()
        .taxi_known
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    known.get(&guid).copied()
}

/// The re-ask counter [`Shared::quest_epoch`].
pub(crate) fn ui_quest_epoch() -> u64 {
    shared().quest_epoch.load(Ordering::SeqCst)
}

/// Drop every cached `SMSG_QUESTGIVER_STATUS` / `SMSG_TAXINODE_STATUS` answer whose object has
/// left the stream window, as the app's `QuestGiver::retain_statuses` does on its guid index.
pub(crate) fn retain_quest_statuses(alive: &HashSet<u64>) {
    let sh = shared();
    sh.quest_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|guid, _| alive.contains(guid));
    sh.taxi_known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|guid, _| alive.contains(guid));
}

/// Drop one guid's cached marker answers, the reference's teardown (`0x6073f0`): the quest status
/// and the flight master's green share the one overhead slot, so both go together.
pub(crate) fn clear_quest_status(guid: u64) {
    let sh = shared();
    sh.quest_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&guid);
    sh.taxi_known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&guid);
}

/// Queue the two overhead-marker asks (`0x182` / `0x1aa`) for `npc`.
pub(crate) fn send_quest_query(npc: u64) {
    let _ = send_cmd(Cmd::QuestgiverStatusQuery(npc));
}

pub(crate) fn send_taxi_query(npc: u64) {
    let _ = send_cmd(Cmd::TaxiNodeStatusQuery(npc));
}

fn copy_name<const N: usize>(dst: &mut [c_char; N], name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (i, b) in bytes[..n].iter().enumerate() {
        dst[i] = *b as c_char;
    }
    dst[n] = 0;
}

/// Read a fixed NUL-terminated name buffer back into a `String`.
fn name_of<const N: usize>(src: &[c_char; N]) -> String {
    let bytes: Vec<u8> = src
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---- worker ----

/// How the worker treats the realm list. `Auto` is the headless host path: it
/// world-connects the first realm with characters and stops at the roster.
/// `Glue` is the in-game menu path: it publishes the realm list and waits for
/// the UI's `RealmPick`, keeping the realmd link for refreshes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunMode {
    Auto,
    Glue,
}

fn worker(
    host: String,
    user: String,
    pass: String,
    rx: std::sync::mpsc::Receiver<Cmd>,
    mode: RunMode,
) {
    let sh = shared();
    sh.set(ST_CONNECTING);
    sh.event(format!("connect|{host}"));
    *sh.creds.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((host.clone(), user.clone(), pass.clone()));

    let mut logon = match benilla_protocol::logon(&host, &user, &pass, 0) {
        Ok(l) => l,
        Err(e) => {
            sh.event(format!("error|logon|{e}"));
            *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("logon: {e}"));
            sh.set(ST_FAILED);
            return;
        }
    };
    sh.set(ST_LOGON_OK);
    sh.event(format!("logon|ok|{} realms", logon.realms.len()));
    *sh.realms.lock().unwrap_or_else(|e| e.into_inner()) = logon.realms.clone();
    sh.realms_epoch.fetch_add(1, Ordering::SeqCst);
    *sh.session_key.lock().unwrap_or_else(|e| e.into_inner()) = logon.session_key;

    let mut session: Option<WorldSession> = None;
    if mode == RunMode::Auto {
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
            *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("no realms on this account".to_string());
            sh.set(ST_FAILED);
            return;
        };
        sh.event(format!(
            "realm|{}|{}|{} chars",
            realm.name, realm.address, realm.characters
        ));

        sh.set(ST_WORLD_CONNECTING);
        match WorldSession::connect(&realm.address, &user, logon.session_key) {
            Ok(s) => {
                session = Some(s);
                // The held realmd socket is only needed for realm-list refreshes; free it.
                logon.close();
            }
            Err(e) => {
                sh.event(format!("error|world|{e}"));
                *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("world: {e}"));
                sh.set(ST_FAILED);
                return;
            }
        }
        match roster(session.as_mut().expect("session"), sh) {
            Ok(n) => sh.event(format!("chars|{n}")),
            Err(e) => {
                sh.event(format!("error|char_enum|{e}"));
                *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("char_enum: {e}"));
                sh.set(ST_FAILED);
                return;
            }
        }
    } else {
        // Glue: the UI's realm list reads `sh.realms`; hold the logon so a
        // refresh stays possible, and wait for the picked realm.
        sh.event("realms|ready");
        loop {
            match rx.recv() {
                Ok(Cmd::RealmPick(index)) => {
                    let realm = {
                        let realms = sh.realms.lock().unwrap_or_else(|e| e.into_inner());
                        realms.get(index as usize).cloned()
                    };
                    let Some(realm) = realm else {
                        sh.event(format!("error|realm|{index} out of range"));
                        continue;
                    };
                    sh.event(format!(
                        "realm|{}|{}|{} chars",
                        realm.name, realm.address, realm.characters
                    ));
                    sh.set(ST_WORLD_CONNECTING);
                    let key = *sh.session_key.lock().unwrap_or_else(|e| e.into_inner());
                    match WorldSession::connect(&realm.address, &user, key) {
                        Ok(mut s) => match roster(&mut s, sh) {
                            Ok(n) => {
                                sh.event(format!("chars|{n}"));
                                logon.close();
                                session = Some(s);
                                break;
                            }
                            Err(e) => {
                                sh.event(format!("error|char_enum|{e}"));
                                *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(format!("char_enum: {e}"));
                                sh.set(ST_LOGON_OK);
                            }
                        },
                        Err(e) => {
                            sh.event(format!("error|world|{e}"));
                            *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(format!("world: {e}"));
                            sh.set(ST_LOGON_OK);
                        }
                    }
                }
                Ok(Cmd::Logout) | Err(_) => {
                    sh.event("closed|roster");
                    logon.close();
                    sh.set(ST_IDLE);
                    return;
                }
                Ok(_) => {}
            }
        }
    }
    let mut session = session.expect("session");

    // Wait for the host to choose a character. The socket stays authenticated;
    // no read deadline is set, so an idle roster does not time out.
    loop {
        match rx.recv() {
            Ok(Cmd::Enter(guid)) => {
                sh.self_guid.store(guid, Ordering::SeqCst);
                sh.units.lock().unwrap_or_else(|e| e.into_inner()).clear();
                crate::gossip_vm::clear_session();
                crate::quest_log_vm::clear_session();
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
                        if mode == RunMode::Glue {
                            *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(format!("player_login: {e}"));
                            sh.set(ST_CHARS);
                            continue;
                        }
                        sh.set(ST_FAILED);
                    }
                }
                return;
            }
            Ok(Cmd::RefreshRoster) => match roster(&mut session, sh) {
                Ok(n) => sh.event(format!("chars|{n}")),
                Err(e) => {
                    sh.event(format!("error|char_enum|{e}"));
                    if mode == RunMode::Glue {
                        *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("char_enum: {e}"));
                        sh.set(ST_CHARS);
                        continue;
                    }
                    sh.set(ST_FAILED);
                    return;
                }
            },
            Ok(Cmd::CharCreateReq(req)) => {
                match session.create_character(&req) {
                    Ok(code) => {
                        // A success streams a fresh roster; a refusal is the
                        // name-in-use / limit byte the host's dialog shows.
                        sh.event(format!("char_create|{}|{code}", req.name));
                        *sh.create_reply.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((req.name.clone(), code));
                        if code == 0x2E {
                            if let Err(e) = roster(&mut session, sh) {
                                sh.event(format!("error|char_enum|{e}"));
                            }
                        }
                    }
                    Err(e) => {
                        sh.event(format!("error|char_create|{e}"));
                        *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("char_create: {e}"));
                    }
                }
            }
            Ok(Cmd::CharDelete(guid)) => {
                match session.delete_character(guid) {
                    Ok(code) => {
                        sh.event(format!("char_delete|{guid}|{code}"));
                        *sh.delete_reply.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((guid, code));
                        // A success takes the row out of the roster the screen shows.
                        if code == 0x39 {
                            if let Err(e) = roster(&mut session, sh) {
                                sh.event(format!("error|char_enum|{e}"));
                            }
                        }
                    }
                    Err(e) => {
                        sh.event(format!("error|char_delete|{e}"));
                        *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("char_delete: {e}"));
                    }
                }
            }
            Ok(Cmd::CharRename { guid, name }) => match session.rename_character(guid, &name) {
                Ok((code, renamed)) => {
                    if let Some((g, n)) = renamed {
                        sh.event(format!("char_rename|{g}|{n}|{code}"));
                    } else {
                        sh.event(format!("char_rename|{guid}|{code}"));
                    }
                    *sh.rename_reply.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some((guid, name.clone(), code));
                    if code == 0x2E {
                        if let Err(e) = roster(&mut session, sh) {
                            sh.event(format!("error|char_enum|{e}"));
                        }
                    }
                }
                Err(e) => {
                    sh.event(format!("error|char_rename|{e}"));
                    *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("char_rename: {e}"));
                }
            },
            Ok(Cmd::RealmPick(index)) => {
                let realm = {
                    let realms = sh.realms.lock().unwrap_or_else(|e| e.into_inner());
                    realms.get(index as usize).cloned()
                };
                let Some(realm) = realm else {
                    sh.event(format!("error|realm|{index} out of range"));
                    continue;
                };
                sh.event(format!(
                    "realm|{}|{}|{} chars",
                    realm.name, realm.address, realm.characters
                ));
                // World-connect on the chosen address; the old session's socket
                // closes when `session` is replaced below.
                sh.set(ST_WORLD_CONNECTING);
                let key = *sh.session_key.lock().unwrap_or_else(|e| e.into_inner());
                match WorldSession::connect(&realm.address, &user, key) {
                    Ok(s) => {
                        session = s;
                        if let Err(e) = roster(&mut session, sh) {
                            sh.event(format!("error|char_enum|{e}"));
                            if mode == RunMode::Glue {
                                sh.set(ST_LOGON_OK);
                                continue;
                            }
                            sh.set(ST_FAILED);
                            return;
                        }
                    }
                    Err(e) => {
                        sh.event(format!("error|world|{e}"));
                        if mode == RunMode::Glue {
                            *sh.menu_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(format!("world: {e}"));
                            sh.set(ST_LOGON_OK);
                            continue;
                        }
                        sh.set(ST_FAILED);
                        return;
                    }
                }
            }
            Ok(Cmd::LogoutToRoster) => {
                match session.logout(Duration::from_secs(2)) {
                    Ok(()) => {
                        // `SMSG_LOGOUT_COMPLETE`: the link is back at the park;
                        // the roster is re-asked so the host's screen refreshes.
                        sh.event("logout|roster");
                        *sh.player.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        if let Err(e) = roster(&mut session, sh) {
                            sh.event(format!("error|char_enum|{e}"));
                            sh.set(ST_FAILED);
                            return;
                        }
                    }
                    Err(e) => sh.event(format!("error|logout|{e}")),
                }
            }
            Ok(Cmd::Logout) | Err(_) => {
                sh.event("closed|roster");
                sh.set(ST_IDLE);
                return;
            }
            Ok(_) => {} // world commands before entering are meaningless
        }
    }
}

/// Ask `CMSG_CHAR_ENUM` and fill the shared roster; returns its row count.
fn roster(
    session: &mut WorldSession,
    sh: &Shared,
) -> std::result::Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let chars = session.char_enum()?;
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
                skin: c.skin,
                face: c.face,
                hair_style: c.hair_style,
                hair_color: c.hair_color,
                facial_hair: c.facial_hair,
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
    sh.chars_epoch.fetch_add(1, Ordering::SeqCst);
    Ok(chars.len())
}

/// Pump the world stream: one `recv` with a short deadline per loop so queued
/// commands (chat, attack, loot) go out promptly even when the server is quiet.
fn run_world(session: &mut WorldSession, rx: &std::sync::mpsc::Receiver<Cmd>, sh: &Shared) {
    let _ = session.set_read_timeout(Some(std::time::Duration::from_millis(200)));

    // A fresh world session owns fresh markers: the previous session's answers name objects that
    // are gone, as the app's cached statuses die with their entities.
    sh.quest_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    sh.taxi_known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    sh.quest_epoch.fetch_add(1, Ordering::SeqCst);

    // Client tick for MovementInfo timestamps: matches benilla-protocol's
    // `world::movement::client_uptime_ms` (pub(super), so we mirror it here).
    fn uptime_ms() -> u32 {
        use std::sync::OnceLock;
        static START: OnceLock<Instant> = OnceLock::new();
        (START.get_or_init(|| Instant::now()).elapsed().as_millis() as u32).max(1)
    }

    // Movement state machine: mirror the host's directional input to emit the
    // right MSG_MOVE_* opcode edges vmangos expects. Without StartForward the
    // server would just drop every heartbeat and rubber-band us back on the
    // next object update.
    const HB_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut next_hb = std::time::Instant::now();
    let mut prev_input: (f32, f32, f32) = (0.0, 0.0, 0.0);
    let mut last_move_flags: u32 = 0;
    let mut last_pose_sent: Option<([f32; 3], f32)> = None;

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
                    sh.attack_target.store(guid, Ordering::SeqCst);
                    // `start_attack_local`'s tail (`0x5ecd78`): a melee start snaps the
                    // weapon out of its sheath, so our own `UNIT_FIELD_BYTES_2` sheath
                    // byte reads drawn to everyone who sees us, not only the anim.
                    let _ = session.set_sheathed(1);
                }
                Ok(Cmd::StopAttack) => {
                    let _ = session.attack_stop();
                    sh.attack_target.store(0, Ordering::SeqCst);
                }
                Ok(Cmd::QuestgiverStatusQuery(npc)) => {
                    let _ = session.questgiver_status_query(npc);
                }
                Ok(Cmd::TaxiNodeStatusQuery(npc)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_TAXINODE_STATUS_QUERY,
                        &benilla_protocol::messages::taxi_node_status_query(npc),
                    );
                }
                Ok(Cmd::Target(guid)) => {
                    // The wire has no selection echo; record it here so the target
                    // frame and `PLAYER_TARGET_CHANGED` can follow the pick.
                    sh.selection.store(guid, Ordering::SeqCst);
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
                Ok(Cmd::ReclaimCorpse(corpse)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_RECLAIM_CORPSE,
                        &benilla_protocol::messages::reclaim_corpse(corpse),
                    );
                }
                Ok(Cmd::ResurrectResponse { caster, accept }) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_RESURRECT_RESPONSE,
                        &benilla_protocol::messages::resurrect_response(caster, accept),
                    );
                    sh.resurrect_from.store(0, Ordering::SeqCst);
                }
                Ok(Cmd::SpiritHealer(npc)) => {
                    let _ = session.spirit_healer_activate(npc);
                    sh.spirit_healer.store(0, Ordering::SeqCst);
                }
                Ok(Cmd::SelfRes) => {
                    let _ = session.self_res();
                }
                Ok(Cmd::GossipSelect { npc, index }) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_GOSSIP_SELECT_OPTION,
                        &benilla_protocol::messages::gossip_select_option(npc, index, None),
                    );
                }
                Ok(Cmd::NpcTextQuery { text_id, npc }) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_NPC_TEXT_QUERY,
                        &benilla_protocol::messages::npc_text_query(text_id, npc),
                    );
                }
                Ok(Cmd::GossipQuest { npc, quest }) => {
                    let _ = session.questgiver_query_quest(npc, quest);
                }
                Ok(Cmd::QuestAbandon { slot }) => {
                    let _ = session.questlog_remove_quest(slot);
                }
                Ok(Cmd::SetAction { slot, packed }) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_SET_ACTION_BUTTON,
                        &benilla_protocol::messages::set_action_button(slot, packed),
                    );
                    // The bar is client-authoritative and the server never echoes the write, so the
                    // snapshot the feed diffs takes it here (`ui_action/drain.rs`'s store write): a
                    // dragged spell stays put through the next feed instead of being rolled back to
                    // the login snapshot.
                    let mut actions = sh.actions.lock().unwrap_or_else(|e| e.into_inner());
                    let at = actions.iter().position(|(s, _, _)| *s == slot);
                    if packed == 0 {
                        if let Some(i) = at {
                            actions.remove(i);
                        }
                    } else {
                        let write = (slot, (packed >> 24) as u8, packed & 0x00ff_ffff);
                        match at {
                            Some(i) => actions[i] = write,
                            None => actions.push(write),
                        }
                        actions.sort_by_key(|s| s.0);
                    }
                }
                Ok(Cmd::ActionBarToggles(toggles)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_SET_ACTIONBAR_TOGGLES,
                        &benilla_protocol::messages::set_actionbar_toggles(toggles),
                    );
                }
                Ok(Cmd::TogglePvp) => {
                    let _ =
                        session.send_body(benilla_protocol::messages::opcode::CMSG_TOGGLE_PVP, &[]);
                }
                Ok(Cmd::ToggleHelm) => {
                    let _ = session
                        .send_body(benilla_protocol::messages::opcode::CMSG_TOGGLE_HELM, &[]);
                }
                Ok(Cmd::ToggleCloak) => {
                    let _ = session
                        .send_body(benilla_protocol::messages::opcode::CMSG_TOGGLE_CLOAK, &[]);
                }
                Ok(Cmd::ItemQuery(entry)) => {
                    let _ = session.item_query(entry, 0);
                }
                Ok(Cmd::LearnTalent { talent_id, rank }) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_LEARN_TALENT,
                        &benilla_protocol::messages::learn_talent(talent_id, rank),
                    );
                }
                Ok(Cmd::ChatKind { kind, target, msg }) => {
                    if let Err(e) = session.send_chat_kind(kind, target.as_deref(), &msg) {
                        sh.event(format!("error|chat|{e}"));
                    }
                }
                Ok(Cmd::GiverHello(npc)) => {
                    let _ = session.questgiver_hello(npc);
                }
                Ok(Cmd::GossipHello(npc)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_GOSSIP_HELLO,
                        &benilla_protocol::messages::gossip_hello(npc),
                    );
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
                Ok(Cmd::CancelCast(spell)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_CANCEL_CAST,
                        &spell.to_le_bytes(),
                    );
                }
                Ok(Cmd::CancelAura(spell)) => {
                    let _ = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_CANCEL_AURA,
                        &spell.to_le_bytes(),
                    );
                }
                Ok(Cmd::UseItem { bag, slot, spell }) => {
                    if let Err(e) = session.use_item(bag, slot, spell) {
                        sh.event(format!("error|use_item|{e}"));
                    }
                }
                Ok(Cmd::AutoEquip { bag, slot }) => {
                    // A refused auto-equip is otherwise silent on the host's side, so the choice
                    // this handler makes is traced: `equip|auto|<bag>/<slot>|target=<live,entry>`.
                    let target = equip_target(sh, bag, slot);
                    note(format!("equip|auto|{bag}/{slot}|target={target:?}"));
                    // VMaNGOS 1.12 answers `CMSG_AUTOEQUIP_ITEM` with `ERR_ITEM_NOT_FOUND` for
                    // the very item it names (probe 2026-10-29: 255/26 refused, then equipped by
                    // `CMSG_SWAP_INV_ITEM` 26→4), so the destination is picked here — the
                    // template's static mask, first empty candidate, as the server's own
                    // auto-equip would — and sent as the drag gesture's swap: both ends in the
                    // player's array is `CMSG_SWAP_INV_ITEM`, an item inside a bag is
                    // `CMSG_SWAP_ITEM`. The displaced piece comes back to the source slot. Ammo
                    // (live 0) loads by entry and stays in its bag (`0x5e1704`). A template that
                    // has not answered keeps the client's own packet.
                    match target {
                        Some((0, entry)) => {
                            let body = benilla_protocol::messages::set_ammo(entry);
                            if let Err(e) = session
                                .send_body(benilla_protocol::messages::opcode::CMSG_SET_AMMO, &body)
                            {
                                sh.event(format!("error|set_ammo|{e}"));
                            }
                        }
                        Some((live, _)) => {
                            let dst = live - 1;
                            let sent = if bag == BAG_EQUIPPED {
                                session.swap_inv_item(slot, dst)
                            } else {
                                let body = benilla_protocol::messages::swap_item(
                                    BAG_EQUIPPED,
                                    dst,
                                    bag,
                                    slot,
                                );
                                session.send_body(
                                    benilla_protocol::messages::opcode::CMSG_SWAP_ITEM,
                                    &body,
                                )
                            };
                            if let Err(e) = sent {
                                sh.event(format!("error|equip|{e}"));
                            }
                        }
                        None => {
                            let (row_bag, row_slot) = row_pos_for_wire(bag, slot);
                            let invt = inventory_rows(sh)
                                .into_iter()
                                .find(|r| r.bag == row_bag && r.slot == row_slot)
                                .map(|r| r.info.inventory_type);
                            note(format!(
                                "equip|no-slot|{bag}/{slot}|row={row_bag}/{row_slot}|invtype={invt:?}"
                            ));
                            if let Err(e) = session.auto_equip_item(bag, slot) {
                                sh.event(format!("error|equip|{e}"));
                            }
                        }
                    }
                }
                Ok(Cmd::SwapInv { src, dst }) => {
                    if let Err(e) = session.swap_inv_item(src, dst) {
                        sh.event(format!("error|swap|{e}"));
                    }
                }
                Ok(Cmd::SwapItem {
                    dst_bag,
                    dst_slot,
                    src_bag,
                    src_slot,
                }) => {
                    let body =
                        benilla_protocol::messages::swap_item(dst_bag, dst_slot, src_bag, src_slot);
                    if let Err(e) =
                        session.send_body(benilla_protocol::messages::opcode::CMSG_SWAP_ITEM, &body)
                    {
                        sh.event(format!("error|swap_item|{e}"));
                    }
                }
                Ok(Cmd::SplitItem {
                    src_bag,
                    src_slot,
                    dst_bag,
                    dst_slot,
                    count,
                }) => {
                    let body = benilla_protocol::messages::split_item(
                        src_bag, src_slot, dst_bag, dst_slot, count,
                    );
                    if let Err(e) = session
                        .send_body(benilla_protocol::messages::opcode::CMSG_SPLIT_ITEM, &body)
                    {
                        sh.event(format!("error|split_item|{e}"));
                    }
                }
                Ok(Cmd::AutoStore {
                    src_bag,
                    src_slot,
                    dst_bag,
                }) => {
                    let body =
                        benilla_protocol::messages::auto_store_bag_item(src_bag, src_slot, dst_bag);
                    if let Err(e) = session.send_body(
                        benilla_protocol::messages::opcode::CMSG_AUTOSTORE_BAG_ITEM,
                        &body,
                    ) {
                        sh.event(format!("error|autostore|{e}"));
                    }
                }
                Ok(Cmd::DestroyItem { bag, slot, count }) => {
                    let body = benilla_protocol::messages::destroy_item(bag, slot, count);
                    if let Err(e) = session
                        .send_body(benilla_protocol::messages::opcode::CMSG_DESTROYITEM, &body)
                    {
                        sh.event(format!("error|destroy_item|{e}"));
                    }
                }
                Ok(Cmd::SetAmmo(entry)) => {
                    let body = benilla_protocol::messages::set_ammo(entry);
                    if let Err(e) =
                        session.send_body(benilla_protocol::messages::opcode::CMSG_SET_AMMO, &body)
                    {
                        sh.event(format!("error|set_ammo|{e}"));
                    }
                }
                Ok(Cmd::GroupInvite(name)) => {
                    if let Err(e) = session.group_invite(&name) {
                        sh.event(format!("error|group|{e}"));
                    }
                }
                Ok(Cmd::GroupAccept) => {
                    // Close the popup immediately: the roster follows on
                    // SMSG_GROUP_LIST (and a party of one carries no rows).
                    sh.party.lock().unwrap_or_else(|e| e.into_inner()).invite = 0;
                    if let Err(e) = session.group_accept() {
                        sh.event(format!("error|group|{e}"));
                    }
                }
                Ok(Cmd::GroupDecline) => {
                    sh.party.lock().unwrap_or_else(|e| e.into_inner()).invite = 0;
                    if let Err(e) = session.group_decline() {
                        sh.event(format!("error|group|{e}"));
                    }
                }
                Ok(Cmd::GroupDisband) => {
                    if let Err(e) = session.group_disband() {
                        sh.event(format!("error|group|{e}"));
                    }
                }
                Ok(Cmd::VendorList(vendor)) => {
                    sh.vendor.lock().unwrap_or_else(|e| e.into_inner()).guid = 0;
                    if let Err(e) = session.list_inventory(vendor) {
                        sh.event(format!("error|vendor|list|{e}"));
                    }
                }
                Ok(Cmd::VendorBuy {
                    vendor,
                    entry,
                    count,
                }) => {
                    if let Err(e) = session.buy_item(vendor, entry, count) {
                        sh.event(format!("error|vendor|buy|{e}"));
                    }
                }
                Ok(Cmd::VendorSell {
                    vendor,
                    item_guid,
                    count,
                }) => {
                    if let Err(e) = session.sell_item(vendor, item_guid, count) {
                        sh.event(format!("error|vendor|sell|{e}"));
                    }
                }
                Ok(Cmd::VendorBuyInSlot {
                    vendor,
                    entry,
                    bag_guid,
                    bag_slot,
                    count,
                }) => {
                    if let Err(e) =
                        session.buy_item_in_slot(vendor, entry, bag_guid, bag_slot, count)
                    {
                        sh.event(format!("error|vendor|buyslot|{e}"));
                    }
                }
                Ok(Cmd::VendorBuyback { vendor, slot }) => {
                    if let Err(e) = session.buyback_item(vendor, slot) {
                        sh.event(format!("error|vendor|buyback|{e}"));
                    }
                }
                Ok(Cmd::VendorRepair { vendor, item_guid }) => {
                    if let Err(e) = session.repair_item(vendor, item_guid) {
                        sh.event(format!("error|vendor|repair|{e}"));
                    }
                    sh.event(format!("vendor|repair|{vendor}|{item_guid}"));
                }
                Ok(Cmd::Logout) | Err(TryRecvError::Disconnected) => {
                    let _ = session.logout(std::time::Duration::from_secs(2));
                    sh.event("closed|world");
                    sh.set(ST_IDLE);
                    return;
                }
                // Park-only verbs: meaningless in-world, so no-ops here.
                Ok(Cmd::CharCreateReq(_))
                | Ok(Cmd::CharDelete(_))
                | Ok(Cmd::CharRename { .. })
                | Ok(Cmd::RefreshRoster)
                | Ok(Cmd::RealmPick(_))
                | Ok(Cmd::LogoutToRoster)
                | Ok(Cmd::Enter(_)) => {}
                Err(TryRecvError::Empty) => break,
            }
        }

        // Mover claim: vmangos discards MSG_MOVE_* before we claim our guid
        // as the active mover (MovementHandler.cpp:291-293). Only send once,
        // after the server has actually streamed our own UpdateObject -
        // applying that object flips mover_ready from 0 to 1.
        if sh.mover_ready.load(Ordering::SeqCst) == 1 {
            let guid = sh.self_guid.load(Ordering::SeqCst);
            if guid != 0 {
                // Use compare_exchange_weak so only the first tick flips it.
                let claimed =
                    sh.mover_ready
                        .compare_exchange_weak(1, 2, Ordering::SeqCst, Ordering::SeqCst);
                if claimed.is_ok() {
                    if let Err(e) = session.set_active_mover(guid) {
                        sh.event(format!("error|set_active_mover|{e}"));
                    } else {
                        sh.event(format!("move|set_active_mover|{guid}"));
                    }
                    // Note: no worldport_ack here — MSG_MOVE_WORLDPORT_ACK is only
                    // owed after SMSG_NEW_WORLD (map transfer), not at login.
                }
            }
        }

        // Movement state machine: emit MSG_MOVE_START_* / STOP_* edges on
        // input transitions, heartbeat every 500 ms while moving, and a
        // zero-flags heartbeat if the body drifted at rest.
        const MF_FORWARD: u32 = 0x1;
        const MF_BACKWARD: u32 = 0x2;
        const MF_STRAFE_LEFT: u32 = 0x4;
        const MF_STRAFE_RIGHT: u32 = 0x8;
        let input = {
            let g = sh.last_input.lock().unwrap_or_else(|e| e.into_inner());
            *g
        };
        let pose = {
            let g = sh.last_pose.lock().unwrap_or_else(|e| e.into_inner());
            *g
        };

        if let Some((pos, yaw)) = pose {
            // Our own body is client-authoritative: the server never relays our `MSG_MOVE_*` back,
            // so the self record only holds the login pose. Without this the host's and the VM's
            // distance tests measure from where we logged in, not where we stand.
            let self_guid = sh.self_guid.load(Ordering::SeqCst);
            if let Some(rec) = sh
                .units
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&self_guid)
            {
                rec.pos = pos;
                rec.facing = yaw;
            }
            let forward = input.0;
            let strafe = input.1;
            let moving_fwd = forward > 0.1;
            let moving_bwd = forward < -0.1;
            let strafe_r = strafe > 0.1;
            let strafe_l = strafe < -0.1;

            let prev_fwd = prev_input.0 > 0.1;
            let prev_bwd = prev_input.0 < -0.1;
            let prev_sr = prev_input.1 > 0.1;
            let prev_sl = prev_input.1 < -0.1;

            // Compute current flags from input.
            let cur_flags: u32 = (if moving_fwd { MF_FORWARD } else { 0 })
                | (if moving_bwd { MF_BACKWARD } else { 0 })
                | (if strafe_r { MF_STRAFE_RIGHT } else { 0 })
                | (if strafe_l { MF_STRAFE_LEFT } else { 0 });

            // Use session.send_movement so MovementInfo construction and serialization
            // exactly match WorldWriter.send_movement (no manual struct build + send_body).
            let now_orient = yaw.rem_euclid(std::f32::consts::TAU);
            let mut send_move = |opcode: u16, flags: u32| -> bool {
                match session.send_movement(opcode, flags, pos, now_orient, 0.0, 0, None, None) {
                    Ok(()) => true,
                    Err(e) => {
                        sh.event(format!("error|move|opcode={opcode:#x}|{e}"));
                        false
                    }
                }
            };

            let mut sent_flags: u32 = 0;

            // Forward edge: MSG_MOVE_START_FORWARD on rising edge, MSG_MOVE_STOP on both
            // forward and backward falling (server clears all motion flags on STOP).
            if moving_fwd && !prev_fwd && !prev_bwd {
                send_move(MOVE_OP::MSG_MOVE_START_FORWARD, MF_FORWARD);
                sent_flags |= MF_FORWARD;
                sh.event(format!(
                    "move|StartForward|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }
            if moving_bwd && !prev_bwd && !prev_fwd {
                send_move(MOVE_OP::MSG_MOVE_START_BACKWARD, MF_BACKWARD);
                sent_flags |= MF_BACKWARD;
                sh.event(format!(
                    "move|StartBackward|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }
            if !moving_fwd && !moving_bwd && (prev_fwd || prev_bwd) {
                // STOP clears FORWARD/BACKWARD on the server.
                send_move(MOVE_OP::MSG_MOVE_STOP, 0);
                sh.event(format!(
                    "move|Stop|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }

            // Strafe edges.
            if strafe_r && !prev_sr && !prev_sl {
                send_move(MOVE_OP::MSG_MOVE_START_STRAFE_RIGHT, MF_STRAFE_RIGHT);
                sent_flags |= MF_STRAFE_RIGHT;
                sh.event(format!(
                    "move|StartStrafeRight|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }
            if strafe_l && !prev_sl && !prev_sr {
                send_move(MOVE_OP::MSG_MOVE_START_STRAFE_LEFT, MF_STRAFE_LEFT);
                sent_flags |= MF_STRAFE_LEFT;
                sh.event(format!(
                    "move|StartStrafeLeft|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }
            if !strafe_r && !strafe_l && (prev_sr || prev_sl) {
                send_move(MOVE_OP::MSG_MOVE_STOP_STRAFE, 0);
                sh.event(format!(
                    "move|StopStrafe|{:.1},{:.1},{:.1}",
                    pos[0], pos[1], pos[2]
                ));
            }

            // Heartbeat while moving (every 500 ms, carries all active flags).
            let now = std::time::Instant::now();
            if cur_flags != 0 && now >= next_hb {
                let _ = session.send_movement(
                    MOVE_OP::MSG_MOVE_HEARTBEAT,
                    cur_flags,
                    pos,
                    now_orient,
                    0.0,
                    0,
                    None,
                    None,
                );
                sh.event(format!(
                    "move|heartbeat|flags={:#x}|{:.1},{:.1},{:.1}",
                    cur_flags, pos[0], pos[1], pos[2]
                ));
                next_hb = now + HB_INTERVAL;
            }

            // Resting body drifted a fraction of a yard: server's exact compare
            // would read it as motion on the next stream packet, so echo once.
            if cur_flags == 0 {
                let drifted = match last_pose_sent {
                    Some((lp, _)) => {
                        let dx = pos[0] - lp[0];
                        let dy = pos[1] - lp[1];
                        let dz = pos[2] - lp[2];
                        (dx * dx + dy * dy + dz * dz) > 0.0001
                    }
                    None => true, // very first tick: nail down starting pose
                };
                if drifted {
                    let _ = session.send_movement(
                        MOVE_OP::MSG_MOVE_HEARTBEAT,
                        0,
                        pos,
                        now_orient,
                        0.0,
                        0,
                        None,
                        None,
                    );
                    sh.event(format!(
                        "move|heartbeat@rest|{:.1},{:.1},{:.1}",
                        pos[0], pos[1], pos[2]
                    ));
                    last_pose_sent = Some((pos, yaw));
                }
            }

            prev_input = input;
            last_move_flags = cur_flags;
        }

        match session.recv() {
            Ok(packet) => {
                // NewWorld / TransferPending: the server has announced a map change.
                // The 1.12 client must ack immediately with MSG_MOVE_WORLDPORT_ACK
                // (empty body) so mangosd advances to PLAYER_STATE_LOGGED_IN;
                // without it the socket is dropped after the next update tick.
                match &packet {
                    ServerPacket::NewWorld { .. } | ServerPacket::TransferPending { .. } => {
                        if let Err(e) = session.worldport_ack() {
                            sh.event(format!("error|worldport_ack|{e}"));
                        } else {
                            sh.event("worldport_ack");
                        }
                    }
                    _ => {}
                }
                handle_packet(&packet, sh);
                flush_item_queries(session, sh);
                queue_self_quests(session, sh);
                queue_creature_names(session, sh);
                queue_player_names(session, sh);
            }
            Err(e) => {
                // A read deadline is the normal quiet path; anything else ends
                // the session (the server dropped us, or the link broke).
                // The IO error sits under benilla-protocol's context layer
                // ("reading world header"), so walk the chain - a plain
                // downcast only sees the outermost error. Windows reports a
                // socket deadline as WSAETIMEDOUT (TimedOut), not WouldBlock.
                let quiet = e.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .map(|io| {
                            matches!(
                                io.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            )
                        })
                        .unwrap_or(false)
                });
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
        let mut seen = sh.creature_asked.lock().unwrap_or_else(|e| e.into_inner());
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

/// Ask the server for the names of the players in the stream window (the target
/// frame's `UnitName`). Other players' descriptors carry no name, and the roster
/// only names our own row. Two per packet, as for creatures.
fn queue_player_names(session: &mut WorldSession, sh: &Shared) {
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let mut asked: Vec<u64> = Vec::new();
    {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = sh.names_asked.lock().unwrap_or_else(|e| e.into_inner());
        for (guid, rec) in units.iter() {
            if rec.kind != 4 || *guid == self_guid {
                continue;
            }
            if seen.insert(*guid) {
                asked.push(*guid);
                if asked.len() >= 2 {
                    break;
                }
            }
        }
    }
    for guid in asked {
        let _ = session.name_query(guid);
    }
}

/// Build the item info row for one entry (empty when the query has not
/// answered yet).
pub(crate) fn item_info_row(entry: u32, templates: &HashMap<u32, ItemTemplate>) -> BnNetItemInfo {
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

/// The 1-based live inventory slots a template's `InventoryType` equips into, the static mask
/// `IsValidForSlot` (`0x5da1d0`) reads; `benilla-app`'s `ui_items/mod.rs:869` `find_equip_slot`,
/// which is vmangos' `Player::FindEquipSlot`. Without it every bag item looks like a misfit, so a
/// drop on the doll takes the auto-equip arm instead of the swap and a `CursorCanGoInSlot`
/// highlight never lights.
pub(crate) fn find_equip_slot(inventory_type: u32, has_relic_slot: bool) -> Vec<u8> {
    // Live-API ids: wire `EQUIPMENT_SLOT_*` + 1. Ammo is 0, loaded by entry with `CMSG_SET_AMMO`.
    const AMMO: u8 = 0;
    const HEAD: u8 = 1;
    const NECK: u8 = 2;
    const SHOULDERS: u8 = 3;
    const BODY: u8 = 4;
    const CHEST: u8 = 5;
    const WAIST: u8 = 6;
    const LEGS: u8 = 7;
    const FEET: u8 = 8;
    const WRISTS: u8 = 9;
    const HANDS: u8 = 10;
    const FINGER1: u8 = 11;
    const FINGER2: u8 = 12;
    const TRINKET1: u8 = 13;
    const TRINKET2: u8 = 14;
    const BACK: u8 = 15;
    const MAINHAND: u8 = 16;
    const OFFHAND: u8 = 17;
    const RANGED: u8 = 18;
    const TABARD: u8 = 19;
    match inventory_type {
        1 => vec![HEAD],
        2 => vec![NECK],
        3 => vec![SHOULDERS],
        4 => vec![BODY],
        5 | 20 => vec![CHEST],
        6 => vec![WAIST],
        7 => vec![LEGS],
        8 => vec![FEET],
        9 => vec![WRISTS],
        10 => vec![HANDS],
        11 => vec![FINGER1, FINGER2],
        12 => vec![TRINKET1, TRINKET2],
        13 => vec![MAINHAND, OFFHAND],
        14 => vec![OFFHAND],
        // The RANGED slot is the relic slot for Paladin, Shaman and Druid, so the ranged types and
        // `INVTYPE_RELIC` (28) exclude each other.
        15 if !has_relic_slot => vec![RANGED],
        16 => vec![BACK],
        17 => vec![MAINHAND],
        18 => vec![20, 21, 22, 23],
        19 => vec![TABARD],
        21 => vec![MAINHAND],
        22 | 23 => vec![OFFHAND],
        24 => vec![AMMO],
        25 | 26 if !has_relic_slot => vec![RANGED],
        28 if has_relic_slot => vec![RANGED],
        // NON_EQUIP (0), QUIVER (27) and anything past MAX_INVTYPE are not equippable.
        _ => Vec::new(),
    }
}

/// The first empty candidate of an equip mask, else the first, as vmangos `Player::FindEquipSlot`
/// picks with `swap` set and the socket's occupant goes back to the source.
fn equip_destination(candidates: &[u8], worn: impl Fn(u8) -> bool) -> Option<u8> {
    candidates
        .iter()
        .copied()
        .find(|live| !worn(*live))
        .or_else(|| candidates.first().copied())
}

/// Vanilla's Dual Wield passive (`SPELL_EFFECT_DUAL_WIELD`, `spell_template` effect 40): the
/// server's `m_canDualWield` is set by it, and an offhand weapon without it is refused.
const DUAL_WIELD_SPELL: u32 = 674;

/// The seats an auto-equip may send an item to: the client's own `InventoryType` map, narrowed
/// where the server is stricter. `Player::FindEquipSlot` walks `GetAllowedEquipSlots`, which
/// offers the off hand for `INVTYPE_WEAPON` only to a dual wielder (`Objects/Item.cpp:624-633`)
/// and refuses the offhand equip otherwise (`Player.cpp:9915`).
fn auto_equip_slots(inventory_type: u32, has_relic_slot: bool, can_dual_wield: bool) -> Vec<u8> {
    const INVTYPE_WEAPON: u32 = 13;
    const MAINHAND: u8 = 16;
    let mut slots = find_equip_slot(inventory_type, has_relic_slot);
    if inventory_type == INVTYPE_WEAPON && !can_dual_wield {
        slots.retain(|&s| s == MAINHAND);
    }
    slots
}

/// The inventory row's own addressing for a wire position: rows carry 255 (`BAG_EQUIPPED`) for
/// worn equipment and 0 for the backpack, whose wire seats start at `SLOT_PACK_FIRST` (23) in the
/// player's array. A worn bag's contents (19..22) already match.
fn row_pos_for_wire(bag: u8, slot: u8) -> (u8, u8) {
    if bag == BAG_EQUIPPED && slot >= SLOT_PACK_FIRST {
        (0, slot - SLOT_PACK_FIRST)
    } else {
        (bag, slot)
    }
}

/// The live slot an `AutoEquip` command sends its item to (0 is the ammo seat, loaded by entry
/// rather than moved) and the entry that names it. `None` when the row is not there or its
/// template names no slot, so the caller can keep the client's own packet.
fn equip_target(sh: &Shared, bag: u8, slot: u8) -> Option<(u8, u32)> {
    let rows = inventory_rows(sh);
    let (row_bag, row_slot) = row_pos_for_wire(bag, slot);
    let row = rows
        .iter()
        .find(|r| r.bag == row_bag && r.slot == row_slot)?;
    let can_dual_wield = sh
        .spells
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&DUAL_WIELD_SPELL);
    let candidates = auto_equip_slots(
        row.info.inventory_type,
        sh.relic_slot.load(Ordering::Relaxed),
        can_dual_wield,
    );
    let live = equip_destination(&candidates, |live| {
        live != 0
            && rows
                .iter()
                .any(|r| r.bag == BAG_EQUIPPED && r.slot == live - 1)
    })?;
    Some((live, row.entry))
}

/// Every occupied slot of the player's own inventory, in client order:
/// equipment 0..18, backpack, then the four equipped bags.
pub(crate) fn inventory_rows(sh: &Shared) -> Vec<BnNetInvItem> {
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let templates = sh.item_templates.lock().unwrap_or_else(|e| e.into_inner());
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
            let is_self = *guid == self_guid;
            if let Some(rec) = units.get_mut(guid) {
                rec.fields.merge(mask.clone());
                if rec.kind == 4 {
                    queue_visible_items(mask, sh);
                }
            } else if units.len() < MAX_UNITS {
                units.insert(
                    *guid,
                    UnitRec {
                        kind: if is_self { 4 } else { 0 },
                        fields: mask.clone(),
                        pos: [0.0; 3],
                        facing: 0.0,
                        spline: None,
                    },
                );
            }
            if is_self {
                // Safe to claim mover ownership once our own object exists
                // anywhere in the stream; vmangos drops mover claims for
                // unknown guids.
                sh.mover_ready.store(1, Ordering::SeqCst);
            }
        }
        Object::Movement { guid, movement } => {
            let is_self = *guid == self_guid;
            if let Some(rec) = units.get_mut(guid) {
                if let Some((v, o)) = movement.position {
                    rec.pos = [v.x, v.y, v.z];
                    rec.facing = o;
                    rec.spline = None;
                }
            }
            if is_self {
                sh.mover_ready.store(1, Ordering::SeqCst);
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
            let kind = object_kind(*object_type);
            units.insert(
                *guid,
                UnitRec {
                    kind,
                    fields: mask.clone(),
                    pos,
                    facing,
                    spline: None,
                },
            );
            if *object_type == ObjectType::Player {
                queue_visible_items(mask, sh);
            }
            if *guid == self_guid {
                sh.mover_ready.store(1, Ordering::SeqCst);
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
        // `0x605f30`: a ghost carries health 1, so `unit_is_dead` alone leaves
        // the death screen and Release Spirit unreachable.
        dead: f.is_dead_or_ghost() as u8,
        ghost: f.player_is_ghost() as u8,
        stat: std::array::from_fn(|i| f.unit_stat(i as u8).unwrap_or(0)),
        armor: f.unit_resistance(0).unwrap_or(0),
        attack_power: f.unit_attack_power().unwrap_or(0),
        damage_min: f.unit_min_damage().unwrap_or(0.0),
        damage_max: f.unit_max_damage().unwrap_or(0.0),
        attack_time: f.unit_base_attack_time(0).unwrap_or(0),
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
    // The clip follows the worn main hand, not the last attack: an equip, an
    // unequip or a lost item object lands here before the next hit.
    store_self_swing_anim(sh, self_main_swing(sh));
    *sh.player.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
}

fn handle_packet(packet: &ServerPacket, sh: &Shared) {
    match packet {
        // The unit frames' names: `CMSG_NAME_QUERY` is asked once per player guid
        // (queue_player_names); an empty name is the server's unknown answer.
        ServerPacket::NameQueryResponse { guid, name, .. } => {
            if !name.is_empty() {
                sh.names
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(*guid, name.clone());
            }
        }
        // The death UI's inputs: the offer the DEATH/RESURRECT dialog answers,
        // the spirit healer's confirm, the reclaim delay and our own combat latch.
        ServerPacket::ResurrectRequest(r) => {
            sh.resurrect_from.store(r.caster, Ordering::SeqCst);
            sh.resurrect_sickness
                .store(u32::from(r.sickness), Ordering::SeqCst);
            sh.resurrect_has_timer
                .store(u32::from(r.has_timer), Ordering::SeqCst);
        }
        ServerPacket::SpiritHealerConfirm { npc } => {
            sh.spirit_healer.store(*npc, Ordering::SeqCst);
        }
        ServerPacket::CorpseReclaimDelay { delay_ms } => {
            sh.corpse_delay_ms.store(*delay_ms, Ordering::SeqCst);
        }
        ServerPacket::AttackStart { attacker, .. } => {
            sh.engaged
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*attacker);
            if *attacker == sh.self_guid.load(Ordering::SeqCst) {
                sh.self_attacking.store(1, Ordering::SeqCst);
                store_self_swing_anim(sh, self_main_swing(sh));
            }
        }
        ServerPacket::AttackStop { attacker, .. } => {
            sh.engaged
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(attacker);
            if *attacker == sh.self_guid.load(Ordering::SeqCst) {
                sh.self_attacking.store(0, Ordering::SeqCst);
                sh.attack_target.store(0, Ordering::SeqCst);
            }
        }
        ServerPacket::MessageChat(msg) => {
            // chat|<type>|<sender>|<text>: the host feeds its chat frame. Types
            // follow the client's ChatType ids (0x00 say, 0x0A system, ...).
            let who = msg
                .sender_name
                .clone()
                .unwrap_or_else(|| msg.sender_guid.to_string());
            sh.event(format!("chat|{}|{}|{}", msg.chat_type, who, msg.text));
            // The same line as the stock UI's event: the FFI has no app chat router, so the
            // VM's own `ChatFrame_OnEvent` prints it and owns the new-message indicator.
            crate::chat_vm::enqueue(msg);
        }
        // The world stream is very chatty; object updates are the bulk, so they
        // are folded into the tracked table rather than echoed as events.
        ServerPacket::UpdateObject { objects } => {
            for obj in objects {
                apply_object(obj, sh);
            }
            refresh_player(sh);
        }
        // Creature/self splines: MaNGOS moves everything except free players
        // with `SMSG_MONSTER_MOVE`, so without this every npc stands still.
        ServerPacket::MonsterMove {
            guid,
            start,
            path,
            facing,
            stop,
            spline_id,
            duration_ms,
            flying,
            run_mode,
            ..
        } => {
            let start = [start.x, start.y, start.z];
            let path: Vec<[f32; 3]> = path.iter().map(|p| [p.x, p.y, p.z]).collect();
            apply_monster_move(
                *guid,
                start,
                &path,
                facing,
                *stop,
                *spline_id,
                *duration_ms,
                *flying,
                *run_mode,
                sh,
            );
        }
        // A relayed player `MSG_MOVE_*`: an authoritative snap of another body.
        ServerPacket::PlayerMove {
            guid,
            position,
            orientation,
            ..
        } => {
            let mut units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(rec) = units.get_mut(guid) {
                rec.pos = [position.x, position.y, position.z];
                rec.facing = *orientation;
                rec.spline = None;
            }
        }
        // The normal carrier past vmangos's 300-packet/10s threshold: a deflated
        // batch of the same movement packets, folded in by recursion.
        ServerPacket::CompressedMoves { packets } => {
            for p in packets {
                handle_packet(p, sh);
            }
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
                            start_quest: info.start_quest,
                            material: info.material,
                            sheath: info.sheath,
                            info: info.clone(),
                        },
                    );
            } else {
                sh.item_unknown
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(*entry);
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
                    civilian: i.civilian,
                    racial_leader: i.racial_leader,
                    type_flags: i.type_flags,
                },
                None => CreatureTemplateLite {
                    name: String::new(),
                    subname: String::new(),
                    creature_type: 0,
                    rank: 0,
                    civilian: false,
                    racial_leader: false,
                    type_flags: 0,
                },
            };
            sh.creatures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*entry, row);
        }
        // The "You receive …" line; `bag`/`slot` locate the new row so the host
        // can refresh an inventory panel. Our own push also arms the stock bag
        // buttons' item animation: `OnItemPush`'s guid gate (`0x491b56`/`0x491b61`)
        // keeps another player's push out of the interface (the line is unguarded
        // and prints either way).
        ServerPacket::ItemPushResult(p) => {
            let me = sh.self_guid.load(Ordering::SeqCst);
            if me != 0 && p.player_guid == me {
                let entry = p.item_entry;
                let fresh = sh
                    .item_asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(entry);
                if fresh {
                    sh.item_queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(entry);
                }
                let mut q = sh.item_pushes.lock().unwrap_or_else(|e| e.into_inner());
                if q.len() >= 64 {
                    q.pop_front();
                }
                q.push_back(PendingItemPush {
                    entry,
                    container: push_container(p.bag_slot, p.item_slot),
                    tries: 0,
                });
            }
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
        // ---- combat feedback: the numbers the client floats over units ----
        ServerPacket::AttackerState(s) => {
            // A landed swing is `0x5ea800`'s first act: `0x5ecdb0(0)`, the latch
            // cleared, for our own guid only (`0x6259b6`).
            if s.attacker == sh.self_guid.load(Ordering::SeqCst) {
                clear_swing_refusal();
            }
            // `displayed` is the client gate: a dodged or
            // parried on-next-swing ability gets neither log line nor number.
            if s.displayed() {
                sh.event(format!(
                    "dmg|melee|{}|{}|{}|{}|{}|{}|{}",
                    s.victim, s.attacker, s.damage, s.victim_state, s.hit_info, s.absorb, s.resist
                ));
                // The victim's reaction clip (`0x60ea70`, defense through `$CPP` `0x624a01`),
                // and the melee spurt's side/size for the host's particle child (`0x624530`).
                if let Some((anim, full_body)) = victim_reaction(sh, s) {
                    sh.event(format!(
                        "react|{}|{}|{}",
                        s.victim,
                        anim,
                        u8::from(full_body)
                    ));
                }
                if let Some((display, attach, large)) = melee_blood(sh, s) {
                    sh.event(format!(
                        "fx|blood|{}|{}|{}|{}",
                        s.victim,
                        display,
                        attach,
                        u8::from(large)
                    ));
                }
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
            // `0x625a8a`/`0x625aa1`: the two refusals that latch a line; the third
            // arm only stops the attack (`0x625ab8`), no message.
            if code == 3 {
                stop_attack();
            } else {
                latch_swing_refusal(code);
            }
            sh.event(format!("attack|error|{code}"));
        }
        // `0x5e7dd0`, `SMSG_CANCEL_COMBAT`: the stop arm of the same family, byte
        // for byte — the server forced our auto-attack off, silently.
        ServerPacket::CancelCombat => stop_attack(),
        ServerPacket::XpGain(x) => {
            sh.event(format!("xp|{}|{}|{}", x.total, x.base, u8::from(x.kill)));
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
            sh.quests.lock().unwrap_or_else(|e| e.into_inner()).insert(
                t.quest_id,
                QuestTemplateLite {
                    level: t.level,
                    zone: t.zone_or_sort,
                    quest_type: t.quest_type,
                    money: t.money,
                    flags: t.flags,
                    title: t.title.clone(),
                    objectives: t.objectives_text.clone(),
                    details: t.details.clone(),
                    end_text: t.end_text.clone(),
                    src_item_id: t.src_item_id,
                    reward_spell: t.reward_spell,
                    objective_rows: t
                        .objectives
                        .iter()
                        .map(|o| QuestObjectiveLite {
                            creature_or_go: o.creature_or_go,
                            required_count: o.required_count,
                            item_id: o.item_id,
                            item_count: o.item_count,
                            text: o.text.clone(),
                        })
                        .collect(),
                    rewards: t.rewards,
                    choices: t.choices,
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
            guid, gold, items, ..
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
        // Questgiver window. The gossip menu is the VM's own (`gossip_vm`): it latches open on this
        // packet, queries the greeting it lacks, and shows the stock `GossipFrame` once the text
        // lands — the reference's `0x4e26e0`. The `BnNetGiver` snapshot keeps feeding the host's
        // quest panels and its `bn.Ui giver` probe.
        ServerPacket::GossipMessage {
            npc,
            text_id,
            options,
            quests,
        } => {
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
            let gender = ui_unit(*npc)
                .and_then(|u| u.fields.unit_gender())
                .unwrap_or(0);
            if crate::gossip_vm::open(*npc, *text_id, options.clone(), quests.clone(), gender) {
                let _ = send_cmd(Cmd::NpcTextQuery {
                    text_id: *text_id,
                    npc: *npc,
                });
            }
            sh.event(format!("giver|open|{}|{}", npc, count));
        }
        ServerPacket::NpcText { text_id, blocks } => {
            let open = crate::gossip_vm::npc();
            let gender = if open == 0 {
                0
            } else {
                ui_unit(open)
                    .and_then(|u| u.fields.unit_gender())
                    .unwrap_or(0)
            };
            crate::gossip_vm::text_arrived(*text_id, blocks.clone(), gender);
        }
        ServerPacket::GossipComplete => {
            *sh.giver.lock().unwrap_or_else(|e| e.into_inner()) = BnNetGiver::default();
            crate::gossip_vm::complete();
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
            // A turn-in can move every giver's marker (`QuestGiver::bump_reask`).
            sh.quest_epoch.fetch_add(1, Ordering::SeqCst);
            sh.event(format!(
                "giver|complete|{}|{}|{}",
                c.quest_id, c.xp, c.money
            ));
        }
        // The overhead marker answers. `npc` is the queried guid; the sweep's re-ask rules
        // decide when it is asked again, so this only stores the latest answer.
        ServerPacket::QuestGiverStatus { npc, status } => {
            sh.quest_status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*npc, *status);
        }
        // The flight master's green `!` (`ui_taxi`'s `FlightMasterStatus`): absent means known.
        ServerPacket::TaxiNodeStatus { guid, known } => {
            sh.taxi_known
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(*guid, *known);
        }
        ServerPacket::QuestGiverInvalid { .. } => {
            sh.event("giver|invalid".to_string());
        }
        ServerPacket::QuestGiverFailed { .. } => {
            sh.event("giver|failed".to_string());
        }
        // ---- vendor / shop ----
        ServerPacket::VendorList { vendor, items } => {
            let mut win = BnNetVendor {
                guid: *vendor,
                ..Default::default()
            };
            let n = items.len().min(win.rows.len());
            for i in 0..n {
                let it = &items[i];
                win.rows[i] = BnNetVendorRow {
                    slot: it.slot,
                    entry: it.entry,
                    display_id: it.display_id,
                    count: it.current_count,
                    price: it.price,
                    max_durability: it.max_durability,
                    buy_count: it.buy_count,
                };
                if it.entry != 0 {
                    let mut asked = sh.item_asked.lock().unwrap_or_else(|e| e.into_inner());
                    let mut queue = sh.item_queue.lock().unwrap_or_else(|e| e.into_inner());
                    if asked.insert(it.entry) {
                        queue.push(it.entry);
                    }
                }
            }
            win.count = n as u32;
            *sh.vendor.lock().unwrap_or_else(|e| e.into_inner()) = win;
            sh.event(format!("vendor|open|{vendor}|{n}"));
        }
        ServerPacket::BuyItem {
            vendor,
            slot,
            new_count,
            purchase_count,
        } => {
            // The purchase is applied through the next SMSG_UPDATE_OBJECT; the
            // stock row refreshes in place so the host can redraw the price.
            let mut win = sh.vendor.lock().unwrap_or_else(|e| e.into_inner());
            let i = (*slot as usize).saturating_sub(1);
            if i < win.rows.len() {
                win.rows[i].count = *new_count;
            }
            sh.event(format!(
                "vendor|bought|{vendor}|slot={slot}|new_stock={new_count}|count={purchase_count}"
            ));
        }
        ServerPacket::SellItemResult {
            vendor,
            item_guid,
            reason,
        } => {
            // Successful sells are silent (the server only shows them in the
            // player's own UPDATE_OBJECT), so a SELL packet means a refusal.
            sh.event(format!("vendor|sellfail|{vendor}|{item_guid}|{reason}"));
        }
        ServerPacket::BuyFailed {
            vendor,
            item_entry,
            reason,
        } => {
            sh.event(format!("vendor|buyfail|{vendor}|{item_entry}|{reason}"));
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
        | ServerPacket::QuestLogFull => {
            sh.event(format!("pkt|{}", packet.name()));
        }
        // Every packet whose answer to a giver can change without our own descriptor moving:
        // the quest-log progress (`QuestGiver::bump_reask` in `ui_quest/net.rs`), a standing that
        // feeds `SatisfyQuestReputation`, and a group roster (`ui_party/net.rs`).
        ServerPacket::QuestUpdateComplete { .. }
        | ServerPacket::QuestUpdateAddKill { .. }
        | ServerPacket::QuestUpdateAddItem { .. }
        | ServerPacket::SetFactionStanding { .. } => {
            sh.quest_epoch.fetch_add(1, Ordering::SeqCst);
            sh.event(format!("pkt|{}", packet.name()));
        }
        // ---- group / party ----
        ServerPacket::GroupInvite { inviter } => {
            {
                let mut party = sh.party.lock().unwrap_or_else(|e| e.into_inner());
                party.invite = 1;
                copy_name(&mut party.inviter, inviter);
            }
            sh.event(format!("group_invite|{inviter}"));
        }
        ServerPacket::GroupDecline { name } => {
            {
                let mut party = sh.party.lock().unwrap_or_else(|e| e.into_inner());
                // Only clear the popup when the decliner is the one who asked.
                let inviter = name_of(&party.inviter);
                if inviter.eq_ignore_ascii_case(name) {
                    party.invite = 0;
                }
            }
            sh.event(format!("group_decline|{name}"));
        }
        // Kick or leave: the roster is gone (an empty list follows too, the
        // full reset here keeps the frames from lingering one packet).
        ServerPacket::GroupUninvited => {
            *sh.party.lock().unwrap_or_else(|e| e.into_inner()) = BnNetParty::default();
            sh.event("group_leave");
        }
        ServerPacket::GroupDestroyed => {
            *sh.party.lock().unwrap_or_else(|e| e.into_inner()) = BnNetParty::default();
            sh.event("group_destroyed");
        }
        ServerPacket::GroupLeaderChanged { name } => {
            {
                let mut party = sh.party.lock().unwrap_or_else(|e| e.into_inner());
                for row in party.members.iter_mut() {
                    row.leader = (name_of(&row.name) == *name) as u32;
                }
            }
            sh.event(format!("group_leader|{name}"));
        }
        // The full roster (the recipient has no row). An empty list is the
        // client's own "you left the group" signal; live stats the server
        // already sent are carried over by guid so the frame never flashes 0.
        ServerPacket::GroupList {
            group_type,
            members,
            leader,
            ..
        } => {
            let mut party = sh.party.lock().unwrap_or_else(|e| e.into_inner());
            if members.is_empty() {
                *party = BnNetParty::default();
            } else {
                let old = party.members;
                let mut next = BnNetParty::default();
                next.in_group = 1;
                next.group_type = u32::from(*group_type) + 1;
                next.count = members.len().min(BN_PARTY_SLOTS) as u32;
                for (i, m) in members.iter().take(BN_PARTY_SLOTS).enumerate() {
                    let mut row = BnNetPartyMember::default();
                    row.guid = m.guid;
                    copy_name(&mut row.name, &m.name);
                    row.online = m.listed_online() as u32;
                    row.leader = (m.guid == *leader) as u32;
                    if let Some(prev) = old.iter().find(|o| o.guid == m.guid) {
                        row.level = prev.level;
                        row.zone = prev.zone;
                        row.dead = prev.dead;
                        row.health = prev.health;
                        row.max_health = prev.max_health;
                        row.power = prev.power;
                        row.max_power = prev.max_power;
                        row.power_type = prev.power_type;
                    }
                    next.members[i] = row;
                }
                *party = next;
            }
            let n = party.count;
            drop(party);
            // A new roster can open quest dialogs we could not see (`ui_party/net.rs`).
            sh.quest_epoch.fetch_add(1, Ordering::SeqCst);
            sh.event(format!("group|{n}"));
        }
        // A delta (or the asked-for _FULL form): patch only the present fields.
        ServerPacket::PartyMemberStats { guid, info, .. } => {
            let mut party = sh.party.lock().unwrap_or_else(|e| e.into_inner());
            let count = party.count as usize;
            let mut known = false;
            for row in party.members.iter_mut().take(count) {
                if row.guid != *guid {
                    continue;
                }
                known = true;
                if let Some(s) = info.status {
                    // ONLINE 0x01; DEAD 0x04 / GHOST 0x08 both draw as dead.
                    row.online = (s & 0x01 != 0) as u32;
                    row.dead = (s & 0x0c != 0) as u32;
                }
                if let Some(v) = info.cur_hp {
                    row.health = u32::from(v);
                }
                if let Some(v) = info.max_hp {
                    row.max_health = u32::from(v);
                }
                if let Some(v) = info.cur_power {
                    row.power = u32::from(v);
                }
                if let Some(v) = info.max_power {
                    row.max_power = u32::from(v);
                }
                if let Some(v) = info.power_type {
                    row.power_type = u32::from(v);
                }
                if let Some(v) = info.level {
                    row.level = u32::from(v);
                }
                if let Some(v) = info.zone {
                    row.zone = u32::from(v);
                }
                break;
            }
            drop(party);
            if !known {
                sh.event("pkt|PARTY_MEMBER_STATS".to_string());
            }
        }
        ServerPacket::PartyCommandResult {
            operation,
            member,
            result,
        } => {
            sh.event(format!("group_result|{operation}|{member}|{result}"));
        }
        _ => {}
    }
}

// ---- C ABI ----

fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(|s| s.to_owned())
}

/// `std::env::var` as an `Option` (an empty value is `Some("")`).
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Start a login on a worker thread. `host` is the realmd address ("host" or
/// "host:port"), `user`/`pass` the account. Empty strings fall back to the
/// `WOW_HOST` / `WOW_USER` / `WOW_PASS` environment variables, so the host can
/// call `bn_net_connect("", "", "")` for an env-driven logon. Returns 1 when
/// the worker started.
#[no_mangle]
pub extern "C" fn bn_net_connect(
    host: *const c_char,
    user: *const c_char,
    pass: *const c_char,
) -> u32 {
    let host = cstr(host)
        .filter(|s| !s.is_empty())
        .or_else(|| env_var("WOW_HOST"));
    let user = cstr(user)
        .filter(|s| !s.is_empty())
        .or_else(|| env_var("WOW_USER"));
    let pass = cstr(pass)
        .filter(|s| !s.is_empty())
        .or_else(|| env_var("WOW_PASS"));
    let (Some(host), Some(user), Some(pass)) = (host, user, pass) else {
        return 0;
    };
    {
        let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return 0; // a session is already running
        }
        let (tx, rx) = channel();
        *guard = Some(tx);
        std::thread::Builder::new()
            .name("bn-net".into())
            .spawn(move || {
                worker(host, user, pass, rx, RunMode::Auto);
                let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
                *guard = None;
            })
            .ok();
    }
    1
}

// ---- glue-menu helpers (the frame's host drives the login flow) ----

/// One read of the worker's menu state for the glue host.
pub(crate) struct MenuSnapshot {
    pub status: u32,
    pub realms_epoch: u64,
    pub chars_epoch: u64,
    /// 1 while the worker thread is alive (commands channel still open).
    pub alive: bool,
}

pub(crate) fn menu_snapshot() -> MenuSnapshot {
    let sh = shared();
    let alive = commands()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    MenuSnapshot {
        status: sh.status(),
        realms_epoch: sh.realms_epoch.load(Ordering::SeqCst),
        chars_epoch: sh.chars_epoch.load(Ordering::SeqCst),
        alive,
    }
}

pub(crate) fn menu_realms() -> Vec<RealmInfo> {
    shared()
        .realms
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn menu_chars() -> Vec<BnNetChar> {
    shared()
        .chars
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Index of `name` in the published realm list (the list rows are also read by
/// index, so a picked realm maps straight back).
pub(crate) fn menu_realm_index(name: &str) -> Option<u32> {
    let realms = shared().realms.lock().unwrap_or_else(|e| e.into_inner());
    realms
        .iter()
        .position(|r| r.name.eq_ignore_ascii_case(name))
        .map(|i| i as u32)
}

/// Start a glue-mode login: publish the realmd realm list and wait for a
/// `RealmPick` from the menu instead of auto-connecting. Fails when a session
/// is already running.
pub(crate) fn menu_login(host: &str, user: &str, pass: &str) -> bool {
    let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return false; // a session is already running
    }
    let (tx, rx) = channel();
    *guard = Some(tx);
    let (host, user, pass) = (host.to_string(), user.to_string(), pass.to_string());
    std::thread::Builder::new()
        .name("bn-net".into())
        .spawn(move || {
            worker(host, user, pass, rx, RunMode::Glue);
            let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        })
        .ok();
    true
}

/// 1 while a worker owns the session (login in flight or the menu's link up).
pub(crate) fn menu_active() -> bool {
    commands()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

pub(crate) fn menu_pick_realm(index: u32) -> bool {
    send_cmd(Cmd::RealmPick(index))
}

/// Drop the menu's link at the worker (the logon or the held realmd socket).
pub(crate) fn menu_logout() -> bool {
    send_cmd(Cmd::Logout)
}

pub(crate) fn menu_enter(guid: u64) -> bool {
    send_cmd(Cmd::Enter(guid))
}

pub(crate) fn menu_refresh_roster() -> bool {
    send_cmd(Cmd::RefreshRoster)
}

pub(crate) fn menu_create_character(req: CharCreateReq) -> bool {
    send_cmd(Cmd::CharCreateReq(req))
}

pub(crate) fn menu_delete_character(guid: u64) -> bool {
    send_cmd(Cmd::CharDelete(guid))
}

pub(crate) fn menu_rename_character(guid: u64, name: String) -> bool {
    send_cmd(Cmd::CharRename { guid, name })
}

pub(crate) fn menu_take_error() -> Option<String> {
    shared()
        .menu_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

pub(crate) fn menu_take_create_reply() -> Option<(String, u8)> {
    shared()
        .create_reply
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

pub(crate) fn menu_take_delete_reply() -> Option<(u64, u8)> {
    shared()
        .delete_reply
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

pub(crate) fn menu_take_rename_reply() -> Option<(u64, String, u8)> {
    shared()
        .rename_reply
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

#[allow(dead_code)]
const _BN_NET_PATCH_V1: () = ();

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

/// The account's realm list (the host's server picker). 0 when no logon has
/// completed.
#[no_mangle]
pub extern "C" fn bn_net_realm_count() -> u32 {
    shared()
        .realms
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len() as u32
}

/// Fill `out` with the i-th realm row. Returns 1 on success.
#[no_mangle]
pub extern "C" fn bn_net_realm(index: u32, out: *mut BnNetRealm) -> u32 {
    if out.is_null() {
        return 0;
    }
    let realms = shared().realms.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = realms.get(index as usize) else {
        return 0;
    };
    let mut row = BnNetRealm::default();
    copy_name(&mut row.address, &r.address);
    copy_name(&mut row.name, &r.name);
    row.characters = r.characters;
    row.realm_type = r.realm_type;
    row.flags = r.flags;
    unsafe {
        *out = row;
    }
    1
}

/// World-connect on realm `index`'s address and re-read the roster. Returns 1
/// when the pick was queued (the answer is a fresh `chars|N` event line at
/// `ST_CHARS`).
#[no_mangle]
pub extern "C" fn bn_net_realm_pick(index: u32) -> u32 {
    send_cmd(Cmd::RealmPick(index)) as u32
}

/// Create a character. `name` is NUL-terminated; the appearance bytes are the
/// `CharSections` indices the host's char-create form collects. The result byte
/// answers as `char_create|<name>|<code>` on the event queue; a 0x2E (success)
/// is followed by a fresh `chars|N` roster.
#[no_mangle]
pub extern "C" fn bn_net_char_create(
    name: *const c_char,
    race: u8,
    class: u8,
    gender: u8,
    skin: u8,
    face: u8,
    hair: u8,
    hair_color: u8,
    facial: u8,
) -> u32 {
    let Some(name) = cstr(name) else {
        return 0;
    };
    if name.is_empty() {
        return 0;
    }
    send_cmd(Cmd::CharCreateReq(CharCreateReq {
        name,
        race,
        class,
        gender,
        skin,
        face,
        hair_style: hair,
        hair_color,
        facial_hair: facial,
    })) as u32
}

/// Delete a roster character. Answers as `char_delete|<guid>|<code>`.
#[no_mangle]
pub extern "C" fn bn_net_char_delete(guid: u64) -> u32 {
    if guid == 0 {
        return 0;
    }
    send_cmd(Cmd::CharDelete(guid)) as u32
}

/// Rename a character the server flagged for it. Answers as
/// `char_rename|<guid>|<code>`, and a success re-reads the roster.
#[no_mangle]
pub extern "C" fn bn_net_char_rename(guid: u64, name: *const c_char) -> u32 {
    let Some(name) = cstr(name) else {
        return 0;
    };
    if name.is_empty() {
        return 0;
    }
    send_cmd(Cmd::CharRename { guid, name }) as u32
}

/// Re-ask the roster (after the host's own refresh, or re-entry to the park).
#[no_mangle]
pub extern "C" fn bn_net_refresh_roster() -> u32 {
    send_cmd(Cmd::RefreshRoster) as u32
}

/// Log out to the character park on the same link (a `bn_net_send` kind 9 in
/// the world does the in-world logout; this one is the park's own back
/// button). Answers as `logout|roster` plus a fresh roster at `ST_CHARS`.
#[no_mangle]
pub extern "C" fn bn_net_logout_to_roster() -> u32 {
    send_cmd(Cmd::LogoutToRoster) as u32
}

/// Re-login with the cached `bn_net_connect` credentials: drops any session
/// (roster or world) and starts a fresh worker on the same host/account.
/// The answer is the usual `logon|ok` / `chars|N` event cascade. 0 when there
/// is nothing cached yet.
#[no_mangle]
pub extern "C" fn bn_net_reconnect() -> u32 {
    let creds = shared()
        .creds
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some((host, user, pass)) = creds else {
        return 0;
    };
    // Close whatever link is running first, so only one worker owns the state.
    let _ = send_cmd(Cmd::Logout);
    let _ = send_cmd(Cmd::LogoutToRoster);
    // Give the running worker a beat to notice its close before we restart.
    std::thread::sleep(Duration::from_millis(50));
    {
        let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return 0;
        }
        let (tx, rx) = channel();
        *guard = Some(tx);
        std::thread::Builder::new()
            .name("bn-net".into())
            .spawn(move || {
                worker(host, user, pass, rx, RunMode::Auto);
                let mut guard = commands().lock().unwrap_or_else(|e| e.into_inner());
                *guard = None;
            })
            .ok();
    }
    1
}

/// Kernel for every in-world action:
/// kind 1 = chat (text), 2 = attack (a=guid), 3 = target (a=guid),
/// 4 = loot (a=guid), 5 = use gameobject (a=guid),
/// 6 = accept quest (a=npc, b=quest), 7 = complete quest (a=npc, b=quest),
/// 8 = quest query (b=quest), 9 = logout,
/// 11 = use item (b = bag<<8 | slot, a = spell slot),
/// 12 = auto-equip (b = bag<<8 | slot),
/// 13 = swap inventory slots (a = src, b = dst),
/// 24 = accept a group invite, 25 = leave/disband the group,
/// 26 = invite by name (text), 27 = decline the pending invite.
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
        10 => Cmd::Cast {
            spell: a as u32,
            target: 0,
        },
        11 => Cmd::UseItem {
            bag: ((b >> 8) & 0xff) as u8,
            slot: (b & 0xff) as u8,
            spell: (a & 0xff) as u8,
        },
        // The host names a backpack item bag 0 slot 0..15, the row's own addressing; the
        // command speaks the wire's player array, where those seats are 255/23..38.
        12 => {
            let bag = ((b >> 8) & 0xff) as u8;
            let slot = (b & 0xff) as u8;
            if bag == 0 {
                Cmd::AutoEquip {
                    bag: BAG_EQUIPPED,
                    slot: SLOT_PACK_FIRST + slot,
                }
            } else {
                Cmd::AutoEquip { bag, slot }
            }
        }
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
        // 24 = accept a group invite, 25 = leave/disband,
        // 26 = invite by name (text), 27 = decline the pending invite.
        24 => Cmd::GroupAccept,
        25 => Cmd::GroupDisband,
        26 => match cstr(text) {
            Some(name) if !name.is_empty() => Cmd::GroupInvite(name),
            _ => return 0,
        },
        27 => Cmd::GroupDecline,
        // 28 = open the vendor's stock list (a = vendor guid); answers as
        // `vendor|open|...` on the event queue.
        // 29 = buy (a = vendor guid, b = entry | ((count-1) << 16), 1..256).
        // 30 = sell whole stack (a = our item guid, at the open vendor).
        // 31 = buyback (a = vendor guid, b = absolute buyback slot 69..80).
        // 32 = repair one item (a = item guid, at the open vendor).
        // 33 = repair everything (a = vendor guid, b = 0).
        28 => Cmd::VendorList(a),
        29 => Cmd::VendorBuy {
            vendor: a,
            entry: (b & 0xFFFF) as u32,
            count: (((b >> 16) & 0xFF) as u8).max(1),
        },
        30 => Cmd::VendorSell {
            // A sale only makes sense at the list that is open, and that guid is 64-bit: the
            // u32 `b` cannot carry it, so the middle layer names its own open vendor.
            vendor: ui_vendor().guid,
            item_guid: a,
            count: 0,
        },
        31 => Cmd::VendorBuyback { vendor: a, slot: b },
        32 => Cmd::VendorRepair {
            vendor: ui_vendor().guid,
            item_guid: a,
        },
        33 => Cmd::VendorRepair {
            vendor: a,
            item_guid: 0,
        },
        // 34 = gossip hello (a = npc): the menu of a unit with `UNIT_NPC_FLAGS`
        // gossip, which the questgiver hello's quest list never fills.
        34 => Cmd::GossipHello(a),
        _ => return 0,
    };
    send_cmd(cmd) as u32
}

/// `UNIT_NPC_FLAGS` (`cursor_mode.rs::npc_flags`), 1.12 numbering, for the arms this kernel can
/// send. REPAIR (0x4000) is never consulted by the ladder (`0x5f0130`).
mod npc_flags {
    pub const GOSSIP: u32 = 0x1;
    pub const QUESTGIVER: u32 = 0x2;
    pub const VENDOR: u32 = 0x4;
}

/// The cached `SMSG_QUESTGIVER_STATUS` gate (`cursor_mode.rs::questgiver_has_quest`, `0x5df490`):
/// a quest-less questgiver opens a frame of placeholder text, so its arm never fires.
fn questgiver_has_quest(status: Option<u32>) -> bool {
    use benilla_protocol::messages::dialog_status;
    !matches!(
        status,
        None | Some(dialog_status::NONE) | Some(dialog_status::UNAVAILABLE)
    )
}

/// The first-match-wins NPC-service ladder (`click.rs`'s `service_arm` over `0x5f0130`),
/// projected onto the kinds this kernel sends: gossip `34`, questgiver `18`, vendor `28`. Bits 3
/// and up have arms (taxi, trainer, bank, binder, stable) with no verb here; they are all higher
/// than the three above, so first-match-wins over them is unchanged and they send nothing.
fn service_send(npc_flags: u32, quest_status: Option<u32>) -> Option<u32> {
    if npc_flags & npc_flags::GOSSIP != 0 {
        Some(34)
    } else if npc_flags & npc_flags::QUESTGIVER != 0 && questgiver_has_quest(quest_status) {
        Some(18)
    } else if npc_flags & npc_flags::VENDOR != 0 {
        Some(28)
    } else {
        None
    }
}

/// Queue one click command and answer with the `bn_net_send` kind the host logs it under, or 0
/// when the session thread is not there to take it.
fn queued(cmd: Cmd, kind: u32) -> u32 {
    if send_cmd(cmd) {
        kind
    } else {
        0
    }
}

/// The world click's whole gesture on a picked object, which is the middle layer's decision, not
/// the host's: `button` 0 = left, 1 = right, as the settled click of `bn_ui_mouse_button`'s bits 2
/// and 3 reports it. The left button selects and nothing else (`0x493540`); the right one selects
/// first (`0x492820`) and then runs the unit dispatcher's branch (`0x60bea0`): the loot fork for a
/// corpse, `CanAttack` for anything hostile, else the service ladder.
///
/// Returns the `bn_net_send` kind queued for the branch (2 attack, 4 loot, 5 use, 18/28/34 a
/// service hello), or 3 for a select alone. An unresolved faction is read as hostile, the way the
/// reference reads a data gap: a missing DBC row must never cost the player their swing.
#[no_mangle]
pub extern "C" fn bn_net_unit_click(button: u32, guid: u64) -> u32 {
    let Some(unit) = ui_unit(guid) else {
        return 0;
    };
    if button == 0 {
        return queued(Cmd::Target(guid), 3);
    }
    // A GameObject is used and never selected (`OnUse 0x5f8660`).
    if unit.kind == 5 {
        return queued(Cmd::GameObjUse(guid), 5);
    }
    // The click's entry selects first, whatever leg follows (`0x492820`).
    send_cmd(Cmd::Target(guid));
    // The dead fork (`0x60bf75`) is terminal and never reaches a service: a corpse with loot
    // loots and one without does nothing - which is why a kill used to look like it dropped
    // nothing. `UNIT_DYNFLAG_DEAD` is the body still in its death animation, not yet a corpse.
    let dead = unit.fields.unit_is_dead();
    if unit.kind == 7 || (dead && !unit.fields.unit_dynflag_dead() && unit.fields.unit_lootable()) {
        return queued(Cmd::Loot(guid), 4);
    }
    if dead {
        return 3;
    }
    if crate::ui_feed::attackable(guid) {
        return queued(Cmd::Attack(guid), 2);
    }
    // The alive branch (`0x60c162`): `UNIT_NPC_FLAGS`, gated by the cached quest status.
    match service_send(unit.fields.unit_npc_flags(), ui_quest_status(guid)) {
        Some(34) => queued(Cmd::GossipHello(guid), 34),
        Some(18) => queued(Cmd::GiverHello(guid), 18),
        Some(28) => queued(Cmd::VendorList(guid), 28),
        _ => 3,
    }
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

/// The live group snapshot as the host draws it. Returns 1 while in a group
/// or with a pending invite, 0 otherwise (the caller then skips the frames).
#[no_mangle]
pub extern "C" fn bn_net_group(out: *mut BnNetParty) -> u32 {
    if out.is_null() {
        return 0;
    }
    let party = *shared().party.lock().unwrap_or_else(|e| e.into_inner());
    let active = party.in_group != 0 || party.invite != 0;
    unsafe {
        *out = party;
    }
    active as u32
}

/// The open vendor/shop window; returns the number of stock rows (0 = closed).
#[no_mangle]
pub extern "C" fn bn_net_vendor(out: *mut BnNetVendor) -> u32 {
    if out.is_null() {
        return 0;
    }
    let window = *shared().vendor.lock().unwrap_or_else(|e| e.into_inner());
    let count = window.count;
    unsafe {
        *out = window;
    }
    count
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

/// Feed the host's latest player pose so the world worker can echo it back to
/// the server as a movement heartbeat. Positions are raw WoW world yards, yaw
/// is radians (0 = +X, same convention `player` uses). The server owns the
/// authoritative position; this only keeps it in sync with the client's local
/// simulation so rubber-banding stops.
#[no_mangle]
pub extern "C" fn bn_net_set_pose(x: f32, y: f32, z: f32, yaw: f32) -> u32 {
    *shared().last_pose.lock().unwrap_or_else(|e| e.into_inner()) = Some(([x, y, z], yaw));
    1
}

/// Feed the host's latest directional input: (forward, strafe, turn) each -1..1.
/// Positive forward = W, negative = S. Positive strafe = D, negative = A.
/// Positive turn = right, negative = left. The world worker uses these to
/// drive the MSG_MOVE_START/STOP opcode edges vmangos requires — only
/// heartbeats would leave the server stuck with FORWARD set forever.
///
/// While the stock UI's bindings hold a movement key (`ui_set_move`), the
/// heartbeats follow the bindings and the host's raw key state is ignored, so
/// the server sees exactly the axes the world frame started.
#[no_mangle]
pub extern "C" fn bn_net_set_input(forward: f32, strafe: f32, turn: f32) -> u32 {
    if UI_MOVE.load(Ordering::SeqCst) {
        return 1;
    }
    *shared()
        .last_input
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = (forward, strafe, turn);
    1
}

/// The stock UI's held axes, written by the binding drain each frame:
/// `held` marks a binding-owned axis, after which the host's per-frame
/// `bn_net_set_input` no longer writes the heartbeat inputs.
static UI_MOVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn ui_set_move(forward: f32, strafe: f32, turn: f32, held: bool) {
    UI_MOVE.store(held, Ordering::SeqCst);
    *shared()
        .last_input
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = (forward, strafe, turn);
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

/// The guid a unit token names right now (`"player"`, `"target"`, `"party1"`, a `target` hop
/// chain), 0 when it names no live unit. The interface's own resolver state answers it, so a host
/// that bakes a unit's portrait finds the body the same way the frames do.
#[no_mangle]
pub extern "C" fn bn_net_token_guid(token: *const c_char) -> u64 {
    let Some(token) = crate::cstr_to_str(token) else {
        return 0;
    };
    crate::ui_feed::token_guid(token)
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

/// The visible-item slots the three hands read from, in `WeaponAttackType` order
/// (mainhand, offhand, ranged): the reference's `PLAYER_HELD_SLOTS`.
const PLAYER_HELD_SLOTS: [u8; 3] = [15, 16, 17];

/// One tracked unit's three weapon slots as `(display, class, subclass, material,
/// inventory_type, sheath)` per hand. A creature's come from
/// `UNIT_VIRTUAL_ITEM_SLOT_DISPLAY`/`UNIT_VIRTUAL_ITEM_INFO`, which the server only
/// fills for units; a player's come from the worn entry in `PLAYER_VISIBLE_ITEM`
/// resolved through its template, exactly the reference's pass 1
/// (`benilla-app/src/entities/equipment/resolve.rs`: `EntityKind::Player =>
/// player_visible_item_entry(PLAYER_HELD_SLOTS[slot])` then `templates.held(entry)`).
/// benilla's own rule (`placement`) turns these into an attachment slot.
/// Always writes 3 entries (a 0 display means "nothing there").
#[no_mangle]
pub extern "C" fn bn_net_unit_virtual_items(
    guid: u64,
    out_displays: *mut u32,
    out_info: *mut u8,
    out_sheath: *mut u8,
) -> u32 {
    if out_displays.is_null() || out_info.is_null() || out_sheath.is_null() {
        return 0;
    }
    // The same lock order as `inventory_rows`: units, then templates.
    let sh = shared();
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rec) = units.get(&guid) else {
        return 0;
    };
    let templates = sh.item_templates.lock().unwrap_or_else(|e| e.into_inner());
    for slot in 0..3u8 {
        let s = slot as usize;
        let (disp, (c, sc, m, it), sheath) = if rec.kind == 4 {
            let entry = rec
                .fields
                .player_visible_item_entry(PLAYER_HELD_SLOTS[s])
                .unwrap_or(0);
            match templates.get(&entry) {
                Some(t) => (
                    t.display_id,
                    (
                        t.class as u8,
                        t.subclass as u8,
                        t.material as u8,
                        t.inventory_type as u8,
                    ),
                    t.sheath as u8,
                ),
                None => (0, (0, 0, 0, 0), 0),
            }
        } else {
            (
                rec.fields.unit_virtual_item_display(slot).unwrap_or(0),
                rec.fields
                    .unit_virtual_item_info(slot)
                    .unwrap_or((0, 0, 0, 0)),
                rec.fields.unit_virtual_item_sheath(slot).unwrap_or(0),
            )
        };
        unsafe {
            *out_displays.add(s) = disp;
            *out_info.add(s * 4) = c;
            *out_info.add(s * 4 + 1) = sc;
            *out_info.add(s * 4 + 2) = m;
            *out_info.add(s * 4 + 3) = it;
            *out_sheath.add(s) = sheath;
        }
    }
    3
}

/// A unit's `UNIT_FIELD_BYTES_2` sheath byte: 0 stowed, 1 melee drawn, 2 ranged
/// drawn, or 0xFF while the object has not delivered that field. For our own
/// body it is whatever we last sent in `CMSG_SETSHEATHED`, which the server
/// echoes back (the reference's setter `0x611cf0`).
#[no_mangle]
pub extern "C" fn bn_net_unit_sheath(guid: u64) -> u32 {
    let sh = shared();
    let state = if guid == sh.self_guid.load(Ordering::SeqCst) {
        ui_self_fields().and_then(|f| f.unit_sheath_state())
    } else {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        units.get(&guid).and_then(|r| r.fields.unit_sheath_state())
    };
    state.map_or(0xFF, u32::from)
}

/// The AnimationData id of a main-hand melee swing for `guid`: the reference's `swing_anim_main`
/// (benilla-app/src/creature_anim/select.rs) over the unit's `UNIT_VIRTUAL_ITEM_INFO` main hand,
/// or, for our own body, the worn `PLAYER_VISIBLE_ITEM` entry's template. 16 (AttackUnarmed) when
/// no weapon template answers, which is the clip a bare fist plays.
#[no_mangle]
pub extern "C" fn bn_unit_swing_anim(guid: u64) -> u16 {
    let sh = shared();
    if guid == sh.self_guid.load(Ordering::SeqCst) {
        return self_main_swing(sh);
    }
    let (class, subclass) = {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        units
            .get(&guid)
            .and_then(|r| r.fields.unit_virtual_item_info(0))
            .map_or((0, 0), |(c, sc, _, _)| (u32::from(c), u32::from(sc)))
    };
    swing_anim_main(class, subclass)
}

/// The equipped main hand as the swing clip reads it.
#[derive(Clone, Copy, Default)]
struct MainHand {
    /// The worn entry `PLAYER_VISIBLE_ITEM_1_0`, the field the held weapon's
    /// model is resolved from; 0 while nothing is worn.
    worn: u32,
    class: u32,
    subclass: u32,
}

/// The main hand's class and subclass, (0, 0) while bare. The worn entry
/// answers first: it is what the held model is drawn from, and it outlives the
/// equipped item's own update object, which the stream can drop mid-fight -
/// reading the clip off that object left a drawn weapon swinging unarmed. The
/// item object's row is the fallback for a body whose fields have not carried
/// the worn entry yet.
fn self_main_hand(sh: &Shared) -> MainHand {
    let guid = sh.self_guid.load(Ordering::SeqCst);
    let worn = if guid == 0 {
        None
    } else {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        units
            .get(&guid)
            .and_then(|r| r.fields.player_visible_item_entry(PLAYER_HELD_SLOTS[0]))
    };
    let worn_class = worn.and_then(|entry| {
        let templates = sh.item_templates.lock().unwrap_or_else(|e| e.into_inner());
        templates.get(&entry).map(|t| (t.class, t.subclass))
    });
    let row_class = if worn_class.is_none() {
        // INV_SLOT_MAINHAND = 15.
        inventory_rows(sh)
            .iter()
            .find(|r| r.bag == BAG_EQUIPPED && r.slot == 15)
            .map(|r| (r.info.class, r.info.subclass))
    } else {
        None
    };
    let (class, subclass) = main_hand_class(worn_class, row_class);
    MainHand {
        worn: worn.unwrap_or(0),
        class,
        subclass,
    }
}

/// The worn entry wins over the equipped item object's row.
fn main_hand_class(worn: Option<(u32, u32)>, row: Option<(u32, u32)>) -> (u32, u32) {
    worn.or(row).unwrap_or((0, 0))
}

fn self_main_swing(sh: &Shared) -> u16 {
    let hand = self_main_hand(sh);
    swing_anim_main(hand.class, hand.subclass)
}

/// [`bn_unit_swing_anim`]'s table: only class 2 (weapon) picks a weapon clip, by subclass — 1H
/// axe/mace/sword/exotic/misc, 2H, polearm/staff/spear/fishing pole, dagger, everything else
/// unarmed.
fn swing_anim_main(class: u32, subclass: u32) -> u16 {
    if class != 2 {
        return 16;
    }
    match subclass {
        0x0 | 0x4 | 0x7 | 0xb | 0xe => 17,
        0x1 | 0x5 | 0x8 | 0xc => 18,
        0x6 | 0xa | 0x11 | 0x14 => 19,
        0xf => 85,
        _ => 16,
    }
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

/// The unit's own gait off the server's spline: a live spline plays Run when it carried
/// `SPLINEFLAG_RUNMODE`, else Walk; a finished spline stands (AnimationData 0).
fn spline_gait(rec: &UnitRec) -> u32 {
    match rec.spline.as_ref() {
        Some(s) if s.running(std::time::Instant::now()) => {
            if s.run_mode {
                5
            } else {
                4
            }
        }
        _ => 0,
    }
}

/// `CreatureTemplate.type_flags` bit that refuses the wound flinch and the parry (`0x6125f0`).
const DO_NOT_PLAY_WOUND_ANIM: u32 = 0x8;

/// The victim's defense reaction (`$CPP`, `0x624a01` → `0x60ec00`): dodge, shield block, and a
/// parry picked by the mainhand's subclass through the LUT at `0x60ec98`, bailing with no clip on
/// anything else (`0x60ec3a`). `None` for a state that carries no defense.
fn defense_anim(victim_state: u32, main: Option<(u8, u8)>) -> Option<u16> {
    match victim_state {
        2 | 8 => Some(30), // DODGE / DEFLECTS → Dodge
        5 => Some(24),     // BLOCKS → ShieldBlock
        3 => match main {
            Some((2, sub)) => match sub {
                0 | 4 | 7 | 0xb | 0xe | 0xf => Some(21), // 1H family, misc, dagger
                1 | 5 | 8 | 0xc => Some(22),             // 2H family
                6 | 0xa | 0x11 => Some(23),              // polearm, staff, spear
                0xd => Some(20),                         // fist → ParryUnarmed
                _ => None,
            },
            _ => None, // empty or non-weapon mainhand: bail
        },
        _ => None,
    }
}

/// The victim's wound flinch (`0x60ea70`), by severity and engagement alone: a crit (`HitInfo &
/// 0x80`) CombatCritical, an engaged victim CombatWound, else StandWound.
fn wound_anim(hit_info: u32, engaged: bool) -> u16 {
    if hit_info & 0x80 != 0 {
        10
    } else if engaged {
        9
    } else {
        8
    }
}

/// Whether the wound takes the full body rather than the upper body (`0x60eae8`, `0x60eb9a`):
/// a ready stance under it, or StandWound on a unit that is neither moving nor mounted. The FFI
/// carries no ready stance (only the spline's gait), so the first clause cannot fire here.
fn wound_full_body(id: u16, base_anim: u32, moving: bool, mounted: bool) -> bool {
    matches!(base_anim, 25..=29) || (id == 8 && !mounted && !moving)
}

/// The victim's mainhand weapon `(class, subclass)` as the parry LUT reads it: a player's worn
/// entry through its template, a creature's `UNIT_VIRTUAL_ITEM_INFO[0]`.
fn victim_mainhand(sh: &Shared, guid: u64) -> Option<(u8, u8)> {
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let rec = units.get(&guid)?;
    if rec.kind == 4 {
        let entry = rec.fields.player_visible_item_entry(15).unwrap_or(0);
        let templates = sh.item_templates.lock().unwrap_or_else(|e| e.into_inner());
        let t = templates.get(&entry)?;
        Some((t.class as u8, t.subclass as u8))
    } else {
        rec.fields
            .unit_virtual_item_info(0)
            .filter(|&(class, _, _, _)| class != 0)
            .map(|(class, sub, _, _)| (class, sub))
    }
}

/// The one reaction clip a melee swing asks of its victim: the defense arm first (a defended hit
/// never flinches, `0x624552`), else the wound flinch, with the full-body choice. `None` when the
/// victim is dead, the template refuses it, or the swing dealt no damage.
fn victim_reaction(sh: &Shared, s: &AttackerState) -> Option<(u16, bool)> {
    let engaged = sh
        .engaged
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&s.victim);
    let (entry, base_anim, moving, mounted) = {
        let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
        let rec = units.get(&s.victim)?;
        // `0x624552`: a lootable corpse shows neither the flinch nor the spurt.
        if rec.fields.unit_is_dead() || rec.fields.unit_lootable() {
            return None;
        }
        let entry = rec
            .fields
            .raw_fields()
            .find(|(index, _)| *index == 3)
            .map(|(_, value)| value)
            .unwrap_or(0);
        let gait = spline_gait(rec);
        (
            entry,
            gait,
            gait != 0,
            rec.fields.unit_mount_display_id() != 0,
        )
    };
    let no_wound = sh
        .creatures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry)
        .is_some_and(|t| t.type_flags & DO_NOT_PLAY_WOUND_ANIM != 0);
    if let Some(anim) = defense_anim(s.victim_state, victim_mainhand(sh, s.victim)) {
        // The flag refuses only the parry (`0x60ec1f`); dodge and block are ungated.
        if s.victim_state == 3 && no_wound {
            return None;
        }
        // `0x60ec00` goes through the unit's animation request, whose mask rule
        // (`0x5fe6a1`) puts an upper-body id on the key bone only while the unit turns,
        // moves or is not standing: a still, unmounted victim takes the whole body.
        return Some((anim, !moving && !mounted));
    }
    if !matches!(s.victim_state, 1 | 4) || s.damage == 0 || no_wound {
        return None;
    }
    let id = wound_anim(s.hit_info, engaged);
    Some((id, wound_full_body(id, base_anim, moving, mounted)))
}

/// The melee spurt's inputs (`0x624530` → `0x625010`): the client's gate (`HitInfo & 0x2`,
/// damage, victim state 1 or 4), front/back by where the attacker stands, and the crushing
/// `0x2000` bit for the large row. Returns `(display id, attach 15 front / 16 back, large)`.
fn melee_blood(sh: &Shared, s: &AttackerState) -> Option<(i32, u8, bool)> {
    if s.hit_info & 0x2 == 0 || s.damage == 0 || !matches!(s.victim_state, 1 | 4) {
        return None;
    }
    let units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    let victim = units.get(&s.victim)?;
    if victim.fields.unit_lootable() {
        return None;
    }
    let display = victim.fields.unit_displayid().unwrap_or(0);
    if display == 0 {
        return None;
    }
    // The sign of `victimForward · (attackerPos − victimPos)`; an unresolved attacker counts
    // as front, as the app's `blood.rs` does.
    let (vpos, facing) = effective_pose(victim);
    let front = units
        .get(&s.attacker)
        .map(|a| {
            let (apos, _) = effective_pose(a);
            facing.cos() * (apos[0] - vpos[0]) + facing.sin() * (apos[1] - vpos[1]) >= 0.0
        })
        .unwrap_or(true);
    Some((
        display,
        if front { 15 } else { 16 },
        s.hit_info & 0x2000 != 0,
    ))
}

fn unit_row(guid: u64, rec: &UnitRec) -> BnNetUnit {
    let f = &rec.fields;
    let (pos, facing) = effective_pose(rec);
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
        // A gameobject's look never rides `UNIT_FIELD_DISPLAYID`: the reference
        // reads `GAMEOBJECT_DISPLAYID` for kind 5 (`live_display.rs`).
        display_id: if rec.kind == 5 {
            f.gameobject_displayid().unwrap_or(0)
        } else {
            f.unit_displayid().unwrap_or(0)
        },
        health: f.unit_health().unwrap_or(0),
        max_health: f.unit_max_health().unwrap_or(0),
        target: f.unit_target().unwrap_or(0),
        mount: f.unit_mount_display_id(),
        dead: f.unit_is_dead() as u8,
        x: pos[0],
        y: pos[1],
        z: pos[2],
        facing,
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
        // Gait straight off the server's spline: a live spline plays Run when it
        // carried `SPLINEFLAG_RUNMODE`, else Walk (`MOVEFLAG_WALK_MODE`); once
        // the spline's duration is over the unit has arrived and stands. This is
        // the same source the Bevy client's gait picker reads (`Spline::speed`
        // exists for it), so the host only mirrors an id, it never guesses one.
        // A dead unit whose dynamic flags still carry bit 0 (lootable, `0x600680`'s
        // input): the corpse sparkle lives until the server clears the bit after
        // the loot is emptied.
        lootable: (f.unit_is_dead() && f.unit_lootable()) as u8,
        anim: spline_gait(rec),
        npc_flags: f.unit_npc_flags(),
    }
}

/// Drop the tracked objects, e.g. after leaving the world.
#[no_mangle]
pub extern "C" fn bn_net_units_clear() -> u32 {
    let sh = shared();
    sh.units.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
    let templates = sh.item_templates.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_) = templates.get(&entry) else {
        return 0;
    };
    let row = item_info_row(entry, &templates);
    unsafe { *out = row };
    1
}

#[cfg(test)]
mod tests {
    use super::{
        auto_equip_slots, clear_swing_refusal, defense_anim, equip_destination, find_equip_slot,
        latch_swing_refusal, main_hand_class, row_pos_for_wire, swing_anim_main,
        take_due_swing_refusal, wound_anim, wound_full_body, BAG_EQUIPPED,
    };

    /// A one-handed weapon reaches the off hand only for a dual wielder, as
    /// `Player::FindEquipSlot` gives it; every other type keeps the client's own map.
    #[test]
    fn an_offhand_weapon_needs_dual_wield() {
        assert_eq!(auto_equip_slots(13, false, true), vec![16, 17]);
        assert_eq!(auto_equip_slots(13, false, false), vec![16]);
        assert_eq!(auto_equip_slots(17, false, false), vec![16]);
        assert_eq!(auto_equip_slots(14, false, false), vec![17]);
    }

    /// The wire seat a player-array source names maps back to the row: worn 0..18 stays, the
    /// backpack's 23..38 is row bag 0 slot 0..15, and a worn bag's own slot is unchanged.
    #[test]
    fn a_wire_position_maps_back_to_its_row() {
        assert_eq!(row_pos_for_wire(BAG_EQUIPPED, 4), (BAG_EQUIPPED, 4));
        assert_eq!(row_pos_for_wire(BAG_EQUIPPED, 23), (0, 0));
        assert_eq!(row_pos_for_wire(BAG_EQUIPPED, 38), (0, 15));
        assert_eq!(row_pos_for_wire(19, 3), (19, 3));
    }

    /// A failed `CMSG_AUTOEQUIP_ITEM` becomes a swap to this slot: the first empty candidate of
    /// the template's mask, the first when every candidate is worn, and the ammo seat (live 0)
    /// when the item is a projectile.
    #[test]
    fn an_equip_lands_in_its_first_empty_candidate() {
        assert_eq!(equip_destination(&[11, 12], |live| live == 11), Some(12));
        assert_eq!(equip_destination(&[11, 12], |_| true), Some(11));
        assert_eq!(equip_destination(&[0], |_| true), Some(0));
        assert_eq!(equip_destination(&[], |_| false), None);
    }

    /// The masks the destination is picked from: rings and trinkets fill both seats, the ranged
    /// types swap places with the relic's, and a bag's seats are the worn bag slots.
    #[test]
    fn the_equip_masks_name_the_reference_slots() {
        assert_eq!(find_equip_slot(11, false), vec![11, 12]);
        assert_eq!(find_equip_slot(13, false), vec![16, 17]);
        assert_eq!(find_equip_slot(15, false), vec![18]);
        assert_eq!(find_equip_slot(15, true), Vec::<u8>::new());
        assert_eq!(find_equip_slot(28, true), vec![18]);
        assert_eq!(find_equip_slot(24, false), vec![0]);
        assert_eq!(find_equip_slot(18, false), vec![20, 21, 22, 23]);
        assert_eq!(find_equip_slot(0, false), Vec::<u8>::new());
    }

    /// The same rows benilla-app's `select::tests` pins: the reference's `swing_anim_main` table,
    /// so the host's swing clip is the one the real client plays for that weapon.
    #[test]
    fn swing_anim_main_matches_the_reference_table() {
        assert_eq!(swing_anim_main(2, 7), 17); // 1H sword
        assert_eq!(swing_anim_main(2, 5), 18); // 2H mace
        assert_eq!(swing_anim_main(2, 0xa), 19); // staff
        assert_eq!(swing_anim_main(2, 0x14), 19); // fishing pole
        assert_eq!(swing_anim_main(2, 0xf), 85); // dagger stabs, not Attack1H
        assert_eq!(swing_anim_main(2, 0xd), 16); // fist swings unarmed
        assert_eq!(swing_anim_main(2, 2), 16); // bow in melee swings unarmed
        assert_eq!(swing_anim_main(4, 6), 16); // non-weapon class
        assert_eq!(swing_anim_main(0, 0), 16); // no virtual item at all
    }

    /// The clip rides the worn entry, so a drawn weapon whose own update object
    /// left the stream still swings Attack1H; only a body with no worn entry
    /// falls back to the item object's row.
    #[test]
    fn the_swing_clip_follows_the_worn_entry() {
        assert_eq!(main_hand_class(Some((2, 7)), None), (2, 7));
        assert_eq!(main_hand_class(None, Some((2, 5))), (2, 5));
        assert_eq!(main_hand_class(Some((2, 7)), Some((4, 0))), (2, 7));
        assert_eq!(main_hand_class(None, None), (0, 0));
    }

    /// `0x5ec950`: a latched refusal is due at once, repeats only after the 4 s
    /// re-arm (`0x5ec9b6`), and a landed swing clears it (`0x5ecdb0(0)`).
    #[test]
    fn a_refusal_line_shows_once_then_rearms() {
        clear_swing_refusal();
        assert_eq!(take_due_swing_refusal(), None);
        latch_swing_refusal(2);
        assert_eq!(take_due_swing_refusal(), Some(2));
        assert_eq!(take_due_swing_refusal(), None, "re-armed four seconds out");
        clear_swing_refusal();
        assert_eq!(take_due_swing_refusal(), None);
    }

    /// The victim's defense clip (`0x60ec00`): dodge and deflect, shield block, and the
    /// parry chosen from the mainhand subclass through the LUT at `0x60ec98`.
    #[test]
    fn a_defended_hit_plays_its_own_reaction() {
        assert_eq!(defense_anim(2, None), Some(30)); // DODGE
        assert_eq!(defense_anim(8, None), Some(30)); // DEFLECTS
        assert_eq!(defense_anim(5, None), Some(24)); // BLOCKS
        assert_eq!(defense_anim(3, Some((2, 7))), Some(21)); // 1H sword parries
        assert_eq!(defense_anim(3, Some((2, 5))), Some(22)); // 2H mace
        assert_eq!(defense_anim(3, Some((2, 6))), Some(23)); // staff
        assert_eq!(defense_anim(3, Some((2, 0xd))), Some(20)); // fist: ParryUnarmed
        assert_eq!(defense_anim(3, Some((4, 6))), None); // not a weapon: no clip
        assert_eq!(defense_anim(3, None), None);
        assert_eq!(defense_anim(1, None), None); // a landed hit carries no defense
    }

    /// The wound flinch (`0x60ea70`) is picked by severity alone: a crit, an engaged victim,
    /// else the standing wound.
    #[test]
    fn the_wound_flinch_follows_severity_and_engagement() {
        assert_eq!(wound_anim(0x80, false), 10); // CombatCritical
        assert_eq!(wound_anim(0, true), 9); // CombatWound
        assert_eq!(wound_anim(0, false), 8); // StandWound
    }

    /// The full-body choice (`0x60eb9a`): StandWound takes the whole body only on a unit that
    /// stands still and is not mounted; the moving wound clips are upper-body only.
    #[test]
    fn only_a_still_standing_wound_takes_the_whole_body() {
        assert!(wound_full_body(8, 0, false, false));
        assert!(!wound_full_body(8, 4, true, false)); // walking
        assert!(!wound_full_body(8, 0, false, true)); // mounted
        assert!(!wound_full_body(9, 0, false, false)); // CombatWound stays upper-body
        assert!(wound_full_body(9, 27, false, false)); // a ready stance takes it all
    }
}

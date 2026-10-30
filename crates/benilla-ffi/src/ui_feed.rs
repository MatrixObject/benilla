//! The in-game UI feed: pushes the live session into the stock FrameXML's model, and drains the
//! VM's intents back into net commands.
//!
//! The stock interface is the source of truth for behaviour: this module only carries state in
//! (unit tokens, money, actions, spellbook, auras, loot, party, death) and carries the verbs it
//! queues back out (`UseAction`, `CastSpell`, selection, `LootSlot`, the death dialog, chat,
//! movement bindings). It never paints and never invents a widget.
//!
//! Borrowing rules: the game thread owns the VM. Every accessor here clones its snapshot under
//! the session lock and returns before any Lua runs; `WORLD`/`CATS`/`FEED` are thread-locals so
//! no `Lua` path can re-enter them.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};

use benilla_formats::{
    load_chr_classes, load_chr_races, load_creature_type_table, load_durability_tables,
    load_faction_catalog, load_item_display_catalog, load_quest_header_names, load_quest_tag_names,
    load_race_pvp_teams, load_skill_line_catalog, load_spell_catalog, ChrClasses, ChrRaces,
    CreatureTypeTable, DurabilityTables, FactionCatalog, FactionTemplate, ItemDisplayCatalog,
    QuestHeaderNames, QuestTagNames, Reaction, SkillLineCatalog, SpellCatalog,
};
use benilla_protocol::messages::{
    ObjectFields, BAG_PLAYER_INVENTORY, SLOT_BAG_FIRST, SLOT_PACK_FIRST,
};
use benilla_ui::script::{
    ActionSlot, ActionState, AuraState, BindingInput, ChatSend, ContainerSlot, ContainerState,
    ContainerUse, DeathAction, DeathUiState, FiredInput, HeldInput, InvSlotView, InventorySlots,
    LootRow, LootState, MerchantItem, MerchantState, NearestMode, PartyMemberInfo, PartyRequest,
    PartyState, PlayerRecord, QuestAction, ScriptCall, ScriptValue, SelectionRequest,
    SessionRequest, SpellBookState, SpellSlotView, SpellTabView, UiScript, UnitBase,
    UnitCombatStats, UnitGuids, UnitState, UnitTokenParse, UseArm, UseFork, VmKind, WornDisplay,
    EQUIPMENT_BAG,
};

use crate::net::{self, Cmd};
use crate::player;
use crate::BnWorld;

/// A missing `ChrClasses` row still needs a tab name; the reference's unresolved line reads
/// "General" with no icon.
const GENERAL_TAB: &str = "General";
/// `Ability_Kick`, the patch's General-tab face.
const GENERAL_TAB_ICON: &str = "Interface\\Icons\\Ability_Kick";
/// `UNIT_NPC_FLAG_REPAIR`, the bit `CanMerchantRepair` (`0x4fadb0`) tests.
const NPC_FLAG_REPAIR: u32 = 0x4000;
/// Melee auto-attack, the one spell `IsAttackAction` answers true for.
const ATTACK_SPELL: u32 = 6603;
/// The corpse-run window the server force-repops after.
const RELEASE_WINDOW: f64 = 360.0;

thread_local! {
    static WORLD: Cell<*mut BnWorld> = const { Cell::new(std::ptr::null_mut()) };
    static CATS: RefCell<Option<Catalogs>> = const { RefCell::new(None) };
    static FEED: RefCell<Feed> = RefCell::new(Feed::new());
}

/// Every client table the feed reads beside a descriptor, loaded once per UI load.
pub(crate) struct Catalogs {
    races: Option<ChrRaces>,
    classes: Option<ChrClasses>,
    spells: Option<SpellCatalog>,
    pub(crate) items: Option<ItemDisplayCatalog>,
    factions: Option<FactionCatalog>,
    creature_types: Option<CreatureTypeTable>,
    skills: Option<SkillLineCatalog>,
    /// `QuestHeaderList.dbc`: the `ZoneOrSort` id behind each quest-log group's title.
    pub(crate) quest_headers: Option<QuestHeaderNames>,
    /// `QuestInfo.dbc`: the bare tag word behind a quest-log row.
    pub(crate) quest_tags: Option<QuestTagNames>,
    /// `DurabilityCosts.dbc` + `DurabilityQuality.dbc`: the vendor's repair price.
    durability: Option<DurabilityTables>,
    race_teams: HashMap<u8, i8>,
}

impl Catalogs {
    fn load(world: &mut BnWorld) -> Self {
        let Some(chain) = world.chain.as_mut() else {
            return Self {
                races: None,
                classes: None,
                spells: None,
                items: None,
                factions: None,
                creature_types: None,
                skills: None,
                quest_headers: None,
                quest_tags: None,
                durability: None,
                race_teams: HashMap::new(),
            };
        };
        Self {
            races: load_chr_races(chain).ok(),
            classes: load_chr_classes(chain).ok(),
            spells: load_spell_catalog(chain).ok(),
            items: load_item_display_catalog(chain).ok(),
            factions: load_faction_catalog(chain).ok(),
            creature_types: load_creature_type_table(chain).ok(),
            skills: load_skill_line_catalog(chain).ok(),
            quest_headers: load_quest_header_names(chain).ok(),
            quest_tags: load_quest_tag_names(chain).ok(),
            durability: load_durability_tables(chain).ok(),
            race_teams: load_race_pvp_teams(chain).unwrap_or_default(),
        }
    }
}

/// Bind the feed to the world `bn_ui_load` is about to load the interface from.
pub(crate) fn attach(world: *mut BnWorld) {
    WORLD.with(|w| w.set(world));
    FEED.with(|f| *f.borrow_mut() = Feed::new());
    CATS.with(|c| *c.borrow_mut() = None);
    let Some(w) = (unsafe { world.as_mut() }) else {
        return;
    };
    let cats = Catalogs::load(w);
    CATS.with(|c| *c.borrow_mut() = Some(cats));
}

/// One row of the loot window: where its take goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LootPick {
    Coin,
    Item(u8),
}

/// The open loot window as the stock frame reads it: a layout fixed at open (`LootFrame.lua:22-37`,
/// `GetNumLootItems` read once at `:132`), where a taken row keeps its place as a `None` gap and the
/// stock handler hides that button in place (`LootFrame.lua:80`).
struct LootWindow {
    guid: u64,
    /// Whether a coin row leads; fixed at open from the response's gold (`LootState::open`).
    coin: bool,
    /// Each wire row's slot, in response order.
    slots: Vec<u8>,
    /// The 1-based layout row's take target, a gap included.
    picks: Vec<Option<LootPick>>,
    /// The cells last pushed, for the change edge: `None` before the first push.
    last: Option<Vec<Option<LootRow>>>,
    /// `LOOT_OPENED` has fired, which waits for every template (`0x4c1e9f`).
    opened: bool,
    /// The emptied window's release was sent; the server's answer closes it (`0x4c2a70` → `0x48f200`).
    released: bool,
}

/// The 1-based layout's take targets for one pass: the coin row while the pile is there, then each
/// fixed wire slot still in the response. A taken row keeps a `None` gap, so the rows below it do
/// not move and a click on the gap sends nothing.
fn loot_picks(window: &LootWindow, wire: &net::BnNetLoot) -> Vec<Option<LootPick>> {
    let live = |slot: u8| {
        wire.items
            .iter()
            .take(wire.count as usize)
            .any(|i| i.slot == slot)
    };
    let mut picks = Vec::with_capacity(window.slots.len() + 1);
    if window.coin {
        picks.push((wire.gold > 0).then_some(LootPick::Coin));
    }
    for &slot in &window.slots {
        picks.push(live(slot).then_some(LootPick::Item(slot)));
    }
    picks
}

/// The scalar fields whose move fires a unit event, diffed against the previous push.
#[derive(Clone, Default, PartialEq)]
struct Watch {
    health: u32,
    max_health: u32,
    power: u32,
    max_power: u32,
    power_type: u8,
    level: u32,
    flags: u32,
    dynamic_flags: u32,
    player_flags: u32,
    reaction: u8,
    dead: bool,
    ghost: bool,
    name: Option<String>,
}

impl From<&UnitState> for Watch {
    fn from(s: &UnitState) -> Self {
        Self {
            health: s.health,
            max_health: s.max_health,
            power: s.power,
            max_power: s.max_power,
            power_type: s.power_type,
            level: s.level,
            flags: s.flags,
            dynamic_flags: s.dynamic_flags,
            player_flags: s.player_flags,
            reaction: s.reaction,
            dead: s.dead,
            ghost: s.ghost,
            name: s.name.clone(),
        }
    }
}

/// What one UI load has already pushed, so an unchanged snapshot costs no Lua and an edge fires
/// its event exactly once.
struct Feed {
    /// Seconds since the load; the death clock and the slow pass both ride it.
    now: f64,
    /// Time until the next slow pass (units, actions, auras, loot, party).
    until_slow: f32,
    /// The first slow pass pushes the world whole without firing events; the interface has just
    /// loaded and must not hear changes from before it.
    first: bool,

    units: HashMap<&'static str, (Option<UnitState>, Watch)>,
    /// The guid each pushed token names, for the resolver's own bookkeeping.
    guids: UnitGuids,
    chain_guids: Vec<u64>,

    name: Option<String>,
    money: u32,
    xp: (u32, u32),
    rest: (u8, u32, bool),
    /// Whether the load's resting push has happened: `PlayerFrame_UpdateStatus`
    /// (PlayerFrame.lua:181) hides the attack/rest icons only when an event
    /// tells it, and without that first event they keep their XML default.
    rest_pushed: bool,
    combo: (u8, u64),
    level: u32,
    selection: u64,
    combat: bool,
    /// Auto-attack (`SMSG_ATTACKSTART`), which fires `PLAYER_ENTER_COMBAT` /
    /// `PLAYER_LEAVE_COMBAT` - the same `PlayerFrame_UpdateStatus` input.
    engaged: bool,
    dead: bool,
    ghost: bool,
    cast: Option<u32>,

    spells: Vec<u32>,
    actions: Vec<(u8, u8, u32)>,
    action_slots: HashMap<u32, ActionSlot>,
    action_states: HashMap<u32, ActionState>,
    auras: Vec<(u32, u8, bool)>,
    target_auras: (u64, Vec<(u32, u8, bool)>),
    /// The open loot window's fixed layout, `None` while closed.
    loot: Option<LootWindow>,
    /// The vendor last pushed: its guid plus each listed row's `(entry, price,
    /// remaining stock, per-buy count, template display id)`.
    merchant_key: Option<(u64, Vec<(u32, u32, u32, u32, u32)>)>,
    party_key: Option<Vec<(u64, String)>>,

    /// The bag containers last pushed, diffed per bag for `BAG_UPDATE`.
    containers: HashMap<i64, ContainerState>,
    /// The last doll snapshot, so the push and its `UNIT_INVENTORY_CHANGED` ride a real change.
    last_inv: Option<InventorySlots>,
    /// The last stat sheet, so its events ride a real change.
    last_stats: Option<UnitCombatStats>,
    /// The bag item in containers 1..=4, indexed `id - 1`: a change from nonzero is `BAG_CLOSED`.
    bag_guids: [u64; 4],
    /// The seats the watchers last saw, whose change is the server releasing a moved item's lock.
    last_seats: Seats,

    died_at: Option<f64>,
    corpse_ms: u32,
    corpse_at: Option<f64>,
    resurrect: (u64, bool, bool),

    held: HeldAxes,
}

/// Which binding-held movement axes are down; the world frame's `MoveForwardStart`/`Stop` pair
/// drives these instead of the host's raw key state.
#[derive(Clone, Copy, Default)]
struct HeldAxes {
    forward: bool,
    backward: bool,
    strafe_left: bool,
    strafe_right: bool,
    turn_left: bool,
    turn_right: bool,
}

impl HeldAxes {
    /// `0x7c5360`: A/D turn the body unless the look channel is held, when they
    /// strafe and no keyboard turn runs - the facing follows the camera.
    fn axes(&self, look: bool) -> (f32, f32, f32) {
        let f = f32::from(self.forward) - f32::from(self.backward);
        let s = f32::from(self.strafe_right) - f32::from(self.strafe_left);
        let t = f32::from(self.turn_left) - f32::from(self.turn_right);
        if look {
            // The turn keys read left-positive, the strafe axis right-positive,
            // so the fold flips the sign (`0x7c5360`: `turn_right - turn_left`).
            (f, s - t, 0.0)
        } else {
            (f, s, t)
        }
    }

    fn any(&self) -> bool {
        self.forward
            || self.backward
            || self.strafe_left
            || self.strafe_right
            || self.turn_left
            || self.turn_right
    }
}

impl Feed {
    fn new() -> Self {
        Self {
            now: 0.0,
            until_slow: 0.0,
            first: true,
            units: HashMap::new(),
            guids: UnitGuids::default(),
            chain_guids: Vec::new(),
            name: None,
            money: 0,
            xp: (0, 0),
            rest: (0, 0, false),
            rest_pushed: false,
            combo: (0, 0),
            level: 0,
            selection: 0,
            combat: false,
            engaged: false,
            dead: false,
            ghost: false,
            cast: None,
            spells: Vec::new(),
            actions: Vec::new(),
            action_slots: HashMap::new(),
            action_states: HashMap::new(),
            auras: Vec::new(),
            target_auras: (0, Vec::new()),
            loot: None,
            merchant_key: None,
            party_key: None,
            containers: HashMap::new(),
            last_inv: None,
            last_stats: None,
            bag_guids: [0; 4],
            last_seats: Seats::new(),
            died_at: None,
            corpse_ms: 0,
            corpse_at: None,
            resurrect: (0, false, false),
            held: HeldAxes::default(),
        }
    }
}

pub(crate) fn cstr<const N: usize>(buf: &[std::ffi::c_char; N]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn norm_icon(path: Option<String>) -> Option<String> {
    path.map(|p| p.replace('/', "\\"))
}

/// The coin row's text (`ui_loot`'s `format_money`): 1.12's `GOLD`, `SILVER` and `COPPER`
/// (`GlobalStrings.lua:2025`, `:3465`, `:865`), read off the player's own pack so a Chinese
/// install reads Chinese, with the enUS word only when the chain lacks the row.
fn money_text(script: &UiScript, copper: u32) -> String {
    let word = |key: &str, fallback: &str| {
        localized_label(script, key).unwrap_or_else(|| fallback.to_string())
    };
    let (g, s, c) = (copper / 10000, (copper % 10000) / 100, copper % 100);
    let mut parts: Vec<String> = Vec::new();
    if g > 0 {
        parts.push(format!("{g} {}", word("GOLD", "Gold")));
    }
    if s > 0 {
        parts.push(format!("{s} {}", word("SILVER", "Silver")));
    }
    if c > 0 || parts.is_empty() {
        parts.push(format!("{c} {}", word("COPPER", "Copper")));
    }
    parts.join(" ")
}

/// The client's quality colour escape for an item link, `0x52ad90` over the table at
/// `0x854124` (>= 7 clamps to white).
fn quality_color(quality: u32) -> &'static str {
    match quality {
        0 => "ff9d9d9d",
        2 => "ff1eff00",
        3 => "ff0070dd",
        4 => "ffa335ee",
        5 => "ffff8000",
        6 => "ffe6cc80",
        _ => "ffffffff",
    }
}

/// One item hyperlink as `0x52adb0` builds it: colour, `|Hitem:…|h[Name]|h`, reset.
pub(crate) fn item_link(item_id: u32, name: &str, quality: u32) -> String {
    format!(
        "|c{}|Hitem:{item_id}:0:0:0|h[{name}]|h|r",
        quality_color(quality)
    )
}

/// Every worn or carried item's repair cost summed (`0x4faf30` per item): the number
/// `GetRepairAllCost` answers, so the merchant's repair button enables and prices itself.
///
/// Deviation: the vendor's reputation discount (`0x612b80`) is not fed here, so the price reads
/// undiscounted. What the purse actually loses is the server's own answer.
fn repair_all_cost(cats: Option<&Catalogs>) -> u32 {
    let Some(tables) = cats.and_then(|c| c.durability.as_ref()) else {
        return 0;
    };
    net::ui_inventory()
        .into_iter()
        .filter(|it| it.guid != 0 && it.max_durability > it.durability)
        .map(|it| {
            tables.repair_cost(
                it.max_durability - it.durability,
                it.info.item_level,
                it.info.quality,
                it.info.class,
                it.info.subclass,
                0.0,
            )
        })
        .sum()
}

/// One template item's `(name, quality, icon)` off its `BnNetItemInfo` row and the display catalog;
/// the name is `None` while the item query is still out.
pub(crate) fn item_look(
    info: &net::BnNetItemInfo,
    cats: Option<&Catalogs>,
) -> (Option<String>, u32, Option<String>) {
    let name = cstr(&info.name);
    (
        (!name.is_empty()).then_some(name),
        info.quality,
        cats.and_then(|c| c.items.as_ref())
            .and_then(|t| t.get(info.display_id))
            .and_then(|d| norm_icon(d.icon.clone())),
    )
}

/// The 1-based live inventory slots a template's `InventoryType` equips into, the static mask
/// `IsValidForSlot` (`0x5da1d0`) reads; `benilla-app`'s `ui_items/mod.rs:869` `find_equip_slot`,
/// which is vmangos' `Player::FindEquipSlot`. Without it every bag item looks like a misfit, so a
/// drop on the doll takes the auto-equip arm instead of the swap and a `CursorCanGoInSlot`
/// highlight never lights.
fn find_equip_slot(inventory_type: u32, has_relic_slot: bool) -> Vec<u8> {
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

/// One occupied bag slot's view, off the item object and its template.
fn container_slot(
    item: &net::BnNetInvItem,
    cats: Option<&Catalogs>,
    has_relic_slot: bool,
) -> ContainerSlot {
    let info = &item.info;
    let name = cstr(&info.name);
    ContainerSlot {
        texture: cats
            .and_then(|c| c.items.as_ref())
            .and_then(|t| t.get(info.display_id))
            .and_then(|d| norm_icon(d.icon.clone())),
        count: item.count,
        // `GetContainerItemInfo` reports quality only when `inventory_type` is nonzero
        // (`0x4f9794`), else -1.
        quality: (info.inventory_type != 0).then_some(info.quality),
        inventory_type: info.inventory_type,
        equip_slots: find_equip_slot(info.inventory_type, has_relic_slot),
        item_id: item.entry,
        link: (!name.is_empty()).then(|| item_link(item.entry, &name, info.quality)),
        guid: item.guid,
        durability: (item.max_durability > 0).then_some((item.durability, item.max_durability)),
        ..Default::default()
    }
}

/// One worn slot's doll view, off the same row and template the bag containers read.
///
/// Deviation: the ffi's template cache carries no item flags, bonding, on-use spell or enchant
/// slots, so those stay at the reference's unresolved values (`None`/0) — what the app shows for a
/// template still in flight. The art, count, quality, name, link, durability and guid are complete.
fn inv_slot_view(
    item: &net::BnNetInvItem,
    cats: Option<&Catalogs>,
    has_relic_slot: bool,
) -> InvSlotView {
    let (name, quality, icon) = item_look(&item.info, cats);
    InvSlotView {
        item_id: item.entry,
        icon,
        count: item.count,
        quality: quality as i32,
        name: name.clone(),
        durability: (item.max_durability > 0).then_some((item.durability, item.max_durability)),
        link: name.map(|n| item_link(item.entry, &n, quality)),
        guid: item.guid,
        equip_slots: find_equip_slot(item.info.inventory_type, has_relic_slot),
        ..Default::default()
    }
}

/// The doll snapshot the stock `GetInventoryItem*` family reads. The live ids 1..=23 are the
/// player-array seats one below their wire index (the app's `ui_items/mod.rs:90` `EQUIPMENT_BAG`
/// arm), and the ffi's rows already carry that wire index, so this is the same addressing a
/// bag→doll drag sends. Slot 0, ammo, is `PLAYER_FIELD_AMMO_ID`'s template-only pseudo-item, which
/// the inventory rows do not carry, so it stays empty.
fn inventory_slots(
    inv: &[net::BnNetInvItem],
    cats: Option<&Catalogs>,
    has_relic_slot: bool,
) -> InventorySlots {
    let mut out: InventorySlots = Default::default();
    for (live, slot) in out.iter_mut().enumerate().skip(1) {
        *slot = inv
            .iter()
            .find(|r| r.bag == net::BAG_EQUIPPED && usize::from(r.slot) == live - 1)
            .map(|r| inv_slot_view(r, cats, has_relic_slot));
    }
    out
}

/// The watched seats as the last pass saw them: the backpack (`bag` 0) and the player's own array
/// (`BAG_EQUIPPED`) keyed by that field, a bag container's seats keyed by the bag item's guid — the
/// reference's two array watchers (`0x4f8db0`, `0x5ddcf0`) and its container watcher (`0x4f912c`).
type Seats = HashMap<(u64, u8), u64>;

/// The seat watchers' unlocks between two looks: a seat whose guid changed unlocks the item that
/// left it (`0x5ddd50`, `0x4f8e15`, `0x4f912c`); a seat that vanished releases only when it is the
/// player's own, since a bag's seats go with it unseen. Sorted, so the release order is
/// reproducible.
///
/// Deviation: the app watches the player's array through its descriptors, so it covers the bank
/// (39-68) and keyring (81-112) seats too; this host's rows carry only the backpack, the equipped
/// array and the equipped bags' contents, which is what a bag drag moves.
fn seat_unlocks(
    last: &mut Seats,
    bags: &[(i64, u32, u32, u64)],
    inv: &[net::BnNetInvItem],
) -> Vec<u64> {
    // The equipped bag in live slot `19..=22`, which owns the rows that name it as their `bag`.
    let owner = |bag: u8| {
        bags.iter()
            .find(|&&(id, _, _, guid)| id == i64::from(bag) - 18 && guid != 0)
            .map(|&(_, _, _, guid)| guid)
    };
    let mut now: Seats = HashMap::new();
    for r in inv {
        if r.guid == 0 {
            continue;
        }
        let key = match r.bag {
            0 | net::BAG_EQUIPPED => Some(u64::from(r.bag)),
            19..=22 => owner(r.bag),
            _ => None,
        };
        let Some(key) = key else { continue };
        now.insert((key, r.slot), r.guid);
    }
    let mut out: Vec<u64> = last
        .iter()
        .filter_map(|(&seat, &old)| match now.get(&seat) {
            Some(&new) => (new != old).then_some(old),
            None => matches!(seat.0, 0 | 255).then_some(old),
        })
        .collect();
    out.sort_unstable();
    *last = now;
    out
}

/// `ITEM_CLASS_WEAPON` and the wand subclass (`ui_char.rs:28,30`), the two template rows the stat
/// sheet's off-hand and `HasWandEquipped` gates read.
const ITEM_CLASS_WEAPON: u32 = 2;
const SUBCLASS_WAND: u32 = 19;

/// The doll's stat sheet, off our own descriptor — the app's `ui_char.rs:402` `unit_combat_stats`
/// plus the player-only avoidance rows.
///
/// Deviation: the four weapon-skill pairs (`UnitAttackBothHands`, `UnitDefense`) need
/// `PLAYER_FIELD_SKILL_LINE`, which this host does not stream, so they stay `(0, 0)`.
fn combat_stats(f: &ObjectFields, inv: &[net::BnNetInvItem]) -> UnitCombatStats {
    /// `0` counts as unstreamed, or `damage / speed` divides by zero (`ui_char.rs:398`).
    fn base_swing(ms: Option<u32>) -> u32 {
        ms.filter(|v| *v != 0).unwrap_or(2000)
    }
    let mut stats = [0i32; 5];
    let mut stat_pos = [0i32; 5];
    let mut stat_neg = [0i32; 5];
    for i in 0..5u8 {
        stats[usize::from(i)] = f.unit_stat(i).unwrap_or(0) as i32;
        stat_pos[usize::from(i)] = f.player_posstat(i).unwrap_or(0);
        stat_neg[usize::from(i)] = f.player_negstat(i).unwrap_or(0);
    }
    let mut resistances = [0i32; 7];
    let mut resistance_pos = [0i32; 7];
    let mut resistance_neg = [0i32; 7];
    for i in 0..7u8 {
        resistances[usize::from(i)] = f.unit_resistance(i).unwrap_or(0);
        resistance_pos[usize::from(i)] = f.player_resistance_buff_pos(i).unwrap_or(0);
        resistance_neg[usize::from(i)] = f.player_resistance_buff_neg(i).unwrap_or(0);
    }
    let (ap_pos, ap_neg) = f.unit_attack_power_mods();
    let (rap_pos, rap_neg) = f.unit_ranged_attack_power_mods();
    // The off hand counts only as a weapon (a shield does not swing), the ranged row's wand
    // subclass gates `HasWandEquipped`. Live 17 and 18 are the wire seats 16 and 17.
    let worn = |live: u8| {
        inv.iter()
            .find(|r| r.bag == net::BAG_EQUIPPED && r.slot == live - 1)
    };
    let weapon = |live: u8| {
        worn(live)
            .filter(|r| r.info.class == ITEM_CLASS_WEAPON)
            .is_some()
    };
    UnitCombatStats {
        stats,
        stat_pos,
        stat_neg,
        resistances,
        resistance_pos,
        resistance_neg,
        min_damage: f.unit_min_damage().unwrap_or(0.0),
        max_damage: f.unit_max_damage().unwrap_or(0.0),
        min_offhand_damage: f.unit_min_offhand_damage().unwrap_or(0.0),
        max_offhand_damage: f.unit_max_offhand_damage().unwrap_or(0.0),
        physical_bonus_pos: f.player_mod_damage_done_pos(0).unwrap_or(0),
        physical_bonus_neg: f.player_mod_damage_done_neg(0).unwrap_or(0),
        damage_percent: f.player_mod_damage_done_pct(0).unwrap_or(1.0),
        main_attack_time_ms: base_swing(f.unit_base_attack_time(0)),
        offhand_attack_time_ms: base_swing(f.unit_base_attack_time(1)),
        attack_power: f.unit_attack_power().unwrap_or(0),
        attack_power_pos: i32::from(ap_pos),
        attack_power_neg: i32::from(ap_neg),
        ranged_attack_power: f.unit_ranged_attack_power().unwrap_or(0),
        ranged_attack_power_pos: i32::from(rap_pos),
        ranged_attack_power_neg: i32::from(rap_neg),
        ranged_attack_time_ms: base_swing(f.unit_ranged_attack_time()),
        ranged_min_damage: f.unit_min_ranged_damage().unwrap_or(0.0),
        ranged_max_damage: f.unit_max_ranged_damage().unwrap_or(0.0),
        has_offhand: weapon(17),
        has_wand: worn(18)
            .is_some_and(|r| r.info.class == ITEM_CLASS_WEAPON && r.info.subclass == SUBCLASS_WAND),
        dodge_percent: f.player_dodge_percentage().unwrap_or(0.0),
        parry_percent: f.player_parry_percentage().unwrap_or(0.0),
        block_percent: f.player_block_percentage().unwrap_or(0.0),
        ..Default::default()
    }
}

/// Build one unit's snapshot from a live descriptor.
fn unit_state(
    guid: u64,
    kind: u8,
    fields: &ObjectFields,
    name: Option<String>,
    cats: Option<&Catalogs>,
    player: &ObjectFields,
) -> UnitState {
    let is_player = kind == 4;
    let power_type = fields.unit_power_type();
    let flags = fields.unit_flags();
    let race = is_player.then(|| fields.unit_race()).flatten();
    let class = is_player.then(|| fields.unit_class()).flatten();
    let charge = if is_player { player } else { fields };
    let template = cats
        .and_then(|c| c.factions.as_ref())
        .and_then(|f| fields.unit_faction_template().and_then(|t| f.template(t)));
    let player_template = cats
        .and_then(|c| c.factions.as_ref())
        .and_then(|f| player.unit_faction_template().and_then(|t| f.template(t)));
    let (reaction, can_attack, can_assist) = match (template, player_template) {
        (Some(t), Some(p)) if guid != 0 && !std::ptr::eq(t, p) => {
            let toward = t.reaction_toward(p);
            let hostile = toward == Reaction::Hostile || p.reaction_toward(t) == Reaction::Hostile;
            let friendly = toward == Reaction::Friendly;
            let r = match toward {
                Reaction::Hostile => 2,
                Reaction::Neutral => 4,
                Reaction::Friendly => 5,
            };
            (r, hostile, !hostile && friendly)
        }
        _ => (0, false, false),
    };
    let race_names = race.and_then(|r| {
        cats.and_then(|c| c.races.as_ref())
            .and_then(|t| t.names(u32::from(r)))
    });
    let class_names = class.and_then(|c| {
        cats.and_then(|x| x.classes.as_ref())
            .and_then(|t| t.name(u32::from(c)).zip(t.file(u32::from(c))))
    });
    let creature = if is_player {
        None
    } else {
        fields
            .object_entry()
            .filter(|&e| e != 0)
            .and_then(net::ui_creature)
    };
    let creature_type = creature.as_ref().map_or(0, |c| c.creature_type);
    let group_mask = template.map_or(0, |t| t.group_mask);
    UnitState {
        exists: true,
        has_object: true,
        is_connected: true,
        guid,
        name,
        health: fields.unit_shown_health().unwrap_or(0),
        max_health: fields.unit_max_health().unwrap_or(0),
        level: fields.unit_level().unwrap_or(0),
        power_type,
        power: fields.unit_shown_power(power_type).unwrap_or(0),
        max_power: fields.unit_shown_max_power(power_type).unwrap_or(0),
        dead: fields.unit_reads_dead(),
        ghost: fields.player_is_ghost(),
        charmed: fields.unit_charmed_by().is_some(),
        tapped: fields.unit_tapped(),
        tapped_by_player: fields.unit_tapped_by_player(),
        group_leader: fields.player_is_group_leader(),
        player_flags: fields.player_flags(),
        reaction,
        can_attack,
        can_assist,
        can_assist_player: can_assist,
        race: race_names.map(|(n, _)| n.to_string()),
        race_file: race_names.map(|(_, f)| f.to_string()),
        class: class_names.map(|(n, _)| n.to_string()),
        class_file: class_names.map(|(_, f)| f.to_string()),
        has_relic_slot: is_player
            && class.is_some_and(|c| {
                cats.and_then(|x| x.classes.as_ref())
                    .is_some_and(|t| t.has_relic_slot(u32::from(c)))
            }),
        damage_bonus_stat: is_player
            .then(|| {
                class.and_then(|c| {
                    cats.and_then(|x| x.classes.as_ref())
                        .and_then(|t| t.damage_bonus_stat(u32::from(c)))
                })
            })
            .flatten(),
        sex: match fields.unit_gender() {
            Some(0) => 2,
            Some(1) => 3,
            _ => 0,
        },
        is_player,
        player_controlled: flags & 0x8 != 0,
        flags,
        dynamic_flags: fields.unit_dynamic_flags(),
        owner: fields
            .unit_summoned_by()
            .or_else(|| fields.unit_charmed_by())
            .or_else(|| fields.unit_created_by())
            .unwrap_or(0),
        subtitle: creature.as_ref().map(|c| c.subname.clone()),
        creature_type_name: cats
            .and_then(|c| c.creature_types.as_ref())
            .and_then(|t| t.name(creature_type))
            .map(str::to_string),
        creature_type,
        rank: creature.as_ref().map_or(0, |c| c.rank),
        civilian: creature.as_ref().is_some_and(|c| c.civilian),
        racial_leader: creature.as_ref().is_some_and(|c| c.racial_leader),
        pvp: flags & 0x1000 != 0,
        skinnable: flags & 0x0400_0000 != 0,
        faction_name: template
            .and_then(|t| {
                cats.and_then(|c| c.factions.as_ref())
                    .and_then(|f| f.faction_name(t.faction))
            })
            .map(str::to_string),
        is_pvp_ffa: fields.player_flags() & 0x80 != 0,
        pvp_rank: fields.player_pvp_rank().unwrap_or(0),
        pvp_medal: fields.player_pvp_medal().unwrap_or(0),
        faction_group: cats
            .and_then(|c| c.factions.as_ref())
            .and_then(|f| f.faction_group_internal_name(group_mask & 6))
            .map(str::to_string),
        faction_group_localized: cats
            .and_then(|c| c.factions.as_ref())
            .and_then(|f| f.faction_group_name(group_mask & 6))
            .map(str::to_string),
        pvp_team: if is_player {
            race.and_then(|r| cats.and_then(|c| c.race_teams.get(&r).copied()))
                .unwrap_or(-1)
        } else {
            -1
        },
        in_combat: flags & 0x0008_0000 != 0,
        quest_log: if is_player {
            charge.player_quest_log_window()
        } else {
            Default::default()
        },
        ..Default::default()
    }
}

fn auras_of(fields: &ObjectFields, cats: Option<&Catalogs>) -> Vec<AuraState> {
    let mut out = Vec::new();
    for slot in 0..48u8 {
        let Some(a) = fields.unit_aura(slot) else {
            continue;
        };
        if a.spell_id == 0 {
            continue;
        }
        let display = cats
            .and_then(|c| c.spells.as_ref())
            .and_then(|s| s.get(a.spell_id));
        out.push(AuraState {
            spell_id: a.spell_id,
            name: display.map(|d| d.name.clone()),
            icon: display.and_then(|d| norm_icon(d.icon.clone())),
            count: a.stacks.max(1),
            debuff_type: display
                .and_then(|d| {
                    cats.and_then(|c| c.spells.as_ref())
                        .and_then(|s| s.dispel_name(d))
                })
                .map(str::to_string),
            duration: 0.0,
            expiration_time: 0.0,
            helpful: a.is_helpful(),
            cancelable: a.is_cancelable(),
            until_cancelled: true,
            channeled: false,
        });
    }
    out
}

fn aura_key(auras: &[AuraState]) -> Vec<(u32, u8, bool)> {
    auras
        .iter()
        .map(|a| (a.spell_id, a.count, a.helpful))
        .collect()
}

fn power_event(power_type: u8) -> &'static str {
    match power_type {
        1 => "UNIT_RAGE",
        2 => "UNIT_FOCUS",
        3 => "UNIT_ENERGY",
        4 => "UNIT_HAPPINESS",
        _ => "UNIT_MANA",
    }
}

fn max_power_event(power_type: u8) -> &'static str {
    match power_type {
        1 => "UNIT_MAXRAGE",
        2 => "UNIT_MAXFOCUS",
        3 => "UNIT_MAXENERGY",
        4 => "UNIT_MAXHAPPINESS",
        _ => "UNIT_MAXMANA",
    }
}

fn ev_str(name: &str) -> ScriptValue {
    ScriptValue::Str(name.to_string())
}

fn fire(script: &mut UiScript, event: &str, args: Vec<ScriptValue>) {
    script.fire_event(event, args);
}

/// One label off the player's own language pack: the `GlobalStrings` global FrameXML reads its
/// text through (`TEXT(BACKPACK_TOOLTIP)` for the bag window), `None` when the chain's Lua has no
/// such row. A middle-layer label is English in a Chinese install unless it comes from here.
fn localized_label(script: &UiScript, key: &str) -> Option<String> {
    script
        .lua()
        .globals()
        .get::<String>(key)
        .ok()
        .filter(|text| !text.is_empty())
}

/// The stock error line for a key: `DisplayError` (`0x4945b0`) resolves the id through
/// `GlobalStrings`, which is the player's own localized table, so read the VM's global
/// and fall back to the key only when the chain lacks the row.
fn fire_error(script: &mut UiScript, key: &str) {
    let text = localized_label(script, key).unwrap_or_else(|| key.to_string());
    fire(script, "UI_ERROR_MESSAGE", vec![ScriptValue::Str(text)]);
}

/// Diff one token's snapshot and fire the field events its change implies, once each.
fn fire_unit_diff(script: &mut UiScript, token: &str, old: &Watch, new: &Watch) {
    if old.health != new.health {
        fire(script, "UNIT_HEALTH", vec![ev_str(token)]);
    }
    if old.max_health != new.max_health {
        fire(script, "UNIT_MAXHEALTH", vec![ev_str(token)]);
    }
    if old.power_type != new.power_type {
        fire(script, "UNIT_DISPLAYPOWER", vec![ev_str(token)]);
    } else if old.power != new.power {
        fire(script, power_event(new.power_type), vec![ev_str(token)]);
    }
    if old.max_power != new.max_power {
        fire(script, max_power_event(new.power_type), vec![ev_str(token)]);
    }
    if old.level != new.level {
        fire(script, "UNIT_LEVEL", vec![ev_str(token)]);
    }
    if old.flags != new.flags {
        fire(script, "UNIT_FLAGS", vec![ev_str(token)]);
    }
    if old.dynamic_flags != new.dynamic_flags {
        fire(script, "UNIT_DYNAMIC_FLAGS", vec![ev_str(token)]);
    }
    if old.player_flags != new.player_flags {
        fire(script, "PLAYER_FLAGS_CHANGED", vec![ev_str(token)]);
    }
    if old.reaction != new.reaction {
        fire(script, "UNIT_FACTION", vec![ev_str(token)]);
    }
    // A name arrives after the unit: `SMSG_CREATURE_QUERY_RESPONSE` lands the
    // record (name, rank, subtitle) that the target frame reads, and the
    // reference fires both signals when it does (`names.rs`, `on_creature_name`).
    if old.name != new.name {
        fire(script, "UNIT_NAME_UPDATE", vec![ev_str(token)]);
        fire(script, "UNIT_CLASSIFICATION_CHANGED", vec![ev_str(token)]);
    }
}

impl Feed {
    /// Push one token's snapshot, firing the diff's events (unless this is the load's first pass).
    fn push_unit(&mut self, script: &mut UiScript, token: &'static str, state: Option<UnitState>) {
        let watch = state.as_ref().map_or(Watch::default(), Watch::from);
        let slot = self.units.entry(token).or_insert((None, Watch::default()));
        let existed = slot.0.is_some();
        let changed = match (slot.0.as_ref(), state.as_ref()) {
            (None, None) => false,
            (Some(a), Some(b)) => a != b,
            _ => true,
        };
        if changed {
            script.set_unit(token, state.clone());
        }
        // A token's first snapshot is news to the frames as much as a change is: the client
        // fires `UNIT_HEALTH` and its siblings when a unit's fields become known, and the stock
        // frames only learn their bar's max/value from that event (`PlayerFrame.lua:202`). The
        // load's first pass skips *changes* from before the interface existed, not arrivals.
        if state.is_some() && (!self.first || !existed) {
            fire_unit_diff(script, token, &slot.1, &watch);
        }
        slot.0 = state;
        slot.1 = watch;
    }

    fn push_slow(
        &mut self,
        script: &mut UiScript,
        cats: Option<&Catalogs>,
        player: &Option<ObjectFields>,
    ) {
        let self_guid = net::ui_self_guid();
        let names = net::ui_names();
        let party = net::ui_party();
        let giver = net::ui_giver();
        let selection = net::ui_selection();
        let prev_selection = self.selection;
        self.selection = selection;

        let name_of = |guid: u64| -> Option<String> {
            names.get(&guid).cloned().or_else(|| {
                if guid == self_guid {
                    net::ui_roster_name(guid)
                } else {
                    net::ui_creature_name(guid)
                }
            })
        };
        let lookup = |guid: u64| -> Option<(u8, ObjectFields)> {
            net::ui_unit(guid).map(|u| (u.kind, u.fields))
        };

        let own = player.as_ref();
        let pet = own.and_then(|f| f.unit_charm());
        let mut party_guids = [0u64; 4];
        for (i, m) in party.members.iter().take(4).enumerate() {
            party_guids[i] = m.guid;
        }
        self.guids = UnitGuids {
            player: self_guid,
            pet: pet.unwrap_or(0),
            target: selection,
            mouseover: 0,
            npc: giver.npc,
            party: party_guids,
            party_pets: [0; 4],
            raid: Vec::new(),
            raid_pets: Vec::new(),
            held: net::ui_held_targets(),
        };
        script.set_unit_guids(&self.guids);
        script.seat_player_guid(self_guid);

        let make = |guid: u64| -> Option<UnitState> {
            if guid == 0 {
                return None;
            }
            let (kind, fields) = lookup(guid)?;
            Some(unit_state(
                guid,
                kind,
                &fields,
                name_of(guid),
                cats,
                own.unwrap_or(&fields),
            ))
        };

        // The token aliases the stock frames read.
        let tokens: [(&'static str, u64); 7] = [
            ("player", self_guid),
            ("target", selection),
            (
                "targettarget",
                self.guids.held.get(&selection).copied().unwrap_or(0),
            ),
            ("pet", self.guids.pet),
            ("party1", party_guids[0]),
            ("party2", party_guids[1]),
            ("npc", giver.npc),
        ];
        for (token, guid) in tokens {
            self.push_unit(script, token, make(guid));
        }
        for i in 2..4 {
            let (token, guid) = match i {
                2 => ("party3", party_guids[2]),
                _ => ("party4", party_guids[3]),
            };
            self.push_unit(script, token, make(guid));
        }

        // The client signals `PLAYER_TARGET_CHANGED` from `SetSelection` (`0x4b7b02`); here the
        // server's mirrored descriptor is the only news of a selection, and this pass is where it
        // lands. Fired after the token snapshots above so the frame's handler reads the new unit
        // (`TargetFrame.lua:37-56` shows the frame, and only this event ever does).
        if selection != prev_selection {
            fire(script, "PLAYER_TARGET_CHANGED", Vec::new());
        }

        // Units the tokens' `target` hops reach: their own snapshots let `targettargettarget`
        // and a tooltip's chain read names, health and auras.
        let mut reachable: HashSet<u64> = HashSet::new();
        let mut cursor = selection;
        for _ in 0..4 {
            let Some(&next) = self.guids.held.get(&cursor) else {
                break;
            };
            if next == 0 || !reachable.insert(next) {
                break;
            }
            cursor = next;
        }
        for guid in reachable.iter().copied() {
            if let Some(state) = make(guid) {
                script.set_unit_by_guid(guid, Some(state));
            }
            if !self.chain_guids.contains(&guid) {
                self.chain_guids.push(guid);
            }
        }
        self.chain_guids.retain(|g| {
            if reachable.contains(g) {
                true
            } else {
                script.set_unit_by_guid(*g, None);
                false
            }
        });

        // Player record, money, XP, rest, combo points.
        if let Some(fields) = own {
            let race = fields.unit_race();
            let class = fields.unit_class();
            let record = PlayerRecord {
                name: name_of(self_guid).unwrap_or_default(),
                race: race
                    .and_then(|r| {
                        cats.and_then(|c| c.races.as_ref())
                            .and_then(|t| t.names(u32::from(r)))
                    })
                    .map(|(n, f)| (n.to_string(), f.to_string())),
                class: class
                    .and_then(|c| {
                        cats.and_then(|x| x.classes.as_ref())
                            .and_then(|t| t.name(u32::from(c)).zip(t.file(u32::from(c))))
                    })
                    .map(|(n, f)| (n.to_string(), f.to_string())),
                sex: match fields.unit_gender() {
                    Some(0) => 2,
                    Some(1) => 3,
                    _ => 0,
                },
            };
            if record.name != self.name.clone().unwrap_or_default() {
                script.set_player_record(record.clone());
                self.name = Some(record.name);
            }
        }
        if let Some(p) = net::ui_player() {
            if p.money != self.money {
                script.set_money(u64::from(p.money));
                self.money = p.money;
                if !self.first {
                    fire(script, "PLAYER_MONEY", Vec::new());
                }
            }
            if (p.xp, p.next_xp) != self.xp {
                script.set_player_xp(p.xp, p.next_xp);
                self.xp = (p.xp, p.next_xp);
                if !self.first {
                    fire(script, "PLAYER_XP_UPDATE", vec![ev_str("player")]);
                }
            }
            if p.level != self.level {
                if self.level != 0 && !self.first {
                    fire(
                        script,
                        "PLAYER_LEVEL_UP",
                        vec![ScriptValue::Int(i64::from(p.level))],
                    );
                }
                self.level = p.level;
            }
            let combo_target = net::ui_selection();
            if (p.combo, combo_target) != self.combo {
                script.set_combo_points(p.combo, combo_target);
                self.combo = (p.combo, combo_target);
                if !self.first {
                    fire(script, "PLAYER_COMBO_POINTS", Vec::new());
                }
            }
        }
        if let Some(fields) = own {
            let rest = (
                fields.player_rest_state().unwrap_or(0),
                fields.player_rest_state_experience().unwrap_or(0),
                fields.player_flags() & 0x20 != 0,
            );
            if rest != self.rest || !self.rest_pushed {
                let was_resting = self.rest.2;
                script.set_rest_state(rest.0, rest.1, rest.2);
                self.rest = rest;
                if !self.first {
                    fire(script, "UPDATE_EXHAUSTION", Vec::new());
                }
                // The stock status icons (zzz, crossed swords) start shown in
                // PlayerFrame.xml; `PlayerFrame_UpdateStatus` is what hides
                // them, and it only runs on these events. The load's own push
                // stands in for the client's login-time one.
                if !self.rest_pushed || was_resting != rest.2 {
                    self.rest_pushed = true;
                    fire(script, "PLAYER_UPDATE_RESTING", Vec::new());
                }
            }
        }

        // Action bar. The inventory snapshot is taken once: the bar's counts and the
        // bag containers below read the same rows.
        let inv = net::ui_inventory();
        let actions = net::ui_actions();
        if actions != self.actions {
            let keep: HashSet<u32> = actions
                .iter()
                .map(|(slot, _, _)| u32::from(*slot) + 1)
                .collect();
            for (slot, _, _) in &self.actions {
                let id = u32::from(*slot) + 1;
                if !keep.contains(&id) {
                    script.set_action(id, None);
                    script.set_action_state(id, None);
                    self.action_slots.remove(&id);
                    self.action_states.remove(&id);
                    if !self.first {
                        fire(
                            script,
                            "ACTIONBAR_SLOT_CHANGED",
                            vec![ScriptValue::Int(i64::from(id))],
                        );
                    }
                }
            }
            for &(slot, kind, action) in &actions {
                let id = u32::from(slot) + 1;
                let texture = match kind {
                    0x00 => cats
                        .and_then(|c| c.spells.as_ref())
                        .and_then(|s| s.get(action))
                        .and_then(|d| norm_icon(d.icon.clone())),
                    0x80 => {
                        let info = net::ui_item_info(action);
                        cats.and_then(|c| c.items.as_ref())
                            .and_then(|t| t.get(info.display_id))
                            .and_then(|d| norm_icon(d.icon.clone()))
                    }
                    _ => None,
                };
                let count = if kind == 0x80 {
                    inv.iter()
                        .filter(|i| i.entry == action)
                        .map(|i| i.count)
                        .sum()
                } else {
                    0
                };
                let fresh = ActionSlot {
                    texture,
                    kind,
                    action,
                    count,
                    consumable: false,
                    use_spell: None,
                };
                if self.action_slots.get(&id) != Some(&fresh) {
                    script.set_action(id, Some(fresh.clone()));
                    self.action_slots.insert(id, fresh);
                    if !self.first {
                        fire(
                            script,
                            "ACTIONBAR_SLOT_CHANGED",
                            vec![ScriptValue::Int(i64::from(id))],
                        );
                    }
                }
                let cast = net::ui_self_cast();
                let attack = action == ATTACK_SPELL;
                let current = (attack && net::ui_self_attacking())
                    || cast.is_some_and(|(s, _, _)| s == action);
                let state = ActionState {
                    usable: true,
                    not_enough_mana: false,
                    in_range: None,
                    has_range: false,
                    current,
                    auto_repeat: false,
                    is_attack: attack,
                    equipped: false,
                    cooldown: None,
                };
                if self.action_states.get(&id) != Some(&state) {
                    script.set_action_state(id, Some(state));
                    self.action_states.insert(id, state);
                    if !self.first {
                        fire(script, "ACTIONBAR_UPDATE_STATE", Vec::new());
                    }
                }
            }
            self.actions = actions;
        }

        // Spellbook: one tab per skill line, the player's race and class resolving the tab.
        let spells = net::ui_spells();
        if spells != self.spells {
            let book = build_book(&spells, cats, own);
            script.set_spellbook(book);
            self.spells = spells;
            if !self.first {
                fire(script, "SPELLS_CHANGED", Vec::new());
            }
        }

        // Auras: the player's own, and the target's off its descriptor.
        if let Some(fields) = own {
            let auras = auras_of(fields, cats);
            let key = aura_key(&auras);
            if key != self.auras {
                script.set_player_auras(auras);
                self.auras = key;
                if !self.first {
                    fire(script, "UNIT_AURA", vec![ev_str("player")]);
                }
            }
        }
        if selection != 0 {
            if let Some((kind, fields)) = lookup(selection) {
                if kind == 3 || kind == 4 {
                    let auras = auras_of(&fields, cats);
                    let key = aura_key(&auras);
                    if self.target_auras.0 != selection || self.target_auras.1 != key {
                        script.set_unit_auras(selection, Some(auras));
                        if self.target_auras.0 != 0 && self.target_auras.0 != selection {
                            script.set_unit_auras(self.target_auras.0, None);
                        }
                        self.target_auras = (selection, key);
                        if !self.first {
                            fire(script, "UNIT_AURA", vec![ev_str("target")]);
                        }
                    }
                }
            }
        } else if self.target_auras.0 != 0 {
            script.set_unit_auras(self.target_auras.0, None);
            self.target_auras = (0, Vec::new());
        }

        self.push_loot(script, cats);
        self.push_merchant(script, cats);
        self.push_containers(script, cats, &inv, player);
        self.push_party(script, &party, self_guid, &name_of);
        self.first = false;
    }

    /// Push the player's bag containers: the backpack (0) and each equipped bag (1..=4), the
    /// snapshots `GetContainerNumSlots`/`GetContainerItemInfo`/`GetBagName` read. A bag with no
    /// item in its slot is absent from the map, as the reference leaves it (`GetContainerNumSlots`
    /// then answers 0), and a bag item leaving a slot closes its window and fires no `BAG_UPDATE`
    /// (`0x4f92cc`).
    fn push_containers(
        &mut self,
        script: &mut UiScript,
        cats: Option<&Catalogs>,
        inv: &[net::BnNetInvItem],
        player: &Option<ObjectFields>,
    ) {
        // The stat sheet rides the same pass, so the doll's rows and its numbers repaint together.
        // Deviation: the reference fires one event per changed field id
        // (`ui_unit/field_events.rs:185-204`); this host fires the sheet's whole set on any change
        // of the snapshot, which repaints the same frames and costs one pass per real change.
        if let Some(f) = player.as_ref() {
            let stats = combat_stats(f, inv);
            if self.last_stats.as_ref() != Some(&stats) {
                self.last_stats = Some(stats.clone());
                script.set_player_combat_stats(Some(stats));
                if !self.first {
                    for event in [
                        "UNIT_STATS",
                        "UNIT_RESISTANCES",
                        "UNIT_DAMAGE",
                        "UNIT_ATTACK_POWER",
                        "UNIT_RANGED_ATTACK_POWER",
                        "UNIT_ATTACK_SPEED",
                        "UNIT_RANGEDDAMAGE",
                        "PLAYER_DAMAGE_DONE_MODS",
                    ] {
                        fire(script, event, vec![ev_str("player")]);
                    }
                }
            }
        }
        let bags = net::ui_bags();
        // The class's relic flag: the RANGED seat is a relic seat for Paladin, Shaman and Druid,
        // which decides which inventory types name it (`ui_items/mod.rs:869`).
        let has_relic_slot = player.as_ref().is_some_and(|f| {
            f.unit_class().is_some_and(|c| {
                cats.and_then(|x| x.classes.as_ref())
                    .is_some_and(|t| t.has_relic_slot(u32::from(c)))
            })
        });
        // The server's half of the item lock, ahead of every feed: the reference compares the
        // incoming guid with the one still in the descriptor (`0x5ddd42`), so a handler reads the
        // slot as it was. The cursor verbs lock what they move and only the server lifts it; with
        // no drain here a swapped item stays locked and can never be picked up again.
        for guid in seat_unlocks(&mut self.last_seats, &bags, inv) {
            script.unlock_item(guid);
        }
        // The doll first: its getters read the snapshot, and the container pass below can return
        // early on an unchanged bag. The app pushes the same pair under one `UNIT_INVENTORY_CHANGED`
        // (`ui_char.rs:910-918`).
        let inv_slots = inventory_slots(inv, cats, has_relic_slot);
        if self.last_inv.as_ref() != Some(&inv_slots) {
            self.last_inv = Some(inv_slots.clone());
            script.set_inventory_slots(inv_slots);
            if !self.first {
                fire(
                    script,
                    "UNIT_INVENTORY_CHANGED",
                    vec![ScriptValue::Str("player".to_string())],
                );
            }
        }
        let mut fresh: HashMap<i64, ContainerState> = HashMap::new();
        let mut guids = [0u64; 4];
        for &(id, num_slots, entry, guid) in &bags {
            if (1..=4).contains(&id) {
                guids[id as usize - 1] = guid;
            }
            let mut slots = HashMap::new();
            for item in inv {
                let row_bag = match item.bag {
                    0 => 0,
                    19..=22 => i64::from(item.bag) - 18,
                    _ => continue,
                };
                if row_bag == id {
                    slots.insert(
                        u32::from(item.slot) + 1,
                        container_slot(item, cats, has_relic_slot),
                    );
                }
            }
            let name = if id == 0 {
                // "Backpack" is the enUS pack's own `BACKPACK_TOOLTIP` value, so it is the
                // fallback only when the chain's Lua has no row.
                Some(
                    localized_label(script, "BACKPACK_TOOLTIP")
                        .unwrap_or_else(|| "Backpack".to_string()),
                )
            } else if entry != 0 {
                let n = cstr(&net::ui_item_info(entry).name);
                (!n.is_empty()).then_some(n)
            } else {
                None
            };
            fresh.insert(
                id,
                ContainerState {
                    name,
                    num_slots,
                    slots,
                },
            );
        }
        let emptied: Vec<i64> = (0..4)
            .filter(|&i| self.bag_guids[i] != 0 && guids[i] != self.bag_guids[i])
            .map(|i| i as i64 + 1)
            .collect();
        self.bag_guids = guids;
        for &id in &emptied {
            fire(script, "BAG_CLOSED", vec![ScriptValue::Int(id)]);
        }
        let changed: Vec<i64> = fresh
            .keys()
            .chain(self.containers.keys())
            .copied()
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|id| fresh.get(id) != self.containers.get(id))
            .collect();
        if changed.is_empty() {
            return;
        }
        for &id in &changed {
            script.set_container(id, fresh.get(&id).cloned());
        }
        self.containers = fresh;
        if !self.first {
            for &id in &changed {
                if !emptied.contains(&id) {
                    fire(script, "BAG_UPDATE", vec![ScriptValue::Int(id)]);
                }
            }
        }
    }

    fn push_loot(&mut self, script: &mut UiScript, cats: Option<&Catalogs>) {
        let wire = net::ui_loot();
        if wire.guid == 0 {
            if self.loot.take().is_some() {
                script.set_loot(None);
                fire(script, "LOOT_CLOSED", Vec::new());
            }
            return;
        }
        // A response for another source replaces the window: its layout is fixed at open, so each
        // window keeps its own guid and row order (`0x5ebc35` tears the old one down first).
        let fresh = self.loot.as_ref().is_none_or(|w| w.guid != wire.guid);
        if fresh {
            let replaced = self.loot.replace(LootWindow {
                guid: wire.guid,
                coin: wire.gold > 0,
                slots: wire
                    .items
                    .iter()
                    .take(wire.count as usize)
                    .map(|i| i.slot)
                    .collect(),
                picks: Vec::new(),
                last: None,
                opened: false,
                released: false,
            });
            if replaced.is_some() {
                script.set_loot(None);
                fire(script, "LOOT_CLOSED", Vec::new());
            }
        }
        let window = self.loot.as_mut().expect("the window was just ensured");
        let items: Vec<&net::BnNetLootItem> = wire.items.iter().take(wire.count as usize).collect();
        let mut rows: Vec<Option<LootRow>> = Vec::with_capacity(window.slots.len() + 1);
        let picks = loot_picks(window, &wire);
        window.picks = picks;
        if window.coin {
            // The coin row's own guard (`0x4c22e0`): taken, it is a gap like any other row.
            let live = wire.gold > 0;
            rows.push(live.then(|| LootRow {
                name: Some(money_text(script, wire.gold)),
                texture: Some(benilla_ui::script::coin_icon(i64::from(wire.gold)).to_string()),
                // `0x4c22e0` returns before the record read (`0x4c22fd`).
                quantity: 0,
                quality: Some(0),
                is_coin: true,
                item_id: 0,
                link: None,
                random_property_id: 0,
            }));
        }
        for &slot in &window.slots {
            let Some(item) = items.iter().find(|i| i.slot == slot) else {
                // Taken: the row keeps its place and answers the cleared sentinel (`0x4c2435`),
                // which the stock `LootFrame_Update` hides in place (`LootFrame.lua:80`).
                rows.push(None);
                continue;
            };
            let info = net::ui_item_info(item.item_id);
            let name: String = cstr(&info.name);
            let texture = cats
                .and_then(|c| c.items.as_ref())
                .and_then(|t| t.get(item.display_info_id))
                .and_then(|d| norm_icon(d.icon.clone()));
            rows.push(Some(LootRow {
                name: (!name.is_empty()).then_some(name),
                texture,
                quantity: item.count.max(1),
                quality: Some(info.quality),
                is_coin: false,
                item_id: item.item_id,
                link: None,
                random_property_id: 0,
            }));
        }
        // The copier fires nothing while a query is pending (`0x4c1e9f`); the cache callback
        // `0x4c2ac0` fires `LOOT_OPENED` at the last answer. Deviation: an entry the server
        // answered with no record releases its row, where the reference's callback returns and
        // the window never opens.
        let pending = rows.iter().flatten().any(|r| {
            !r.is_coin && r.item_id != 0 && r.name.is_none() && !net::ui_item_unknown(r.item_id)
        });
        let before = window.last.clone();
        if window.last.as_ref() != Some(&rows) {
            script.set_loot(Some(LootState {
                rows: rows.clone(),
                fishing: false,
                master_candidates: Vec::new(),
                source_unit: net::ui_unit(wire.guid).is_some_and(|u| u.kind == 7),
            }));
            window.last = Some(rows.clone());
        }
        // An emptied window releases itself (`0x4c2a70` → `0x48f200`); vmangos does not, so the
        // server's answer to this release closes the window. Empty at open stays open.
        let emptied = !rows.is_empty()
            && rows.iter().all(Option::is_none)
            && before
                .as_ref()
                .is_some_and(|b| b.iter().any(Option::is_some));
        if emptied && !window.released {
            window.released = true;
            let _ = net::send_cmd(Cmd::LootRelease(window.guid));
        }
        if !window.opened {
            if !pending {
                window.opened = true;
                fire(script, "LOOT_OPENED", Vec::new());
            }
        } else if let Some(was) = &before {
            // One `LOOT_SLOT_CLEARED` per row that went, with its 1-based row: the stock handler
            // hides that button and pages down when the page empties (`LootFrame.lua:22-50`).
            for (i, old) in was.iter().enumerate() {
                if old.is_some() && rows.get(i).is_none_or(Option::is_none) {
                    fire(
                        script,
                        "LOOT_SLOT_CLEARED",
                        vec![ScriptValue::Int(i as i64 + 1)],
                    );
                }
            }
        }
    }

    /// Feed the open vendor into the VM so its stock `MerchantFrame` shows the list — the same
    /// shape [`Feed::push_loot`] runs: the middle layer owns the stock, the frame reads it, and a
    /// row click queues `BuyMerchantItem`, which the drain turns into `CMSG_BUY_ITEM`.
    ///
    /// Rows stay positional: the drain resolves Lua's 1-based row against the wire list, so a row
    /// whose template has not answered still holds its place rather than shifting its neighbours.
    fn push_merchant(&mut self, script: &mut UiScript, cats: Option<&Catalogs>) {
        let vendor = net::ui_vendor();
        // The stock frame reads `GetRepairAllCost` on its own update, so the price rides every
        // pass: a fight wears the gear without the vendor's list changing.
        let can_repair = vendor.guid != 0
            && net::ui_unit(vendor.guid)
                .is_some_and(|u| u.fields.unit_npc_flags() & NPC_FLAG_REPAIR != 0);
        if can_repair {
            script.set_repair_all_cost(repair_all_cost(cats));
        }
        let n = (vendor.count as usize).min(vendor.rows.len());
        let key: Vec<(u32, u32, u32, u32, u32)> = vendor
            .rows
            .iter()
            .take(n)
            // The template's display id is 0 until `SMSG_ITEM_QUERY_SINGLE` answers, so its
            // landing moves the key and the frame re-reads the row it could not name.
            .map(|r| {
                (
                    r.entry,
                    r.price,
                    r.count,
                    r.buy_count,
                    net::ui_item_info(r.entry).display_id,
                )
            })
            .collect();
        let key = (vendor.guid != 0).then(|| (vendor.guid, key));
        if key == self.merchant_key {
            return;
        }
        let was_open = self.merchant_key.is_some();
        self.merchant_key = key.clone();
        let Some((_guid, _)) = key else {
            if was_open {
                script.set_merchant(None);
                fire(script, "MERCHANT_CLOSED", Vec::new());
            }
            return;
        };
        let items: Vec<MerchantItem> = vendor
            .rows
            .iter()
            .take(n)
            .map(|r| {
                let info = net::ui_item_info(r.entry);
                let (name, quality, icon) = item_look(&info, cats);
                // A vendor row whose item template carries no display id still has the one
                // `SMSG_LIST_INVENTORY` sent for it.
                let icon = icon.or_else(|| {
                    cats.and_then(|c| c.items.as_ref())
                        .and_then(|t| t.get(r.display_id))
                        .and_then(|d| norm_icon(d.icon.clone()))
                });
                MerchantItem {
                    name: name.clone(),
                    texture: icon,
                    price: r.price,
                    quantity: r.buy_count.max(1),
                    num_available: if r.count == u32::MAX {
                        -1
                    } else {
                        r.count as i32
                    },
                    item_id: r.entry,
                    stats: None,
                    link: name.as_ref().map(|n| item_link(r.entry, n, quality)),
                    max_stack: (info.stackable > 0).then_some(info.stackable),
                }
            })
            .collect();
        script.set_merchant(Some(MerchantState {
            items,
            buyback: Vec::new(),
            can_repair,
        }));
        fire(
            script,
            if was_open {
                "MERCHANT_UPDATE"
            } else {
                "MERCHANT_SHOW"
            },
            Vec::new(),
        );
    }

    fn push_party(
        &mut self,
        script: &mut UiScript,
        party: &net::BnNetParty,
        self_guid: u64,
        name_of: &dyn Fn(u64) -> Option<String>,
    ) {
        let members: Vec<PartyMemberInfo> = party
            .members
            .iter()
            .take(party.count as usize)
            .map(|m| PartyMemberInfo {
                name: {
                    let n = cstr(&m.name);
                    if n.is_empty() {
                        name_of(m.guid).unwrap_or_default()
                    } else {
                        n
                    }
                },
                guid: m.guid,
            })
            .collect();
        let key: Vec<(u64, String)> = members.iter().map(|m| (m.guid, m.name.clone())).collect();
        if self.party_key.as_ref() == Some(&key) {
            return;
        }
        let was_grouped = self.party_key.as_ref().is_some_and(|k| !k.is_empty());
        let leader = party
            .members
            .iter()
            .take(party.count as usize)
            .find(|m| m.leader != 0);
        let leader_index = leader
            .and_then(|l| members.iter().position(|m| m.guid == l.guid))
            .map_or(0, |i| i as u32 + 1);
        let state = PartyState {
            members,
            leader_index,
            leader_guid: leader.map_or(0, |l| l.guid),
            own_guid: self_guid,
            raid: Vec::new(),
            loot_method: String::new(),
            master_looter: None,
            loot_threshold: 2,
        };
        script.set_party(state);
        self.party_key = Some(key);
        if !self.first {
            fire(script, "PARTY_MEMBERS_CHANGED", Vec::new());
            if leader_index != 0 || was_grouped {
                fire(script, "PARTY_LEADER_CHANGED", Vec::new());
            }
            if !was_grouped && !party.members.is_empty() {
                fire(script, "PARTY_MEMBERS_CHANGED", Vec::new());
            }
        }
    }

    /// Push the death snapshot and the per-frame scalar edges.
    fn push_frame(
        &mut self,
        script: &mut UiScript,
        cats: Option<&Catalogs>,
        player: &Option<ObjectFields>,
        dt: f32,
    ) {
        let dead = net::ui_player().is_some_and(|p| p.dead != 0);
        let ghost = player.as_ref().is_some_and(|f| f.player_is_ghost());
        if dead && self.died_at.is_none() {
            self.died_at = Some(self.now);
            fire(script, "PLAYER_DEAD", Vec::new());
        } else if !dead {
            if self.dead {
                fire(script, "PLAYER_ALIVE", Vec::new());
            }
            self.died_at = None;
        }
        if ghost != self.ghost {
            if ghost {
                fire(script, "PLAYER_GHOST", Vec::new());
            } else if self.ghost {
                fire(script, "PLAYER_UNGHOST", Vec::new());
            }
            self.ghost = ghost;
        }
        self.dead = dead;

        let corpse_ms = net::ui_corpse_delay_ms();
        if corpse_ms != self.corpse_ms {
            self.corpse_ms = corpse_ms;
            self.corpse_at = (corpse_ms > 0).then(|| self.now + f64::from(corpse_ms) / 1000.0);
        }
        let resurrect = net::ui_resurrect()
            .map(|r| (r.caster, r.sickness, r.has_timer))
            .unwrap_or((0, false, false));
        let resurrect_edge = resurrect.0 != 0 && resurrect != self.resurrect;
        self.resurrect = resurrect;

        let release_remaining = if dead
            && player
                .as_ref()
                .is_some_and(|f| f.player_release_timer_running())
        {
            self.died_at
                .map(|t| ((t + RELEASE_WINDOW - self.now).max(0.0)) as f32)
        } else {
            None
        };
        let recovery_delay = self
            .corpse_at
            .map_or(0.0, |at| (at - self.now).max(0.0) as f32);
        let self_res_label = player
            .as_ref()
            .and_then(|f| f.player_self_res_spell())
            .filter(|&s| s != 0)
            .and_then(|s| cats.and_then(|c| c.spells.as_ref()).and_then(|t| t.get(s)))
            .map(|d| d.name.clone());
        let spirit_healer = net::ui_spirit_healer();
        script.set_death(DeathUiState {
            release_remaining,
            recovery_delay,
            resurrect_sickness: resurrect.1,
            resurrect_has_timer: resurrect.2,
            spirit_healer_in_range: spirit_healer != 0,
            sickness_duration: None,
            self_res_label,
        });
        if resurrect_edge {
            fire(script, "RESURRECT_REQUEST", vec![ev_str("player")]);
        }

        let cast = net::ui_self_cast();
        let cast_id = cast.map(|(s, _, _)| s);
        if cast_id != self.cast {
            if let Some((id, _, duration)) = cast {
                let name = cats
                    .and_then(|c| c.spells.as_ref())
                    .and_then(|t| t.get(id))
                    .filter(|d| !d.no_casting_bar_text())
                    .map(|d| d.name.clone())
                    .unwrap_or_default();
                fire(
                    script,
                    "SPELLCAST_START",
                    vec![
                        ScriptValue::Str(name),
                        ScriptValue::Int((duration * 1000.0) as i64),
                    ],
                );
            } else if self.cast.is_some() {
                fire(script, "SPELLCAST_STOP", Vec::new());
            }
            self.cast = cast_id;
        }
        script.set_casting(cast.is_some());

        let combat = net::ui_self_fields().is_some_and(|f| f.unit_flags() & 0x0008_0000 != 0);
        if combat != self.combat {
            fire(
                script,
                if combat {
                    "PLAYER_REGEN_DISABLED"
                } else {
                    "PLAYER_REGEN_ENABLED"
                },
                Vec::new(),
            );
            self.combat = combat;
        }

        // Auto-attack's own edges (`SMSG_ATTACKSTART`/`ATTACKSTOP`): the stock
        // `PlayerFrame_UpdateStatus` shows the red ring and the swords on
        // `PLAYER_ENTER_COMBAT`, which the combat flag above never fires.
        let engaged = net::ui_self_attacking();
        if engaged != self.engaged {
            self.engaged = engaged;
            fire(
                script,
                if engaged {
                    "PLAYER_ENTER_COMBAT"
                } else {
                    "PLAYER_LEAVE_COMBAT"
                },
                Vec::new(),
            );
        }

        // The melee refusal line, the reference's per-player tick `0x5ec950`:
        // shown only while an attack target stands (`[+0xc48]`), latched, and
        // repeated every four seconds by the latch itself. `DisplayError(0xd8)`/
        // `(0xd7)` land on the stock error frame through `UI_ERROR_MESSAGE`.
        if net::ui_attack_target() != 0 {
            if let Some(code) = net::take_due_swing_refusal() {
                let key = if code == 1 {
                    "ERR_BADATTACKPOS"
                } else {
                    "ERR_BADATTACKFACING"
                };
                fire_error(script, key);
            }
        }

        let _ = dt;
    }

    fn apply_binding_input(&mut self, world: *mut BnWorld, inputs: Vec<BindingInput>) {
        for input in inputs {
            match input {
                BindingInput::Start(HeldInput::MoveForward) => {
                    self.held.forward = true;
                }
                BindingInput::Stop(HeldInput::MoveForward) => self.held.forward = false,
                BindingInput::Start(HeldInput::MoveBackward) => {
                    self.held.backward = true;
                }
                BindingInput::Stop(HeldInput::MoveBackward) => self.held.backward = false,
                BindingInput::Start(HeldInput::StrafeLeft) => {
                    self.held.strafe_left = true;
                }
                BindingInput::Stop(HeldInput::StrafeLeft) => self.held.strafe_left = false,
                BindingInput::Start(HeldInput::StrafeRight) => {
                    self.held.strafe_right = true;
                }
                BindingInput::Stop(HeldInput::StrafeRight) => self.held.strafe_right = false,
                BindingInput::Start(HeldInput::TurnLeft) => {
                    self.held.turn_left = true;
                }
                BindingInput::Stop(HeldInput::TurnLeft) => self.held.turn_left = false,
                BindingInput::Start(HeldInput::TurnRight) => {
                    self.held.turn_right = true;
                }
                BindingInput::Stop(HeldInput::TurnRight) => self.held.turn_right = false,
                BindingInput::Start(HeldInput::TurnOrAction) => {
                    player::ui_turn_held(true);
                }
                BindingInput::Stop(HeldInput::TurnOrAction) => {
                    player::ui_turn_held(false);
                }
                // `CameraOrSelectOrMove` is the left button's bit (`0x514190`): KVM's move-to
                // point, which the click gestures already cover. Nothing holds it here.
                BindingInput::Start(HeldInput::CameraOrSelectOrMove)
                | BindingInput::Stop(HeldInput::CameraOrSelectOrMove) => {}
                BindingInput::Fire(FiredInput::Jump) => player::ui_jump(world),
                BindingInput::Fire(FiredInput::ToggleRun) => player::ui_toggle_walk(world),
                BindingInput::Fire(FiredInput::CameraZoomIn(n)) => player::ui_zoom(world, n),
                BindingInput::Fire(FiredInput::CameraZoomOut(n)) => player::ui_zoom(world, -n),
                BindingInput::Fire(FiredInput::ToggleAutoRun)
                | BindingInput::Fire(FiredInput::SitOrStand)
                | BindingInput::Fire(FiredInput::ToggleSheath) => {}
            }
        }
        // The bindings own the mover outright, as the reference's `BindingsState`
        // does: this frame's axes are the held bits, and no raw key state mixes in.
        // The look channel is the right button's (`IsMouselooking`), read after the
        // edges above so a same-frame press and A/D already strafe.
        let (f, s, t) = self.held.axes(player::look_turn());
        player::ui_move(world, f, s, t, true);
        // The heartbeat's turn axis reads right-positive, the body's left-positive.
        net::ui_set_move(f, s, -t, true);
    }
}

/// Group the known spells into skill-line tabs and one flat slot list.
fn build_book(
    spells: &[u32],
    cats: Option<&Catalogs>,
    own: Option<&ObjectFields>,
) -> SpellBookState {
    let Some(catalog) = cats.and_then(|c| c.spells.as_ref()) else {
        return SpellBookState::default();
    };
    let race = own.and_then(|f| f.unit_race()).unwrap_or(0);
    let class = own.and_then(|f| f.unit_class()).unwrap_or(0);
    let mut by_line: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &id in spells {
        if !catalog.get(id).is_some_and(|d| d.in_spellbook()) {
            continue;
        }
        let tab = cats
            .and_then(|c| c.skills.as_ref())
            .map_or(0, |t| t.spell_tab(id, race, class));
        by_line.entry(tab).or_default().push(id);
    }
    let mut lines: Vec<(u32, String, Option<String>, Vec<u32>)> = by_line
        .into_iter()
        .map(|(line, ids)| {
            let (name, texture) = if line == 0 {
                (GENERAL_TAB.to_string(), Some(GENERAL_TAB_ICON.to_string()))
            } else {
                match cats
                    .and_then(|c| c.skills.as_ref())
                    .and_then(|t| t.line(line))
                {
                    Some(info) => (info.name.clone(), info.icon.clone()),
                    None => (GENERAL_TAB.to_string(), None),
                }
            };
            (line, name, texture, ids)
        })
        .collect();
    lines.sort_by(|a, b| (a.0 != 0).cmp(&(b.0 != 0)).then(a.1.cmp(&b.1)));
    let mut tabs = Vec::with_capacity(lines.len());
    let mut slots = Vec::new();
    for (_, name, texture, mut ids) in lines {
        ids.sort_by_key(|&id| {
            let d = catalog.get(id);
            (
                d.map(|d| d.name.clone()).unwrap_or_default(),
                d.and_then(|d| d.rank.clone()).unwrap_or_default(),
            )
        });
        let offset = slots.len() as u32;
        let num_spells = ids.len() as u32;
        for id in ids {
            let d = catalog.get(id);
            slots.push(SpellSlotView {
                spell_id: id,
                name: d.map(|d| d.name.clone()).unwrap_or_default(),
                rank: d.and_then(|d| d.rank.clone()),
                texture: d.and_then(|d| norm_icon(d.icon.clone())),
                passive: d.is_some_and(|d| d.passive),
                current: false,
                cooldown: None,
                autocast: None,
                packed: 0,
                bar_texture: None,
            });
        }
        tabs.push(SpellTabView {
            name,
            texture,
            offset,
            num_spells,
        });
    }
    SpellBookState { tabs, slots }
}

/// Push the live session into the interface. `dt` is seconds since the last tick.
pub(crate) fn sync(script: &mut UiScript, dt: f32) {
    if script.vm_kind() != VmKind::InGame {
        return;
    }
    if net::ui_self_guid() == 0 {
        return;
    }
    let Some(own) = net::ui_self_fields() else {
        return;
    };
    // The wire chat lines the network thread queued: fired here, where the VM lives, so the stock
    // `ChatFrame_OnEvent` composes and prints them.
    for (event, args) in net::take_chat_events() {
        script.fire_event(&event, args);
    }
    // The gossip menu likewise shows itself here: the latch holds it until its greeting lands, so
    // the stock `GossipFrame` opens with its greeting, options and quest rows in one go.
    crate::gossip_vm::feed(script);
    CATS.with(|c| {
        let cats = c.borrow();
        // The quest log likewise lists what this pushes: our slots whose template has landed.
        crate::quest_log_vm::feed(script, cats.as_ref());
        // The questgiver marker sweep rides the UI tick, where the catalog and our own
        // descriptor meet (`crates/benilla-app/src/quest_markers`).
        crate::quest_markers::sweep(&own, cats.as_ref().and_then(|c| c.factions.as_ref()));
        FEED.with(|f| {
            let mut feed = f.borrow_mut();
            feed.now += f64::from(dt);
            feed.push_frame(script, cats.as_ref(), &Some(own.clone()), dt);
            feed.until_slow -= dt;
            if feed.until_slow <= 0.0 || feed.first {
                feed.until_slow = 0.1;
                feed.push_slow(script, cats.as_ref(), &Some(own));
            }
        });
    });
    // The plates seat on this frame's world and the stock `NAMEPLATES_ON` verbs, so they ride the
    // same tick the feed does (`crates/benilla-app/src/vplates.rs`, across the boundary).
    crate::nameplates::drive(WORLD.with(|w| w.get()), script);
}

/// The OS window deactivate, the reference's `0x514490` (`and eax,0xfffff00f`, sole caller
/// `0x493058` off the root's WM_ACTIVATE slot): the direction bits are cleared while the mouse
/// pair and autorun survive. The host reports it when its viewport loses focus, so a movement
/// key held across an alt-tab does not keep the body walking or turning.
pub(crate) fn focus_lost() {
    let world = WORLD.with(|w| w.get());
    FEED.with(|f| {
        let mut feed = f.borrow_mut();
        if !feed.held.any() {
            return;
        }
        // The mask keeps the mouse pair and autorun: only the direction bits die.
        feed.held.forward = false;
        feed.held.backward = false;
        feed.held.strafe_left = false;
        feed.held.strafe_right = false;
        feed.held.turn_left = false;
        feed.held.turn_right = false;
        // Both channels a held axis feeds: the body's local input and the
        // movement the net layer reports to the server.
        player::ui_move(world, 0.0, 0.0, 0.0, true);
        net::ui_set_move(0.0, 0.0, 0.0, true);
    });
}

/// Drain the interface's verbs into net commands and the middle layer's movement.
pub(crate) fn intents(script: &mut UiScript) {
    if script.vm_kind() != VmKind::InGame {
        return;
    }
    // The look channel the VM's `MouselookStart`/`Stop` calls just moved, read back
    // into `IsMouselooking` before the frame's world input engages.
    player::drive_mouselook(script);
    let world = WORLD.with(|w| w.get());

    let inputs = script.take_binding_input();
    FEED.with(|f| f.borrow_mut().apply_binding_input(world, inputs));

    let selection = net::ui_selection();

    let actions = net::ui_actions();
    for use_ in script.take_action_uses() {
        let Some(&(_, kind, action)) = actions
            .iter()
            .find(|(slot, _, _)| u32::from(*slot) + 1 == use_.action)
        else {
            continue;
        };
        match kind {
            0x00 if action == ATTACK_SPELL => {
                // `UseAction`'s Attack is TryCast's, so its gate came first and
                // passes to the validator's pick (`0x6e4c90` → `0x6131a0`).
                if use_.may_cast {
                    attack_press(script, true);
                }
            }
            0x00 => {
                let _ = net::send_cmd(Cmd::Cast {
                    spell: action,
                    target: if use_.on_self { 0 } else { selection },
                });
            }
            0x80 => {
                if let Some(item) = net::ui_inventory().iter().find(|i| i.entry == action) {
                    let (bag, slot) = wire_pos(item.bag, item.slot);
                    let _ = net::send_cmd(Cmd::UseItem {
                        bag,
                        slot,
                        spell: 0,
                    });
                }
            }
            _ => {}
        }
    }

    for (action, packed) in script.take_action_sets() {
        if action == 0 {
            continue;
        }
        let _ = net::send_cmd(Cmd::SetAction {
            slot: (action - 1) as u8,
            packed,
        });
    }

    for (spell, on_self) in script.take_spell_casts() {
        let _ = net::send_cmd(Cmd::Cast {
            spell,
            target: if on_self { 0 } else { selection },
        });
    }
    if script.take_spell_stop() {
        if let Some((spell, _, _)) = net::ui_self_cast() {
            let _ = net::send_cmd(Cmd::CancelCast(spell));
        }
    }
    for spell in script.take_cancel_aura_requests() {
        let _ = net::send_cmd(Cmd::CancelAura(spell));
    }

    // The vendor window. `BuyMerchantItem(index)` names the 1-based row of the stock this feed
    // pushed from [`net::ui_vendor`], so the wire wants the entry that row carries and the guid of
    // the vendor still open.
    let vendor = net::ui_vendor();
    for (index, quantity) in script.take_merchant_buys() {
        if vendor.guid == 0 {
            continue;
        }
        let Some(row) = vendor
            .rows
            .iter()
            .take(vendor.count.min(128) as usize)
            // Lua's row is its place in `SMSG_LIST_INVENTORY`'s order, so the row is taken by
            // position; the wire buy then names that row's entry, never a slot.
            .skip((index as usize).saturating_sub(1))
            .next()
            .filter(|r| index > 0 && r.entry != 0)
        else {
            net::note(format!("vendor: no row {index} to buy"));
            continue;
        };
        // The stack count is a u8 on the wire.
        let count = quantity.clamp(1, u32::from(u8::MAX)) as u8;
        let _ = net::send_cmd(Cmd::VendorBuy {
            vendor: vendor.guid,
            entry: row.entry,
            count,
        });
    }

    // `PickupMerchantItem`'s sell arm (`0x4fb760`): our item dropped on the vendor window sells the
    // whole stack, by item guid, so a drop on a seat that has since emptied sells nothing.
    for (bag, slot) in script.take_merchant_cursor_sells() {
        if vendor.guid == 0 {
            continue;
        }
        let Some(row) = lua_wire_pos(bag, slot).and_then(inv_row_at) else {
            net::note(format!(
                "vendor: cursor sell on an empty slot ({bag}, {slot})"
            ));
            continue;
        };
        net::note(format!(
            "vendor: sell lua ({bag}, {slot}) item {:x}",
            row.guid
        ));
        let _ = net::send_cmd(Cmd::VendorSell {
            vendor: vendor.guid,
            item_guid: row.guid,
            count: 0,
        });
    }
    // A held vendor row dropped into a seat is `CMSG_BUY_ITEM_IN_SLOT` (`0x4c78fd`), count 1 as on
    // all three of the reference's drop sites. The wire names the container: the player for a seat
    // of their own array, the bag item for a real bag.
    for (bag, slot, entry) in script.take_merchant_slot_buys() {
        if vendor.guid == 0 {
            continue;
        }
        let Some((bag_index, bag_slot)) = lua_wire_pos(bag, slot) else {
            net::note(format!(
                "vendor: slot buy into an unaddressable seat ({bag}, {slot})"
            ));
            continue;
        };
        let bag_guid = if bag_index == BAG_PLAYER_INVENTORY {
            net::ui_self_guid()
        } else {
            net::ui_bags()
                .into_iter()
                .find(|&(id, _, _, _)| id == i64::from(bag_index) - 18)
                .map(|(_, _, _, guid)| guid)
                .unwrap_or(0)
        };
        if bag_guid == 0 {
            net::note(format!(
                "vendor: slot buy entry {entry} has no container for bag {bag}"
            ));
            continue;
        }
        net::note(format!(
            "vendor: buy entry {entry} into lua ({bag}, {slot})"
        ));
        let _ = net::send_cmd(Cmd::VendorBuyInSlot {
            vendor: vendor.guid,
            entry,
            bag_guid,
            bag_slot,
            count: 1,
        });
    }

    // `CloseMerchant()` (`0x4fadf0`) sends no packet — but the open vendor is what makes a bag
    // right-click a *sale*, so the close has to clear the middle layer's stock window as well as
    // the VM's, or the window would be re-pushed from it on the next pass.
    if script.take_merchant_close() {
        net::ui_vendor_closed();
        script.set_merchant(None);
        fire(script, "MERCHANT_CLOSED", vec![]);
    }

    // `RepairAllItems` (`0x4fbff0`) only arms behind the vendor's repair bit, so the drain needs no
    // second test; a zero item guid is `CMSG_REPAIR_ITEM`'s repair-everything form, whose price and
    // purse the server owns.
    if script.take_repair_all() {
        let vendor = net::ui_vendor().guid;
        if vendor != 0 {
            let _ = net::send_cmd(Cmd::VendorRepair {
                vendor,
                item_guid: 0,
            });
        }
    }

    // Selections: tokens, by name, nearest, and the clear.
    let guids = FEED.with(|f| f.borrow().guids.clone());
    for request in script.take_selection_requests() {
        match request {
            SelectionRequest::Unit(token) => {
                if let Some(guid) = resolve_token(&token, &guids) {
                    net::select(guid);
                }
            }
            SelectionRequest::Assist(token) => {
                if let Some(guid) = resolve_token(&token, &guids) {
                    let target = guids.held.get(&guid).copied().unwrap_or(0);
                    net::select(target);
                }
            }
            SelectionRequest::AssistByName(name) => {
                if let Some(member) = net::ui_party()
                    .members
                    .iter()
                    .take(4)
                    .find(|m| cstr(&m.name).eq_ignore_ascii_case(&name))
                {
                    let target = guids.held.get(&member.guid).copied().unwrap_or(0);
                    net::select(target);
                }
            }
            SelectionRequest::LastEnemy => {}
        }
    }
    for (name, _reverse) in script.take_target_by_name_requests() {
        let names = net::ui_names();
        let player_pos = net::ui_player().map(|p| p.pos);
        let mut best: Option<(f32, u64)> = None;
        for (guid, n) in names.iter() {
            if !n.eq_ignore_ascii_case(&name) {
                continue;
            }
            let Some(unit) = net::ui_unit(*guid) else {
                continue;
            };
            let d = match player_pos {
                Some(p) => {
                    let dx = unit.pos[0] - p[0];
                    let dy = unit.pos[1] - p[1];
                    let dz = unit.pos[2] - p[2];
                    dx * dx + dy * dy + dz * dz
                }
                None => 0.0,
            };
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, *guid));
            }
        }
        if let Some((_, guid)) = best {
            net::select(guid);
        }
    }
    for (mode, reverse) in script.take_target_nearest_requests() {
        if let Some(guid) = nearest_candidate(mode, reverse, selection) {
            net::select(guid);
        }
    }
    if script.take_target_clear() {
        net::select(0);
    }

    // `AttackTarget()` (`0x489b50`), the attack key and the target frame's attack
    // button: the verb queues a call no other drain here takes, so the stock key
    // did nothing. Applied after this frame's selections, so a `TargetUnit(…)`
    // earlier in the same script is already the selection.
    let mut keep = Vec::new();
    for queued in script.take_queued_calls() {
        match queued.call {
            ScriptCall::AttackTarget { may_pick, .. } => attack_press(script, may_pick),
            // A bag-slot click. The VM has already run `0x4fa0e0`'s forks and locked the slot, so
            // the arm it took picks the packet; the item is looked up by guid because the wire's
            // (bag, slot) is not the Lua argument.
            ScriptCall::UseContainerItem(use_) => container_use(&use_),
            _ => keep.push(queued),
        }
    }
    script.requeue_calls(keep);

    container_moves(script);

    giver_intents(script);

    // Death dialog.
    for action in script.take_death_actions() {
        match action {
            DeathAction::Repop => {
                let _ = net::send_cmd(Cmd::Repop);
            }
            DeathAction::RetrieveCorpse => {
                let _ = net::send_cmd(Cmd::ReclaimCorpse(net::ui_self_guid()));
            }
            DeathAction::AcceptResurrect => {
                let _ = net::send_cmd(Cmd::ResurrectResponse {
                    caster: net::ui_resurrect().map_or(0, |r| r.caster),
                    accept: true,
                });
            }
            DeathAction::DeclineResurrect => {
                let _ = net::send_cmd(Cmd::ResurrectResponse {
                    caster: net::ui_resurrect().map_or(0, |r| r.caster),
                    accept: false,
                });
            }
            DeathAction::AcceptXpLoss => {
                let _ = net::send_cmd(Cmd::SpiritHealer(net::ui_spirit_healer()));
            }
            DeathAction::UseSoulstone { .. } => {
                let _ = net::send_cmd(Cmd::SelfRes);
            }
        }
    }

    // Loot: picks map to the coin pile or a wire slot, through the layout the push built; a
    // taken row's gap carries no pick, so a click on it sends nothing.
    let picks = FEED.with(|f| {
        f.borrow()
            .loot
            .as_ref()
            .map(|w| w.picks.clone())
            .unwrap_or_default()
    });
    for slot in script.take_loot_picks() {
        match slot
            .checked_sub(1)
            .and_then(|i| picks.get(i as usize).copied().flatten())
        {
            Some(LootPick::Coin) => {
                let _ = net::send_cmd(Cmd::LootMoney);
            }
            Some(LootPick::Item(wire)) => {
                let _ = net::send_cmd(Cmd::LootItem(wire));
            }
            None => {}
        }
    }
    for _ in script.take_loot_confirms() {
        // The bind confirmation's take is a plain take of the row the dialog named; the VM
        // queues it through the same pick path, so there is nothing extra to send here.
    }
    if script.take_loot_close().is_some() {
        let guid = net::ui_loot().guid;
        let _ = net::send_cmd(Cmd::LootRelease(guid));
    }

    // Party.
    for request in script.take_party_requests() {
        match request {
            PartyRequest::Accept => {
                let _ = net::send_cmd(Cmd::GroupAccept);
            }
            PartyRequest::Decline => {
                let _ = net::send_cmd(Cmd::GroupDecline);
            }
            PartyRequest::Leave => {
                let _ = net::send_cmd(Cmd::GroupDisband);
            }
            PartyRequest::InviteName(name) => {
                let _ = net::send_cmd(Cmd::GroupInvite(name));
            }
            PartyRequest::InviteUnit(token) => {
                if let Some(guid) = resolve_token(&token, &guids) {
                    let name = net::ui_names()
                        .get(&guid)
                        .cloned()
                        .or_else(|| net::ui_roster_name(guid));
                    if let Some(name) = name {
                        let _ = net::send_cmd(Cmd::GroupInvite(name));
                    }
                }
            }
            _ => {}
        }
    }

    // Chat, session, display toggles.
    for ChatSend {
        text,
        chat_type,
        target,
        ..
    } in script.take_chat_sends()
    {
        let kind = match chat_type.as_str() {
            "SAY" => 0x0,
            "PARTY" => 0x1,
            "RAID" => 0x2,
            "GUILD" => 0x3,
            "OFFICER" => 0x4,
            "YELL" => 0x5,
            "WHISPER" => 0x6,
            "EMOTE" => 0x8,
            "CHANNEL" => 0xE,
            "AFK" => 0x14,
            "DND" => 0x15,
            "RAID_WARNING" => 0x58,
            "BATTLEGROUND" => 0x5C,
            _ => continue,
        };
        let _ = net::send_cmd(Cmd::ChatKind {
            kind,
            target,
            msg: text,
        });
    }
    for request in script.take_session_requests() {
        match request {
            SessionRequest::Logout | SessionRequest::Quit | SessionRequest::ForceQuit => {
                let _ = net::send_cmd(Cmd::Logout);
            }
            _ => {}
        }
    }
    for _ in 0..script.take_pvp_toggles() {
        let _ = net::send_cmd(Cmd::TogglePvp);
    }
    for worn in script.take_worn_display_toggles() {
        match worn {
            WornDisplay::Helm => {
                let _ = net::send_cmd(Cmd::ToggleHelm);
            }
            WornDisplay::Cloak => {
                let _ = net::send_cmd(Cmd::ToggleCloak);
            }
        }
    }
    for toggles in script.take_action_bar_toggle_sends() {
        let _ = net::send_cmd(Cmd::ActionBarToggles(toggles));
    }

    // Engine refusals surface as the stock error frame's event.
    for key in script.take_ui_errors() {
        fire_error(script, key);
    }
}

/// One quest row's packet: an active row (`SelectActiveQuest`, `0x501320`) is
/// always a turn-in, and so is an available one flagged one-click (`0x5dbc13`
/// stores the icon-0 flag, `0x5012db` reads it); anything else queries the panel.
fn quest_row_cmd(npc: u64, quest: u32, icon: u32) -> Cmd {
    if matches!(icon, 3 | 4) || icon == 0 {
        Cmd::QuestComplete { npc, quest }
    } else {
        Cmd::GossipQuest { npc, quest }
    }
}

/// The questgiver chain's Lua intents. The stock `GossipFrame` and `QuestFrame`
/// queue one wire intent per click; the app's own systems drain them, and nothing
/// on this path did, so a row click and the Accept/Continue/Complete buttons did
/// nothing at all.
fn giver_intents(script: &mut UiScript) {
    // `CloseGossip` (`0x4e2b20`) is the close core with a zero guid and sends no packet.
    crate::gossip_vm::drain_close(script);
    let giver = net::ui_giver();
    let npc = giver.npc;
    if npc == 0 {
        // No window open: take the queues so they do not grow, send nothing.
        script.take_gossip_selects();
        script.take_gossip_quest_selects();
        script.take_quest_selects();
        script.take_quest_actions();
        return;
    }
    let rows: Vec<(u32, u32)> = giver
        .quests
        .iter()
        .take(giver.quest_count as usize)
        .map(|q| (q.quest_id, q.icon))
        .collect();
    // `SelectGossipOption` (`0x4e2320`): the 1-based row's wire index, refused while the
    // greeting's query is still in flight (the latch at `[0xbbb670]`). A coded option is greyed,
    // as its prompt (`GOSSIP_ENTER_CODE`, `0x4e237e`) is not built.
    let gossip_npc = crate::gossip_vm::npc();
    let pending = crate::gossip_vm::text_pending();
    for pos in script.take_gossip_selects() {
        if gossip_npc == 0 || pending {
            continue;
        }
        if let Some(opt) = crate::gossip_vm::option_at(pos) {
            if !opt.coded {
                net::note(format!("giver|select|{gossip_npc:#x}|{}", opt.index));
                let _ = net::send_cmd(Cmd::GossipSelect {
                    npc: gossip_npc,
                    index: opt.index,
                });
            }
        }
    }
    // The gossip menu's quest rows are the whole list, in wire order.
    for pos in script.take_gossip_quest_selects() {
        if gossip_npc == 0 || pending {
            continue;
        }
        if let Some(q) = crate::gossip_vm::quest_at(pos) {
            net::note(format!(
                "giver|quest-row|{gossip_npc:#x}|{}|{}",
                q.quest_id, q.icon
            ));
            let _ = net::send_cmd(quest_row_cmd(gossip_npc, q.quest_id, q.icon));
        }
    }
    // The greeting panel shows the two pools apart, so its row index counts
    // within the pool the click came from.
    for sel in script.take_quest_selects() {
        let pool: Vec<(u32, u32)> = rows
            .iter()
            .copied()
            .filter(|&(_, icon)| matches!(icon, 3 | 4) == sel.active)
            .collect();
        if let Some(&(quest, icon)) = sel.index.checked_sub(1).and_then(|i| pool.get(i as usize)) {
            net::note(format!("giver|quest-pick|{npc:#x}|{quest}|{icon}"));
            let _ = net::send_cmd(quest_row_cmd(npc, quest, icon));
        }
    }
    for action in script.take_quest_actions() {
        let cmd = match action {
            QuestAction::Accept => Cmd::QuestAccept {
                npc,
                quest: giver.quest_id,
            },
            QuestAction::Continue => Cmd::QuestRequestReward {
                npc,
                quest: giver.quest_id,
            },
            QuestAction::Reward(choice) => Cmd::QuestReward {
                npc,
                quest: giver.quest_id,
                reward: choice,
            },
            // Decline re-opens the giver and Close tears the panel down; neither
            // has a packet of its own here — the server's own reply closes the frame.
            QuestAction::Decline | QuestAction::Close => continue,
        };
        net::note(format!("giver|action|{npc:#x}"));
        let _ = net::send_cmd(cmd);
    }
}

/// Resolve a unit token to a guid through the pushed resolver state, following `target` hops.
fn resolve_token(token: &str, guids: &UnitGuids) -> Option<u64> {
    let UnitTokenParse::Unit { base, hops } = benilla_ui::script::parse_unit_token(token) else {
        return None;
    };
    let mut guid = match base {
        UnitBase::Player => guids.player,
        UnitBase::Pet => guids.pet,
        UnitBase::Target => guids.target,
        UnitBase::Mouseover => guids.mouseover,
        UnitBase::Npc => guids.npc,
        UnitBase::Party(i) => guids.party.get(i as usize).copied().unwrap_or(0),
        UnitBase::PartyPet(i) => guids.party_pets.get(i as usize).copied().unwrap_or(0),
        UnitBase::Raid(i) => guids.raid.get(i as usize).copied().unwrap_or(0),
        UnitBase::RaidPet(i) => guids.raid_pets.get(i as usize).copied().unwrap_or(0),
    };
    for _ in 0..hops {
        guid = guids.held.get(&guid).copied().unwrap_or(0);
    }
    (guid != 0).then_some(guid)
}

/// The guid a unit token names right now, 0 when it names no live unit: the same resolver state
/// the VM's own `UnitGUID` reads.
pub(crate) fn token_guid(token: &str) -> u64 {
    FEED.with(|f| resolve_token(token, &f.borrow().guids).unwrap_or(0))
}

/// The descriptor's `(bag, slot)` is the client's own addressing; the wire wants
/// `Player::GetItemByPos`'s, where the backpack is not bag 0 but the player's array at
/// 255/`SLOT_PACK_FIRST`..38. A worn item (255) and an item inside an equipped bag (that bag's
/// player-array slot 19..22, with its inner slot) already carry the wire address.
fn wire_pos(bag: u8, slot: u8) -> (u8, u8) {
    if bag == 0 {
        (BAG_PLAYER_INVENTORY, SLOT_PACK_FIRST + slot)
    } else {
        (bag, slot)
    }
}

/// The Lua container id a cursor verb queued is the client's own addressing; the wire wants
/// `Player::GetItemByPos`'s. Ported with the reference's ranges
/// (`benilla-app/src/ui_items/mod.rs:90`), bank and keyring sentinels included.
fn lua_wire_pos(bag: i64, slot1: u32) -> Option<(u8, u8)> {
    const PACK_SLOTS: u8 = 16;
    const BANK_SLOTS: u8 = 24;
    const KEYRING_SLOTS: u8 = 16;
    const BANK_SLOT_FIRST: u8 = 39;
    const BANK_BAG_SLOT_FIRST: u8 = 63;
    const KEYRING_SLOT_FIRST: u8 = 81;
    const BANK_CONTAINER: i64 = -1;
    const BANK_BAG_ID_FIRST: i64 = 5;
    const KEYRING_CONTAINER: i64 = -2;
    let slot0 = u8::try_from(slot1.checked_sub(1)?).ok()?;
    match bag {
        0 if slot0 < PACK_SLOTS => Some((BAG_PLAYER_INVENTORY, SLOT_PACK_FIRST + slot0)),
        1..=4 if slot0 < 36 => Some((SLOT_BAG_FIRST + (bag as u8 - 1), slot0)),
        BANK_CONTAINER if slot0 < BANK_SLOTS => {
            Some((BAG_PLAYER_INVENTORY, BANK_SLOT_FIRST + slot0))
        }
        5..=10 if slot0 < 36 => {
            Some((BANK_BAG_SLOT_FIRST + (bag - BANK_BAG_ID_FIRST) as u8, slot0))
        }
        KEYRING_CONTAINER if slot0 < KEYRING_SLOTS => {
            Some((BAG_PLAYER_INVENTORY, KEYRING_SLOT_FIRST + slot0))
        }
        EQUIPMENT_BAG if (1..=23).contains(&slot1) || (64..=69).contains(&slot1) => {
            Some((BAG_PLAYER_INVENTORY, slot0))
        }
        _ => None,
    }
}

/// The inventory row at a wire position: the rows carry their own addressing, 255 (`BAG_EQUIPPED`)
/// for worn equipment and 0 for the backpack, whose wire seats are `SLOT_PACK_FIRST`..
fn inv_row_at(wire: (u8, u8)) -> Option<net::BnNetInvItem> {
    net::ui_inventory().into_iter().find(|r| {
        if wire.0 == BAG_PLAYER_INVENTORY {
            if r.bag == BAG_PLAYER_INVENTORY || r.bag == net::BAG_EQUIPPED {
                r.slot == wire.1
            } else {
                r.bag == 0 && wire.1 >= SLOT_PACK_FIRST && r.slot == wire.1 - SLOT_PACK_FIRST
            }
        } else {
            r.bag == wire.0 && r.slot == wire.1
        }
    })
}

/// `ITEM_CLASS_PROJECTILE` (`benilla-app/src/ui_items/mod.rs:859`), the template class the
/// auto-equip sender loads by entry instead of moving (`0x5e16ff`).
const ITEM_CLASS_PROJECTILE: u32 = 6;

/// The container drains the reference runs every frame (`ui_items/drain.rs`): the cursor verbs queue
/// the intent and the drain only sends. A whole stack with both ends in the player's own array is
/// `CMSG_SWAP_INV_ITEM`, anything crossing into a bag is `CMSG_SWAP_ITEM` (destination first,
/// `Server/Packets/Item.cpp:30-36`), a split is `CMSG_SPLIT_ITEM` and `PutItemInBag` is
/// `CMSG_AUTOSTORE_BAG_ITEM` with no seat, which the server picks.
///
/// The bind-on-equip deferral (`0x5e0c40`) is not ported: this host raises no `EQUIP_BIND_CONFIRM`
/// popup, so a move into an equipment position equips straight away.
fn container_moves(script: &mut UiScript) {
    for mv in script.take_container_moves() {
        let (Some(src), Some(dst)) = (
            lua_wire_pos(mv.src_bag, mv.src_slot),
            lua_wire_pos(mv.dst_bag, mv.dst_slot),
        ) else {
            net::note(format!("container move {mv:?} is not a wire position"));
            continue;
        };
        match mv.count {
            None if src.0 == BAG_PLAYER_INVENTORY && dst.0 == BAG_PLAYER_INVENTORY => {
                net::note(format!(
                    "ui_items: swap backpack lua {}→{} (wire 255 slot {}↔{})",
                    mv.src_slot, mv.dst_slot, src.1, dst.1
                ));
                let _ = net::send_cmd(Cmd::SwapInv {
                    src: src.1,
                    dst: dst.1,
                });
            }
            None => {
                net::note(format!(
                    "ui_items: swap whole-space lua {}/{}→{}/{} (wire {}/{})→({}/{})",
                    mv.src_bag, mv.src_slot, mv.dst_bag, mv.dst_slot, src.0, src.1, dst.0, dst.1
                ));
                let _ = net::send_cmd(Cmd::SwapItem {
                    dst_bag: dst.0,
                    dst_slot: dst.1,
                    src_bag: src.0,
                    src_slot: src.1,
                });
            }
            Some(n) => {
                let count = n.min(u32::from(u8::MAX)) as u8;
                net::note(format!(
                    "ui_items: split lua {}/{}→{}/{} × {count} (wire {}/{})→({}/{})",
                    mv.src_bag, mv.src_slot, mv.dst_bag, mv.dst_slot, src.0, src.1, dst.0, dst.1
                ));
                let _ = net::send_cmd(Cmd::SplitItem {
                    src_bag: src.0,
                    src_slot: src.1,
                    dst_bag: dst.0,
                    dst_slot: dst.1,
                    count,
                });
            }
        }
    }
    for a in script.take_bag_autostores() {
        let (Some((src_bag, src_slot)), Some((dst_bag, _))) = (
            lua_wire_pos(a.src_bag, a.src_slot),
            lua_wire_pos(a.dst_bag, 1),
        ) else {
            net::note(format!("ui_items: bag autostore {a:?} out of range"));
            continue;
        };
        match a.count {
            None => {
                net::note(format!(
                    "ui_items: autostore lua {}/{} → bag {} (wire {src_bag}/{src_slot} → {dst_bag})",
                    a.src_bag, a.src_slot, a.dst_bag
                ));
                let _ = net::send_cmd(Cmd::AutoStore {
                    src_bag,
                    src_slot,
                    dst_bag,
                });
            }
            Some(n) => {
                let count = n.min(u32::from(u8::MAX)) as u8;
                net::note(format!(
                    "ui_items: autostore split lua {}/{} → bag {} × {count} (wire {src_bag}/{src_slot} → {dst_bag})",
                    a.src_bag, a.src_slot, a.dst_bag
                ));
                let _ = net::send_cmd(Cmd::SplitItem {
                    src_bag,
                    src_slot,
                    dst_bag,
                    // The reference's literal for no destination slot.
                    dst_slot: 0xFF,
                    count,
                });
            }
        }
    }
    for (bag, slot) in script.take_container_autoequips() {
        let Some(wire) = lua_wire_pos(bag, slot) else {
            continue;
        };
        match inv_row_at(wire).filter(|r| r.info.class == ITEM_CLASS_PROJECTILE) {
            Some(ammo) => {
                net::note(format!(
                    "ui_items: autoequip lua {bag}/{slot} is ammo entry {} — loaded",
                    ammo.entry
                ));
                let _ = net::send_cmd(Cmd::SetAmmo(ammo.entry));
            }
            None => {
                net::note(format!(
                    "ui_items: autoequip lua {bag}/{slot} (wire {}/{})",
                    wire.0, wire.1
                ));
                let _ = net::send_cmd(Cmd::AutoEquip {
                    bag: wire.0,
                    slot: wire.1,
                });
            }
        }
    }
    // `DeleteCursorItem` (`0x4897b0`) queues the item it dropped and relocks it (`0x4898bc`) with
    // no reply; the verb's Lua container resolves to the wire position and a 0 count is the whole
    // stack (`ui_items/drain.rs:741`).
    for (bag, slot, count) in script.take_container_destroys() {
        let Some((bag_index, wire_slot)) = lua_wire_pos(bag, slot) else {
            net::note(format!(
                "ui_items: destroy lua {bag}/{slot} is not a wire position"
            ));
            continue;
        };
        let count = count.min(u32::from(u8::MAX)) as u8;
        net::note(format!(
            "ui_items: destroy lua {bag}/{slot} × {count} (wire {bag_index}/{wire_slot})"
        ));
        let _ = net::send_cmd(Cmd::DestroyItem {
            bag: bag_index,
            slot: wire_slot,
            count,
        });
    }
    // A bind-on-equip question keeps the item on the cursor and sends nothing; taking the queue keeps
    // it from growing while the popup is unimplemented.
    let _ = script.take_container_equip_asks();
    // `UseInventoryItem(id, onSelf)` — the doll's right-click — reaches the same `CGItem::Use` a bag
    // click does (`0x4c7af0`), so it is the same `CMSG_USE_ITEM` at wire 255/(id-1). The app refuses
    // ids outside 1..=19, the equipped-bag band and ammo having no use path (`ui_items/drain.rs:171`).
    for id in script.take_inventory_uses() {
        if !(1..=19).contains(&id) {
            net::note(format!(
                "ui_items: UseInventoryItem({id}) out of range — ignored"
            ));
            continue;
        }
        let slot = (id - 1) as u8;
        net::note(format!(
            "ui_items: use equipped item, lua slot {id} (wire 255/{slot})"
        ));
        let _ = net::send_cmd(Cmd::UseItem {
            bag: BAG_PLAYER_INVENTORY,
            slot,
            spell: 0,
        });
    }
}

/// `UseContainerItem` past its gates (`0x4fa0e0`): the arm the VM took chooses the packet, so the
/// click behaves as the reference client's does — an equippable template auto-equips, a usable one
/// fires its item spell, and a click on a vendor-open slot sells the stack.
fn container_use(use_: &ContainerUse) {
    if use_.asks {
        // Bind-on-equip question: the item stays on the cursor under the popup, nothing sent.
        return;
    }
    let Some(item) = net::ui_inventory()
        .into_iter()
        .find(|i| i.guid == use_.guid)
    else {
        net::note(format!(
            "container: slot ({}, {}) has no item {} to send",
            use_.bag, use_.slot, use_.guid
        ));
        return;
    };
    let (bag, slot) = wire_pos(item.bag, item.slot);
    match use_.arm {
        UseArm::Item(UseFork::Equip) => {
            // The click's own packet, at the wire position the reference client uses. VMaNGOS 1.12
            // answers this with `EQUIP_ERR_ITEM_NOT_FOUND` while naming the item it just resolved
            // (probe 2026-10-29: `inv|fail|23|0|0x4000000000000016` for a pack item at 255/26), yet
            // the same item equips over `CMSG_SWAP_INV_ITEM` 26->4 — the drag gesture's packet.
            // The packet here stays the client's; the swap console is the host's equip instrument.
            let _ = net::send_cmd(Cmd::AutoEquip { bag, slot });
        }
        UseArm::Item(_) if use_.may_cast => {
            let _ = net::send_cmd(Cmd::UseItem {
                bag,
                slot,
                spell: 0,
            });
        }
        UseArm::Sell => {
            let vendor = net::ui_vendor();
            if vendor.guid != 0 {
                let _ = net::send_cmd(Cmd::VendorSell {
                    vendor: vendor.guid,
                    item_guid: use_.guid,
                    count: 0,
                });
            }
        }
        arm => net::note(format!("container: {arm:?} has no wire path yet")),
    }
}

/// `AttackTarget`'s toggle, `0x489b50` → `0x6131a0`, also the attack button's
/// `UseAction` slot (`0x6e4c90`). The validator picks first: the selection is kept
/// unless it is empty or friendly toward us (`0x6130a3`), and only then, with the
/// gate's tag 4, does the cycle commit a nearest enemy (`0x6130bc`). Then the
/// toggle: swinging (`0x60ecb0`) stops, else the kept unit is swung at.
fn attack_press(script: &mut UiScript, may_pick: bool) {
    let mut target = net::ui_selection();
    if may_pick && (target == 0 || !attack_keeps(target)) {
        if let Some(guid) = nearest_candidate(NearestMode::Enemy, false, 0) {
            target = guid;
            net::select(guid);
        }
    }
    if target == 0 {
        // `0x6130d9`: the empty selection is the validator's one worded refusal.
        fire_error(script, "ERR_NO_ATTACK_TARGET");
        return;
    }
    if net::ui_self_attacking() {
        net::stop_attack();
    } else if attack_keeps(target) {
        let _ = net::send_cmd(Cmd::Attack(target));
    } else {
        // The final gate's own reaction test (`0x61312e`): a friendly unit is
        // refused with a line, and no swing goes out.
        fire_error(script, "ERR_INVALID_ATTACK_TARGET");
    }
}

/// `0x6130a3`: the actor's reaction to the held unit alone — friendly (4) is not
/// kept, so the cycle runs instead; a unit that does not resolve is kept for the
/// second resolve to refuse.
fn attack_keeps(guid: u64) -> bool {
    let Some(unit) = net::ui_unit(guid) else {
        return true;
    };
    let (Some(t), Some(p)) = (
        faction_template(&unit.fields),
        net::ui_self_fields().as_ref().and_then(faction_template),
    ) else {
        return true;
    };
    t.reaction_toward(&p) != Reaction::Friendly
}

/// The right click's attack fork (`cursor_mode.rs`'s `can_attack`, asked at `0x482680`): hostile
/// toward us or we toward it, the same two-way test the enemy cycle uses. A faction that does not
/// resolve is read as hostile, as the reference reads a data gap: no missing DBC row may cost the
/// player their swing.
pub(crate) fn attackable(guid: u64) -> bool {
    let Some(unit) = net::ui_unit(guid) else {
        return false;
    };
    let (Some(t), Some(p)) = (
        faction_template(&unit.fields),
        net::ui_self_fields().as_ref().and_then(faction_template),
    ) else {
        return true;
    };
    t.reaction_toward(&p) == Reaction::Hostile || p.reaction_toward(&t) == Reaction::Hostile
}

/// `TargetNearest*`: the next (or, reversed, previous) candidate in distance order from the body.
fn nearest_candidate(mode: NearestMode, reverse: bool, current: u64) -> Option<u64> {
    let player = net::ui_player()?;
    let party = net::ui_party();
    let party_guids: Vec<u64> = party.members.iter().take(4).map(|m| m.guid).collect();
    let names = net::ui_names();
    let mut rows: Vec<(f32, u64)> = Vec::new();
    for guid in net::ui_unit_guids() {
        if guid == player.guid {
            continue;
        }
        let Some(unit) = net::ui_unit(guid) else {
            continue;
        };
        let eligible = match mode {
            NearestMode::Enemy => {
                let Some(t) = faction_template(&unit.fields) else {
                    continue;
                };
                let Some(p) = net::ui_self_fields().as_ref().and_then(faction_template) else {
                    continue;
                };
                t.reaction_toward(&p) == Reaction::Hostile
                    || p.reaction_toward(&t) == Reaction::Hostile
            }
            NearestMode::Friend => {
                let Some(t) = faction_template(&unit.fields) else {
                    continue;
                };
                let Some(p) = net::ui_self_fields().as_ref().and_then(faction_template) else {
                    continue;
                };
                t.reaction_toward(&p) == Reaction::Friendly
            }
            NearestMode::PartyMember | NearestMode::RaidMember => party_guids.contains(&guid),
        };
        if !eligible || !names.contains_key(&guid) && mode == NearestMode::PartyMember {
            // Party rows still resolve through the roster even with no name cache entry.
            if !eligible {
                continue;
            }
        }
        let dx = unit.pos[0] - player.pos[0];
        let dy = unit.pos[1] - player.pos[1];
        let dz = unit.pos[2] - player.pos[2];
        rows.push((dx * dx + dy * dy + dz * dz, guid));
    }
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let index = rows.iter().position(|(_, g)| *g == current);
    let next = match (index, reverse) {
        (Some(0), true) => rows.len() - 1,
        (Some(i), true) => i - 1,
        (Some(i), false) => (i + 1) % rows.len(),
        (None, true) => rows.len() - 1,
        (None, false) => 0,
    };
    Some(rows[next].1)
}

/// The faction template behind a descriptor, off the handle's own loaded catalog.
fn faction_template(fields: &ObjectFields) -> Option<FactionTemplate> {
    let id = fields.unit_faction_template()?;
    CATS.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|c| c.factions.as_ref())
            .and_then(|f| f.template(id))
            .cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::{loot_picks, money_text, HeldAxes, LootPick, LootWindow};
    use crate::net::BnNetLoot;
    use benilla_ui::script::UiScript;

    #[test]
    fn a_and_d_turn_until_the_look_channel_is_held() {
        let mut held = HeldAxes::default();
        held.turn_left = true;
        held.turn_right = true;
        assert_eq!(held.axes(false), (0.0, 0.0, 0.0), "A+D cancel");
        held.turn_right = false;
        assert_eq!(held.axes(false), (0.0, 0.0, 1.0), "A turns left");
        // `0x7c5360`: with the right button's channel held the same key strafes
        // left and no keyboard turn runs.
        assert_eq!(held.axes(true), (0.0, -1.0, 0.0), "A strafes left");
        held.turn_left = false;
        held.strafe_right = true;
        assert_eq!(held.axes(true), (0.0, 1.0, 0.0), "D strafes right");
        assert_eq!(held.axes(false), (0.0, 1.0, 0.0), "Q/E strafe either way");
    }

    fn window() -> LootWindow {
        LootWindow {
            guid: 7,
            coin: true,
            slots: vec![0, 1, 2],
            picks: Vec::new(),
            last: None,
            opened: false,
            released: false,
        }
    }

    fn wire(gold: u32, slots: &[u8]) -> BnNetLoot {
        let mut wire = BnNetLoot {
            guid: 7,
            gold,
            ..Default::default()
        };
        for &slot in slots {
            wire.items[wire.count as usize].slot = slot;
            wire.items[wire.count as usize].item_id = 100 + u32::from(slot);
            wire.count += 1;
        }
        wire
    }

    /// The window's rows hold still: a taken row is a gap at its own place, so the rows below it
    /// keep their indices and `GetNumLootItems` (`LootFrame.lua:132`) never moves.
    #[test]
    fn a_taken_row_keeps_its_place_in_the_layout() {
        let w = window();
        assert_eq!(
            loot_picks(&w, &wire(45, &[0, 1, 2])),
            vec![
                Some(LootPick::Coin),
                Some(LootPick::Item(0)),
                Some(LootPick::Item(1)),
                Some(LootPick::Item(2))
            ]
        );
        // The middle row taken: its slot is gone from the response, the layout is not.
        assert_eq!(
            loot_picks(&w, &wire(45, &[0, 2])),
            vec![
                Some(LootPick::Coin),
                Some(LootPick::Item(0)),
                None,
                Some(LootPick::Item(2))
            ]
        );
        // Coin taken last, every item gone: four gaps, not a shorter list.
        assert_eq!(loot_picks(&w, &wire(0, &[])), vec![None, None, None, None]);
        let mut no_coin = window();
        no_coin.coin = false;
        assert_eq!(
            loot_picks(&no_coin, &wire(45, &[])),
            vec![None, None, None],
            "a window opened without gold has no coin row"
        );
    }

    /// The coin row's words come off the player's own `GlobalStrings`; a bare VM has none, so the
    /// enUS words stand in (`ui_loot`'s `format_money`).
    #[test]
    fn the_coin_row_reads_money_words() {
        let script = UiScript::new().unwrap();
        assert_eq!(money_text(&script, 0), "0 Copper");
        assert_eq!(money_text(&script, 45), "45 Copper");
        assert_eq!(money_text(&script, 12345), "1 Gold 23 Silver 45 Copper");
        assert_eq!(money_text(&script, 20000), "2 Gold");
    }
}

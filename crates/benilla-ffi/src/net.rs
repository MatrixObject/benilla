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

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};

use benilla_protocol::messages::{Object, ObjectFields, ObjectType, ServerPacket};
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
    GameObjUse(u64),
    QuestAccept { npc: u64, quest: u32 },
    QuestComplete { npc: u64, quest: u32 },
    QuestQuery(u32),
    /// Cast `spell` at `target` (0 = self), answered by `SMSG_CAST_RESULT`.
    Cast { spell: u32, target: u64 },
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

fn copy_name(dst: &mut [c_char; 32], name: &str) {
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
            Ok(packet) => handle_packet(&packet, sh),
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

/// Fold one `SMSG_UPDATE_OBJECT` entry into the tracked object table.
fn apply_object(obj: &Object, sh: &Shared) {
    let self_guid = sh.self_guid.load(Ordering::SeqCst);
    let mut units = sh.units.lock().unwrap_or_else(|e| e.into_inner());
    match obj {
        Object::Values { guid, mask } => {
            if let Some(rec) = units.get_mut(guid) {
                rec.fields.merge(mask.clone());
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
        ServerPacket::LevelUp(_)
        | ServerPacket::LogoutComplete
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
///   8 = quest query (b=quest), 9 = logout.
/// Returns 1 when the command was queued.
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
        _ => return 0,
    };
    send_cmd(cmd) as u32
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
    let f = &rec.fields;
    // Player appearance bytes; creatures leave all of this zero.
    let bytes0 = f.unit_bytes_0().unwrap_or(0);
    let pb = f.player_bytes().unwrap_or(0);
    let pb2 = f.player_bytes_2().unwrap_or(0);
    let row = BnNetUnit {
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
    };
    unsafe { *out = row };
    1
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

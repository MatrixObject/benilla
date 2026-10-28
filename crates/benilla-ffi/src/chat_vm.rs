//! The chat router across the FFI boundary: a wire `SMSG_MESSAGE_CHAT` becomes the `CHAT_MSG_*`
//! event the stock `ChatFrame_OnEvent` prints, queued on the network thread and fired on the UI
//! tick.
//!
//! Port of the app's `crates/benilla-app/src/ui_chat/{event,frames}.rs` for the types the wire
//! carries: the same ten `arg1..arg10` in the reference's order, the same `CHAT_MSG_<ChatTypeInfo
//! key>` event name (`ChatFrame.lua` recovers the key with `strsub(event, 10)`), the same
//! `<AFK>`/`<DND>`/`<GM>` tag token and the same language-name table. The combat log never arrives
//! as wire chat in 1.12, so an unmapped type byte is dropped, exactly as the app's router does.

use benilla_protocol::messages::{chat_tag, ChatMessage};
use benilla_ui::script::ScriptValue;

use crate::net;

/// The reference's event name for a wire type byte; `None` for a type it never sends as chat.
fn event_name(chat_type: u8) -> Option<&'static str> {
    use benilla_protocol::messages as m;
    Some(match chat_type {
        m::CHAT_MSG_SAY => "CHAT_MSG_SAY",
        m::CHAT_MSG_PARTY => "CHAT_MSG_PARTY",
        m::CHAT_MSG_RAID => "CHAT_MSG_RAID",
        m::CHAT_MSG_GUILD => "CHAT_MSG_GUILD",
        m::CHAT_MSG_OFFICER => "CHAT_MSG_OFFICER",
        m::CHAT_MSG_YELL => "CHAT_MSG_YELL",
        m::CHAT_MSG_WHISPER => "CHAT_MSG_WHISPER",
        m::CHAT_MSG_WHISPER_INFORM => "CHAT_MSG_WHISPER_INFORM",
        m::CHAT_MSG_EMOTE => "CHAT_MSG_EMOTE",
        m::CHAT_MSG_SYSTEM => "CHAT_MSG_SYSTEM",
        m::CHAT_MSG_MONSTER_SAY => "CHAT_MSG_MONSTER_SAY",
        m::CHAT_MSG_MONSTER_YELL => "CHAT_MSG_MONSTER_YELL",
        m::CHAT_MSG_MONSTER_EMOTE => "CHAT_MSG_MONSTER_EMOTE",
        m::CHAT_MSG_CHANNEL => "CHAT_MSG_CHANNEL",
        m::CHAT_MSG_AFK => "CHAT_MSG_AFK",
        m::CHAT_MSG_DND => "CHAT_MSG_DND",
        m::CHAT_MSG_IGNORED => "CHAT_MSG_IGNORED",
        m::CHAT_MSG_MONSTER_WHISPER | m::CHAT_MSG_RAID_BOSS_WHISPER => "CHAT_MSG_MONSTER_WHISPER",
        m::CHAT_MSG_RAID_LEADER => "CHAT_MSG_RAID_LEADER",
        m::CHAT_MSG_RAID_WARNING => "CHAT_MSG_RAID_WARNING",
        m::CHAT_MSG_RAID_BOSS_EMOTE => "CHAT_MSG_RAID_BOSS_EMOTE",
        m::CHAT_MSG_FILTERED => "CHAT_MSG_FILTERED",
        m::CHAT_MSG_BATTLEGROUND => "CHAT_MSG_BATTLEGROUND",
        m::CHAT_MSG_BATTLEGROUND_LEADER => "CHAT_MSG_BATTLEGROUND_LEADER",
        m::CHAT_MSG_BG_SYSTEM_NEUTRAL => "CHAT_MSG_BG_SYSTEM_NEUTRAL",
        m::CHAT_MSG_BG_SYSTEM_ALLIANCE => "CHAT_MSG_BG_SYSTEM_ALLIANCE",
        m::CHAT_MSG_BG_SYSTEM_HORDE => "CHAT_MSG_BG_SYSTEM_HORDE",
        _ => return None,
    })
}

/// The `arg6` flag token, shown as the name's `<AFK>`/`<DND>`/`<GM>` prefix.
fn flag_of_tag(chat_tag: u8) -> &'static str {
    match chat_tag {
        chat_tag::AFK => "AFK",
        chat_tag::DND => "DND",
        chat_tag::GM => "GM",
        _ => "",
    }
}

/// The 1.12 language names by id (vmangos `LANG_*`); id 0 and unknown ids answer empty, which is
/// what suppresses the `[Orcish]` header (`ChatFrame.lua:1442` tests `arg3 ~= "Universal"`).
fn language_name(id: u32) -> &'static str {
    match id {
        1 => "Orcish",
        2 => "Darnassian",
        3 => "Taurahe",
        6 => "Dwarvish",
        7 => "Common",
        8 => "Demonic",
        9 => "Titan",
        10 => "Thalassian",
        11 => "Draconic",
        12 => "Kalimag",
        13 => "Gnomish",
        14 => "Troll",
        33 => "Gutterspeak",
        _ => "",
    }
}

/// Queue one wire line for the VM. Runs on the network thread: the name resolution reads the
/// shared caches only, so nothing here touches the Lua VM.
pub(crate) fn enqueue(msg: &ChatMessage) {
    let Some(name) = event_name(msg.chat_type) else {
        return;
    };
    if msg.is_addon() {
        return;
    }
    let names = net::ui_names();
    // A monster line carries its name in the packet; a player line is the guid `queue_player_names`
    // asked for, and an unanswered one prints with no name rather than holding the line.
    let sender = match &msg.sender_name {
        Some(n) => n.clone(),
        None => names.get(&msg.sender_guid).cloned().unwrap_or_default(),
    };
    let target = if msg.target_guid != 0 {
        names.get(&msg.target_guid).cloned().unwrap_or_default()
    } else {
        String::new()
    };
    let channel = msg.channel.clone().unwrap_or_default();
    let args = vec![
        ScriptValue::Str(msg.text.clone()),
        ScriptValue::Str(sender),
        ScriptValue::Str(language_name(msg.language).to_string()),
        ScriptValue::Str(channel.clone()),
        ScriptValue::Str(target),
        ScriptValue::Str(flag_of_tag(msg.chat_tag).to_string()),
        ScriptValue::Int(0),
        ScriptValue::Int(0),
        ScriptValue::Str(channel),
        ScriptValue::Int(0),
    ];
    net::push_chat_event(name, args);
}

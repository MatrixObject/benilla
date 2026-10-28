//! The quest log's VM side, ported from `crates/benilla-app/src/ui_quest_log.rs`.
//!
//! The stock `QuestLogFrame` lists exactly what `GetNumQuestLogEntries` counts, and that count is
//! the rows pushed here: our `PLAYER_QUEST_LOG` descriptor slots whose template has landed. A slot
//! with no cached template owes a `CMSG_QUEST_QUERY` and stays out of the list until the answer
//! arrives, as the reference's rebuild (`0x4de510`) skips an uncached quest and counts it in
//! `[0xbb7498]` (`0x4de67e`).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use benilla_protocol::messages::quest_slot_state;
use benilla_ui::script::{
    QuestItemView, QuestLogDetail, QuestLogEntryView, QuestLogObjectiveView, QuestLogState,
    UiScript,
};
use benilla_ui::strings::{fill, Arg};

use crate::net::{self, Cmd};
use crate::ui_feed::{self, Catalogs};

/// Marks a GameObject objective's entry on the wire (vmangos `Quest.cpp:512-516`).
const GO_OBJECTIVE_BIT: u32 = 0x8000_0000;
/// `QUEST_FLAGS_SHARABLE` (vmangos `QuestDef.h:153`), the one test behind `GetQuestLogPushable`.
const QUEST_FLAGS_SHARABLE: u32 = 0x8;
/// The reference's placeholder for a name its cache does not hold yet (`0x4e03f1`).
const NAME_PLACEHOLDER: &str = "...";
/// A quest whose `ZoneOrSort` is 0 has no header id; the reference names the group after the gap.
const MISSING_HEADER: &str = "Missing header! (quest designers)";

/// One row's grouping and ordering inputs, before the headers are interleaved.
struct GroupRow {
    zone_or_sort: i32,
    header: String,
    level: u32,
    title: String,
}

/// What the feed remembers between ticks: the ids whose query is already out, and the last
/// snapshot pushed, so a repaint fires nothing.
#[derive(Default)]
struct QuestLogFeed {
    asked: HashSet<u32>,
    last: Option<QuestLogState>,
}

impl QuestLogFeed {
    /// Drop the session's memory: a new character has a new log and may re-ask every template.
    fn clear(&mut self) {
        self.asked.clear();
        self.last = None;
    }
}

fn state() -> &'static Mutex<QuestLogFeed> {
    static S: OnceLock<Mutex<QuestLogFeed>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(QuestLogFeed::default()))
}

fn lock() -> std::sync::MutexGuard<'static, QuestLogFeed> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// One item's live count across the bags; an item objective has no slot counter, so the reference
/// reads the inventory (`0x4e04f4`).
fn bag_count(items: &[(u32, u32)], entry: u32) -> u32 {
    items
        .iter()
        .filter(|&&(id, _)| id == entry)
        .map(|&(_, n)| n)
        .sum()
}

/// One leaderboard line from its `%s`/`%d`/`%d` key, read off the VM's own `GlobalStrings.lua`.
fn leaderboard_text(
    script: &UiScript,
    key: &str,
    name: &str,
    cur: u32,
    req: u32,
) -> Option<String> {
    let template = global_string(script, key)?;
    Some(fill(
        &template,
        &[Arg::S(name), Arg::D(i64::from(cur)), Arg::D(i64::from(req))],
    ))
}

/// One `GlobalStrings` row; `None` when the patch chain's Lua has no such key.
fn global_string(script: &UiScript, key: &str) -> Option<String> {
    script
        .lua()
        .globals()
        .get::<String>(key)
        .ok()
        .filter(|t| !t.is_empty())
}

/// This quest's objective lines: creatures and GameObjects first, in array order (the counter
/// index is the array index, empty quads included), then items (`0x4e0000`, `0x4e0110`).
fn build_objectives(
    script: &UiScript,
    template: &net::QuestTemplateLite,
    counters: &[u8; 4],
    items: &[(u32, u32)],
    cats: Option<&Catalogs>,
) -> Vec<QuestLogObjectiveView> {
    let mut lines = Vec::new();
    for (i, obj) in template.objective_rows.iter().enumerate() {
        if obj.creature_or_go == 0 {
            continue;
        }
        let is_go = obj.creature_or_go & GO_OBJECTIVE_BIT != 0;
        let entry = obj.creature_or_go & !GO_OBJECTIVE_BIT;
        // A non-empty override text is the name and takes `QUEST_OBJECTS_FOUND` (`0x4e03da`),
        // while `kind` stays "monster" (`0x4e04ca`). A GameObject has no name cache to read.
        let cached = net::ui_creature(entry)
            .map(|c| c.name)
            .filter(|n| !n.is_empty());
        let named = if !obj.text.is_empty() {
            obj.text.as_str()
        } else if is_go {
            NAME_PLACEHOLDER
        } else {
            cached.as_deref().unwrap_or(NAME_PLACEHOLDER)
        };
        let key = if !obj.text.is_empty() || is_go {
            "QUEST_OBJECTS_FOUND"
        } else {
            "QUEST_MONSTERS_KILLED"
        };
        let req = obj.required_count;
        let cur = u32::from(counters.get(i).copied().unwrap_or(0)).min(req);
        if let Some(text) = leaderboard_text(script, key, named, cur, req) {
            lines.push(QuestLogObjectiveView {
                text,
                kind: if is_go { "object" } else { "monster" }.into(),
                finished: cur >= req,
                cur,
                req,
            });
        }
    }
    for obj in template.objective_rows.iter() {
        if obj.item_id == 0 || obj.item_id == template.src_item_id {
            continue;
        }
        let req = obj.item_count;
        let cur = bag_count(items, obj.item_id).min(req);
        let (name, _, _) = ui_feed::item_look(&net::ui_item_info(obj.item_id), cats);
        let shown = name.as_deref().unwrap_or(NAME_PLACEHOLDER);
        if let Some(text) = leaderboard_text(script, "QUEST_ITEMS_NEEDED", shown, cur, req) {
            lines.push(QuestLogObjectiveView {
                text,
                kind: "item".into(),
                finished: cur >= req,
                cur,
                req,
            });
        }
    }
    lines
}

/// A template reward or choice `(id, count)` as a row; `SMSG_QUEST_QUERY_RESPONSE` carries no
/// display id, so the icon comes from the item template through the display catalog.
fn item_row(entry: u32, count: u32, cats: Option<&Catalogs>) -> QuestItemView {
    let info = net::ui_item_info(entry);
    let (name, quality, texture) = ui_feed::item_look(&info, cats);
    QuestItemView {
        link: name.as_ref().map(|n| ui_feed::item_link(entry, n, quality)),
        name,
        texture,
        count,
        quality,
        item_id: entry,
        usable: true,
    }
}

/// Group the rows by `ZoneOrSort` and order them the way the reference's rebuild (`0x4dea50`)
/// does: the ungrouped block first, then each header alphabetically, and inside a header by level,
/// title and slot.
fn order_groups(rows: &[GroupRow]) -> Vec<(i32, String, Vec<usize>)> {
    let mut groups: Vec<(i32, String, Vec<usize>)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match groups.iter_mut().find(|(z, _, _)| *z == r.zone_or_sort) {
            Some((_, _, idxs)) => idxs.push(i),
            None => groups.push((r.zone_or_sort, r.header.clone(), vec![i])),
        }
    }
    groups.sort_by_key(|(zos, name, _)| {
        if *zos == 0 {
            (0u8, String::new())
        } else {
            (1, name.to_lowercase())
        }
    });
    for (_, _, idxs) in &mut groups {
        idxs.sort_by_key(|&i| {
            let r = &rows[i];
            (r.level, r.title.to_lowercase(), i)
        });
    }
    groups
}

/// Read our `PLAYER_QUEST_LOG` slots and push the log on a change. `cats` carries the two quest
/// DBCs; without client data no quest has a tag and every group is unnamed.
pub(crate) fn feed(script: &mut UiScript, cats: Option<&Catalogs>) {
    let headers = cats.and_then(|c| c.quest_headers.as_ref());
    let tags = cats.and_then(|c| c.quest_tags.as_ref());
    let Some(own) = net::ui_self_fields() else {
        return;
    };
    let mut slots: Vec<(u8, u32, [u8; 4], u8, u32)> = Vec::new();
    for i in 0..20u8 {
        let Some(q) = own.player_quest_log(i) else {
            continue;
        };
        if q.quest_id == 0 {
            continue;
        }
        slots.push((i, q.quest_id, q.counters, q.state, q.timer));
    }
    let templates = net::ui_quests();
    let items: Vec<(u32, u32)> = net::ui_inventory()
        .iter()
        .map(|it| (it.entry, it.count))
        .collect();

    let mut st = lock();
    // Ask once per id: the miss is what sends the query, and an id the server does not know stays
    // out of the list rather than re-asking every tick.
    for &(_, quest, _, _, _) in &slots {
        if !templates.contains_key(&quest) && st.asked.insert(quest) {
            net::send_cmd(Cmd::QuestQuery(quest));
        }
    }
    let rows: Vec<_> = slots
        .iter()
        .filter_map(|row| templates.get(&row.1).map(|t| (row, t)))
        .collect();

    let keyed: Vec<GroupRow> = rows
        .iter()
        .map(|(_, t)| GroupRow {
            zone_or_sort: t.zone,
            header: if t.zone == 0 {
                MISSING_HEADER.to_string()
            } else {
                headers
                    .and_then(|h| h.resolve(t.zone))
                    .unwrap_or_default()
                    .to_string()
            },
            level: t.level,
            title: t.title.clone(),
        })
        .collect();
    let groups = order_groups(&keyed);

    let mut entries: Vec<QuestLogEntryView> = Vec::new();
    for (_, name, idxs) in &groups {
        entries.push(QuestLogEntryView {
            title: name.clone(),
            is_header: true,
            ..Default::default()
        });
        for &i in idxs {
            let (slot, _) = rows[i];
            let template = rows[i].1;
            let complete = if slot.3 & quest_slot_state::COMPLETE != 0 {
                1
            } else if slot.3 & quest_slot_state::FAIL != 0 {
                -1
            } else {
                0
            };
            let (required_money, reward_money) = if template.money < 0 {
                ((-template.money) as u32, 0)
            } else {
                (0, template.money as u32)
            };
            entries.push(QuestLogEntryView {
                quest_id: slot.1,
                title: template.title.clone(),
                level: template.level,
                tag: tags
                    .and_then(|t| t.resolve(template.quest_type))
                    .map(str::to_string),
                is_header: false,
                collapsed: false,
                complete,
                timer: slot.4,
                pushable: template.flags & QUEST_FLAGS_SHARABLE != 0,
                objectives: build_objectives(script, template, &slot.2, &items, cats),
                detail: Some(QuestLogDetail {
                    description: template.details.clone(),
                    objectives_text: template.objectives.clone(),
                    required_money,
                    reward_money,
                    choices: template
                        .choices
                        .iter()
                        .filter(|&&(id, _)| id != 0)
                        .map(|&(id, count)| item_row(id, count, cats))
                        .collect(),
                    rewards: template
                        .rewards
                        .iter()
                        .filter(|&&(id, _)| id != 0)
                        .map(|&(id, count)| item_row(id, count, cats))
                        .collect(),
                    reward_spell: None,
                }),
            });
        }
    }
    let fresh = QuestLogState {
        entries,
        num_quests: slots.len() as u32,
        hidden_quest_ids: Vec::new(),
    };
    if st.last.as_ref() == Some(&fresh) {
        return;
    }
    st.last = Some(fresh.clone());
    script.set_quest_log(fresh);
    script.fire_event("QUEST_LOG_UPDATE", Vec::new());
}

/// Forget the session's log: a new character re-asks every template.
pub(crate) fn clear_session() {
    lock().clear();
}

//! The item template, item set and player-requirement views the stock item tooltip builds from.
//!
//! The reference holds one item cache and every tooltip reads it synchronously (`0x5d8b00`); the
//! VM's builder (`0x52b650`) walks a template's stats, damage, spells and requirements and prints
//! a key from `GlobalStrings.lua` for each. Without the templates pushed the builder takes its
//! uncached path and draws the row's name alone, so the middle layer feeds what the network
//! answered into [`UiScript::set_item_template`] as it lands, primes a fresh VM with the whole
//! cache, and answers the VM's asynchronously-recorded asks (`Model::item_stat_asks`,
//! `Model::item_set_asks`) — the same shape as `benilla-app`'s `ui_items::feed` lanes, minus the
//! app-only stores (spell modifiers, reputation and proficiency snapshots).

use std::collections::{HashMap, HashSet};

use benilla_protocol::messages::{ItemInfo, ItemSpellEntry, ObjectFields, PLAYER_SKILL_SLOTS};
use benilla_ui::script::{ItemSetView, ItemTemplateView, PlayerReqState, UiScript};
use benilla_ui::strings::Arg;

use crate::net;
use crate::ui_feed::Catalogs;

/// One load's state: which templates and sets have been pushed, and the player-requirement state
/// last sent. Reset with the VM (`ui_feed::attach` builds a new `Feed`).
#[derive(Default)]
pub(crate) struct ItemVm {
    /// Entries the VM asked about whose template has not landed.
    pending: HashSet<u32>,
    /// Set rows already pushed (or waiting on a member's name), so a member that resolves
    /// re-pushes the row exactly once.
    sets: HashMap<u32, ItemSetView>,
    /// The priming pass ran: every template cached at load was pushed.
    primed: bool,
    /// The player state the red requirement lines compare against, last pushed.
    req: Option<PlayerReqState>,
}

impl ItemVm {
    /// Push the load's whole template cache once, then answer the VM's asks and re-answer the
    /// ones still waiting on a query. Called from the UI tick, where the VM lives.
    pub(crate) fn feed(&mut self, script: &mut UiScript, cats: &Catalogs, own: &ObjectFields) {
        self.feed_player_req(script, cats, own);

        let mut ids = script.take_item_stat_asks();
        if !self.primed {
            self.primed = true;
            ids.extend(net::ui_cached_template_ids());
        }
        for id in ids {
            // A hover of an entry the cache lacks asks the server once, the reference's
            // `Items::template` miss query; a row it already answered with nothing stays.
            if self.pending.insert(id)
                && net::ui_item_template(id).is_none()
                && !net::ui_item_unknown(id)
            {
                net::send_cmd(net::Cmd::ItemQuery(id));
            }
        }
        for id in script.take_item_set_asks() {
            self.sets.entry(id).or_default();
        }
        // Both views read the VM's own `GlobalStrings.lua`, so they are built under one
        // immutable borrow and pushed after it ends.
        let (templates, sets) = {
            let skill = |spell_id: u32| spell_skill_value(cats, own, spell_id);
            let gender = own.unit_gender().unwrap_or(0);
            let tables = TooltipTables {
                cats,
                script,
                skill: &skill,
                gender,
            };
            let templates: Vec<(u32, ItemTemplateView)> = self
                .pending
                .iter()
                .filter_map(|&id| {
                    net::ui_item_template(id).map(|t| (id, template_view(&t, &tables)))
                })
                .collect();
            let sets: Vec<(u32, ItemSetView)> = self
                .sets
                .iter()
                .filter_map(|(&id, last)| {
                    let view = item_set_view(id, &tables)?;
                    (view != *last).then_some((id, view))
                })
                .collect();
            (templates, sets)
        };
        for (id, view) in templates {
            self.pending.remove(&id);
            script.set_item_template(id, view);
        }
        // An entry the server answered with no record never resolves; drop it rather than
        // retrying every tick (`ui_loot`'s failed-query deviation).
        self.pending.retain(|&id| !net::ui_item_unknown(id));
        for (id, view) in sets {
            let settled = view.members.iter().all(|(_, n)| n.is_some());
            if settled {
                self.sets.remove(&id);
            } else {
                self.sets.insert(id, view.clone());
            }
            script.set_item_set(id, view);
        }
    }

    /// The state the builder's red lines compare against (`0x5eaae0`, `0x5ea930`): the player's
    /// level, class, race, skill values and honor rank. Deviation: the ffi session keeps no
    /// proficiency or reputation snapshot, so those cells never red; a class the catalog has no
    /// row for never reds either, which is the reference's own answer when the tables are empty.
    fn feed_player_req(&mut self, script: &mut UiScript, cats: &Catalogs, own: &ObjectFields) {
        let mut skills = HashMap::new();
        for slot in 0..PLAYER_SKILL_SLOTS {
            let Some(s) = own.player_skill(slot) else {
                continue;
            };
            if s.skill_id != 0 {
                skills.insert(
                    u32::from(s.skill_id),
                    u32::from(s.value).saturating_add_signed(i32::from(s.perm_bonus)),
                );
            }
        }
        // The reference's `0xc4d770`: a known spell whose first effect is `SPELL_EFFECT_DUAL_WIELD`.
        let can_dual_wield = cats.spells.as_ref().is_some_and(|c| {
            net::ui_spells().iter().any(|&id| {
                c.get(id)
                    .and_then(|sd| sd.effects.first())
                    .is_some_and(|&e| e == 40)
            })
        });
        let state = PlayerReqState {
            level: own.unit_level().unwrap_or(0),
            class_id: u32::from(own.unit_class().unwrap_or(0)),
            race_id: u32::from(own.unit_race().unwrap_or(0)),
            skills,
            proficiency: HashMap::new(),
            rep_ranks: HashMap::new(),
            can_dual_wield,
            honor_rank: own.player_honor_rank().unwrap_or(0),
        };
        if self.req.as_ref() != Some(&state) {
            self.req = Some(state.clone());
            script.set_player_req_state(state);
        }
    }
}

/// The active player's skill in a spell's `SkillLineAbility` line, bonuses included: the `$`
/// engine's per-level terms scale by it (`0x5ea520` behind `0x6de040`). 0 without a line or a
/// slot, which is the reference's answer for a spell outside every skill.
fn spell_skill_value(cats: &Catalogs, own: &ObjectFields, spell_id: u32) -> u32 {
    let Some(line) = cats.skills.as_ref().and_then(|c| c.spell_to_line(spell_id)) else {
        return 0;
    };
    for slot in 0..PLAYER_SKILL_SLOTS {
        let Some(s) = own.player_skill(slot) else {
            continue;
        };
        if u32::from(s.skill_id) == line {
            return u32::from(s.value).saturating_add_signed(i32::from(s.perm_bonus));
        }
    }
    0
}

/// `SStrPrintf` (`0x64a7f0`) as the VM's `GetText` formats it: the same fill the app's token
/// context passes.
fn token_printf(template: &str, args: &[benilla_formats::TokenNumber]) -> String {
    let args: Vec<_> = args
        .iter()
        .map(|n| match n {
            benilla_formats::TokenNumber::Int(v) => Arg::D(*v),
            benilla_formats::TokenNumber::Float(v) => Arg::F(*v),
        })
        .collect();
    benilla_ui::strings::fill(template, &args)
}

/// The catalogs and player reads a tooltip view resolves its prose from.
struct TooltipTables<'a> {
    cats: &'a Catalogs,
    /// The VM, for the `GlobalStrings.lua` lookups its own tooltip would make.
    script: &'a UiScript,
    /// The active player's skill in a spell's line, the `$` tokens' level.
    skill: &'a dyn Fn(u32) -> u32,
    /// `UNIT_FIELD_BYTES_0` byte 2, the `$g`/`$G` branch (`0x508214`).
    gender: u8,
}

impl TooltipTables<'_> {
    fn global(&self, key: &str) -> Option<String> {
        benilla_ui::strings::global(self.script.lua(), key)
    }

    fn spell_name(&self, id: u32) -> Option<String> {
        self.cats
            .spells
            .as_ref()
            .and_then(|c| c.get(id))
            .map(|d| d.name.clone())
    }

    /// A spell's `$`-expanded description, the item builder's trigger text (`0x5075f0`); an empty
    /// one yields `None` and the caller drops the whole line, so an undescribed spell such as a
    /// key's "Opening" prints nothing.
    fn spell_text(&self, id: u32) -> Option<String> {
        let spells = self.cats.spells.as_ref()?;
        let durations = self.cats.spell_durations.as_ref()?;
        let radii = self.cats.spell_radii.as_ref()?;
        let d = spells.get(id)?;
        let desc = d.description.as_deref().filter(|t| !t.is_empty())?;
        let global = |key: &str| self.global(key);
        let ctx = benilla_formats::TokenContext {
            durations,
            radii,
            ranges: self.cats.spell_ranges.as_ref(),
            skill: self.skill,
            lookup: &|i| spells.get(i),
            mods: None,
            unmodified_points: false,
            gender: &|| self.gender,
            // Deviation: the ffi session tracks no bind point yet, so a `$z` stays raw, the
            // app's answer before `SMSG_BINDPOINTUPDATE` lands.
            home_area: &|| None,
            global: &global,
            printf: &token_printf,
        };
        let text = benilla_formats::substitute(desc, d, &ctx);
        (!text.is_empty()).then_some(text)
    }
}

/// One template's tooltip view: the wire fields plus the strings only the client resolves
/// (type pair, subclass spelling, trigger text, requirement names). Mirrors `benilla-app`'s
/// `ui_items::feed::template_view`.
fn template_view(t: &ItemInfo, tables: &TooltipTables) -> ItemTemplateView {
    let cats = tables.cats;
    let sub = cats.item_sub_classes.as_ref();
    ItemTemplateView {
        name: t.name.clone(),
        quality: t.quality,
        class: t.class,
        subclass: t.subclass,
        inventory_type: t.inventory_type,
        proficiency_alt: sub.and_then(|c| c.proficiency_alt(t.class, t.subclass)),
        proficiency_prereq: sub.and_then(|c| c.proficiency_prerequisite(t.class, t.subclass)),
        item_level: t.item_level,
        spell_slots: t
            .spells
            .iter()
            .filter_map(|s| tables.spell_name(s.spell_id).map(|n| (n, s.charges)))
            .collect(),
        hide_subclass: sub.is_some_and(|c| c.hides_name(t.class, t.subclass)),
        item_type: cats
            .item_classes
            .as_ref()
            .and_then(|c| c.name(t.class))
            .map(str::to_string),
        item_sub_type: sub
            .and_then(|c| c.name(t.class, t.subclass))
            .map(str::to_string),
        sub_class_display: sub
            .and_then(|c| c.display_name(t.class, t.subclass))
            .map(str::to_string),
        flags: t.flags,
        bonding: t.bonding,
        max_count: t.max_count,
        stackable: t.stackable,
        icon: cats
            .items
            .as_ref()
            .and_then(|i| i.get(t.display_info_id))
            .and_then(|d| d.icon.clone()),
        start_quest: t.start_quest,
        container_slots: t.container_slots,
        stats: t.stats.clone(),
        damages: t.damages.iter().map(|d| (d.min, d.max, d.school)).collect(),
        delay_ms: t.delay_ms,
        armor: t.armor,
        block: t.block,
        resistances: t.resistances,
        max_durability: t.max_durability,
        required_level: t.required_level,
        allowable_class: t.allowable_class,
        allowable_race: t.allowable_race,
        required_skill: t.required_skill,
        required_skill_rank: t.required_skill_rank,
        required_skill_name: (t.required_skill != 0)
            .then(|| {
                cats.skills
                    .as_ref()
                    .and_then(|sl| sl.line(t.required_skill))
                    .map(|l| l.name.clone())
            })
            .flatten(),
        required_spell: t.required_spell,
        required_spell_name: (t.required_spell != 0)
            .then(|| tables.spell_name(t.required_spell))
            .flatten(),
        required_honor_rank: t.required_honor_rank,
        required_city_rank: t.required_city_rank,
        required_rep_line: (t.required_rep_faction != 0)
            .then(|| {
                let faction = cats
                    .factions
                    .as_ref()
                    .and_then(|c| c.faction_name(t.required_rep_faction))?;
                let standing = tables.global(&format!(
                    "FACTION_STANDING_LABEL{}",
                    t.required_rep_rank.min(7) + 1
                ))?;
                let template = tables.global("ITEM_REQ_REPUTATION")?;
                Some(benilla_ui::strings::fill(
                    &template,
                    &[Arg::S(faction), Arg::S(&standing)],
                ))
            })
            .flatten(),
        required_rep_faction: t.required_rep_faction,
        required_rep_rank: t.required_rep_rank,
        lock_id: t.lock_id,
        spell_triggers: t
            .spells
            .iter()
            .filter(|s| s.spell_id != 0)
            .filter_map(|s| {
                tables
                    .spell_text(s.spell_id)
                    .map(|n| (s.trigger, s.spell_id, n))
            })
            .collect(),
        charges: charges_count(&t.spells),
        description: t.description.clone(),
        page_text: t.page_text,
        sell_price: t.sell_price,
        item_set: t.item_set,
        random_property: t.random_property,
    }
}

/// The first spell block carrying finite charges, as the builder prints "N Charges" (`0x52db51`).
fn charges_count(spells: &[ItemSpellEntry]) -> i32 {
    spells
        .iter()
        .find(|s| s.spell_id != 0 && s.charges != 0 && s.charges != -1)
        .map(|s| s.charges.abs())
        .unwrap_or(0)
}

/// One `ItemSet.dbc` row as the tooltip's set block reads it: members by name from the template
/// cache, each bonus's `$`-expanded text, and the set's own skill requirement.
fn item_set_view(id: u32, tables: &TooltipTables) -> Option<ItemSetView> {
    let row = tables.cats.item_sets.as_ref()?.set(id)?;
    Some(ItemSetView {
        name: row.name.clone(),
        members: row
            .items
            .iter()
            .map(|&item| (item, net::ui_item_template(item).map(|t| t.name)))
            .collect(),
        bonuses: row
            .bonuses
            .iter()
            .filter_map(|&(n, spell)| tables.spell_text(spell).map(|text| (n, text)))
            .collect(),
        required_skill: row.required_skill,
        required_skill_rank: row.required_skill_rank,
        required_skill_name: (row.required_skill != 0)
            .then(|| {
                tables
                    .cats
                    .skills
                    .as_ref()
                    .and_then(|sl| sl.line(row.required_skill))
                    .map(|l| l.name.clone())
            })
            .flatten(),
    })
}

//! The talent window's pages: what `GetTalentTabInfo`/`GetTalentInfo`/`GetTalentPrereqs` read,
//! and what a `LearnTalent(tab, index)` click becomes on the wire.
//!
//! The pages are built exactly as `benilla-app`'s `ui_talent` builds them
//! (`crates/benilla-app/src/ui_talent.rs`, `build_pages`): the class's tabs from `TalentTab.dbc`
//! and `Talent.dbc`, each talent's rank as its highest known rank spell (learning grants every
//! lower rank, vmangos `Player::LearnTalent`), the tab's spent sum gating its rows in fives, and
//! the red requirement lines from the VM's own `GlobalStrings.lua`. A click goes out as
//! `CMSG_LEARN_TALENT` for the 0-based next rank; not-at-max is the client's only gate
//! (`LearnTalent` `0x4f36a0`), the points and prerequisites are the server's to enforce.

use std::collections::BTreeSet;

use benilla_formats::{SpellCatalog, Talent, TalentCatalog};
use benilla_protocol::messages::ObjectFields;
use benilla_ui::script::{
    ScriptValue, TalentPrereqView, TalentTabView, TalentUiState, TalentView, UiScript,
};
use benilla_ui::strings::{fill, Arg};

use crate::net::{self, Cmd};
use crate::ui_feed::{self, Catalogs};

/// The inputs the last build read: race, class, the points pair and the known spells, sorted.
type FeedKey = (u8, u8, (u32, u32), Vec<u32>);

/// One load's talent state: the inputs the last build read, the snapshot it pushed and the
/// points pair `CHARACTER_POINTS_CHANGED`'s deltas come from. Reset with the VM
/// (`ui_feed::attach` builds a new `Feed`).
#[derive(Default)]
pub(crate) struct TalentVm {
    key: Option<FeedKey>,
    pushed: TalentUiState,
    /// `None` until the first read, which gives zero deltas.
    points: Option<(u32, u32)>,
}

impl TalentVm {
    /// Push the class's pages when the inputs changed. Called from the UI tick, where the VM
    /// lives and the `GlobalStrings.lua` lookups can run.
    pub(crate) fn feed(&mut self, script: &mut UiScript, cats: &Catalogs, own: &ObjectFields) {
        let (Some(catalog), Some(spells)) = (cats.talents.as_ref(), cats.spells.as_ref()) else {
            return;
        };
        let race = own.unit_race().unwrap_or(0);
        let class = own.unit_class().unwrap_or(0);
        let points = (
            own.player_talent_points().unwrap_or(0),
            own.player_free_professions().unwrap_or(0),
        );
        let mut known_spells = net::ui_spells();
        known_spells.sort_unstable();
        known_spells.dedup();
        let key = (race, class, points, known_spells);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let known: BTreeSet<u32> = key.3.iter().copied().collect();
        // Scoped: the `GlobalStrings.lua` lookups borrow the VM, which the push needs mutably.
        let fresh = {
            let get = |k: &str| benilla_ui::strings::global(script.lua(), k);
            build_pages(catalog, &known, spells, race, class, points, &get)
        };
        self.key = Some(key);
        if fresh != self.pushed {
            script.set_talents(fresh.clone());
            self.pushed = fresh;
            // The deltas the reference's `SignalEvent2` sends; `ChatFrame.lua:1326` tests arg2
            // unguarded, so an argless fire raises. `self.points` still holds the previous pair.
            let (talent_delta, profession_delta) = match self.points {
                Some((old_talent, old_professions)) => (
                    i64::from(points.0) - i64::from(old_talent),
                    i64::from(points.1) - i64::from(old_professions),
                ),
                None => (0, 0),
            };
            script.fire_event(
                "CHARACTER_POINTS_CHANGED",
                vec![
                    ScriptValue::Int(talent_delta),
                    ScriptValue::Int(profession_delta),
                ],
            );
        }
        self.points = Some(points);
    }
}

/// Drain the queued `LearnTalent(tab, index)` clicks, both 1-based, into `CMSG_LEARN_TALENT`.
pub(crate) fn drain(script: &mut UiScript) {
    let clicks = script.take_talent_learns();
    if clicks.is_empty() {
        return;
    }
    let Some(own) = net::ui_self_fields() else {
        return;
    };
    let race = own.unit_race().unwrap_or(0);
    let class = own.unit_class().unwrap_or(0);
    ui_feed::with_catalogs(|cats| {
        let Some(catalog) = cats.talents.as_ref() else {
            return;
        };
        let known: BTreeSet<u32> = net::ui_spells().into_iter().collect();
        let tabs = catalog.tabs_for_class(race, class);
        for (tab, index) in clicks {
            // 1-based Lua-facing pair; a 0 from a stray script is a miss, never an underflow.
            let Some(t) = (tab as usize)
                .checked_sub(1)
                .and_then(|i| tabs.get(i))
                .and_then(|tab| {
                    (index as usize)
                        .checked_sub(1)
                        .and_then(|i| catalog.talents_in_tab(tab.id).get(i))
                })
            else {
                continue;
            };
            let rank = rank_of(t, &known);
            // At max is the client's only send gate (`LearnTalent` `0x4f36a0`).
            if rank >= t.max_rank() {
                continue;
            }
            let _ = net::send_cmd(Cmd::LearnTalent {
                talent_id: t.id,
                rank,
            });
        }
    });
}

/// A talent's rank: the highest rank whose spell is known.
fn rank_of(t: &Talent, known: &BTreeSet<u32>) -> u32 {
    t.ranks
        .iter()
        .enumerate()
        .filter(|(_, &s)| s != 0 && known.contains(&s))
        .map(|(i, _)| i as u32 + 1)
        .max()
        .unwrap_or(0)
}

/// Builds the pushed snapshot (the app's `build_pages`, one-for-one).
fn build_pages(
    catalog: &TalentCatalog,
    known: &BTreeSet<u32>,
    spells: &SpellCatalog,
    race: u8,
    class: u8,
    points: (u32, u32),
    // The VM's `GlobalStrings.lua`, for the red requirement lines.
    get: &dyn Fn(&str) -> Option<String>,
) -> TalentUiState {
    let mut tabs = Vec::new();
    let mut pages = Vec::new();
    for tab in catalog.tabs_for_class(race, class) {
        let list = catalog.talents_in_tab(tab.id);
        let spent: u32 = list.iter().map(|t| rank_of(t, known)).sum();
        let mut views = Vec::with_capacity(list.len());
        for t in list {
            let rank = rank_of(t, known);
            let max_rank = t.max_rank();
            let display_spell = t.ranks[rank.max(1) as usize - 1];
            let next_spell = if rank > 0 && rank < max_rank {
                t.ranks[rank as usize]
            } else {
                0
            };
            let d = spells.get(display_spell);
            // `meetsPrereq` is the `requiredSpell` check alone (`GetTalentInfo` `0x4f3200`).
            let meets_prereq = t.required_spell == 0 || known.contains(&t.required_spell);
            // The tier gate reads the tab's own spent sum; prereqs read the prereq's rank.
            let tier_unlocked = t.row * 5 <= spent;
            let mut req_lines = Vec::new();
            if !tier_unlocked {
                req_lines.extend(
                    get("TOOLTIP_TALENT_TIER_POINTS")
                        .map(|f| fill(&f, &[Arg::D(i64::from(t.row * 5)), Arg::S(&tab.name)])),
                );
            }
            // The item tooltip's `ITEM_REQ_SKILL`, the key `0x52b390` emits for a missing one.
            if !meets_prereq {
                if let Some(req) = spells.get(t.required_spell) {
                    req_lines.extend(get("ITEM_REQ_SKILL").map(|f| fill(&f, &[Arg::S(&req.name)])));
                }
            }
            let mut prereqs = Vec::new();
            if t.prereq_talent != 0 {
                if let Some(p) = catalog.talent(t.tab, t.prereq_talent) {
                    let need = t.prereq_rank + 1;
                    let learnable = rank_of(p, known) >= need;
                    prereqs.push(TalentPrereqView {
                        tier: p.row + 1,
                        column: p.col + 1,
                        learnable,
                    });
                    if !learnable {
                        let p_name = spells
                            .get(p.ranks[0])
                            .map(|d| d.name.clone())
                            .unwrap_or_default();
                        req_lines.extend(
                            benilla_ui::strings::plural("TOOLTIP_TALENT_PREREQ", Some(need), get)
                                .map(|f| fill(&f, &[Arg::D(i64::from(need)), Arg::S(&p_name)])),
                        );
                    }
                }
            }
            // The green hint is points left and not maxed alone (`SetTalent` `0x535170`).
            let learnable = rank < max_rank && points.0 > 0;
            views.push(TalentView {
                name: d.map(|d| d.name.clone()).unwrap_or_default(),
                texture: d.and_then(|d| d.icon.clone()),
                tier: t.row + 1,
                column: t.col + 1,
                rank,
                max_rank,
                exceptional: t.exceptional,
                meets_prereq,
                prereqs,
                display_spell,
                next_spell,
                req_lines,
                learnable,
            });
        }
        tabs.push(TalentTabView {
            name: tab.name.clone(),
            background: tab.background.clone(),
            points_spent: spent,
            texture: tab.icon.clone(),
        });
        pages.push(views);
    }
    TalentUiState {
        tabs,
        talents: pages,
        points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use benilla_formats::MAX_TALENT_RANK;

    fn talent(id: u32, tab: u32, row: u32, col: u32, ranks: &[u32]) -> Talent {
        let mut r = [0u32; MAX_TALENT_RANK];
        r[..ranks.len()].copy_from_slice(ranks);
        Talent {
            id,
            tab,
            row,
            col,
            ranks: r,
            prereq_talent: 0,
            prereq_rank: 0,
            required_spell: 0,
            exceptional: false,
        }
    }

    #[test]
    fn rank_is_the_highest_known_rank_spell() {
        let t = talent(1, 81, 0, 0, &[100, 101, 102]);
        let known: BTreeSet<u32> = [100, 101].into_iter().collect();
        assert_eq!(rank_of(&t, &known), 2);
        assert_eq!(rank_of(&t, &BTreeSet::new()), 0);
        let holey: BTreeSet<u32> = [102].into_iter().collect();
        assert_eq!(rank_of(&t, &holey), 3);
    }

    /// The pages come from the install's own tables: human (`ChrRaces.dbc` 1) shaman
    /// (`ChrClasses.dbc` 7) has its three tabs with their `SpellIcon` faces.
    #[test]
    fn a_shamans_pages_carry_three_tabs() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let talents = benilla_formats::load_talent_catalog(&mut chain).expect("talent catalog");
        let spells = benilla_formats::load_spell_catalog(&mut chain).expect("spell catalog");
        let get = |_: &str| None;
        let state = build_pages(&talents, &BTreeSet::new(), &spells, 1, 7, (0, 0), &get);
        assert_eq!(state.tabs.len(), 3);
        assert_eq!(
            state.tabs[0].texture.as_deref(),
            Some("Interface\\Icons\\Spell_Fire_Volcano")
        );
        assert!(!state.talents[0].is_empty());
    }
}

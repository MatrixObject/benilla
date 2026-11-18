//! The NPC's select greeting (`0x60c270`), fired on the middle layer's selection edge.
//!
//! A unit's `CreatureDisplayInfo.NPCSoundID` names an `NPCSounds` row — hello, goodbye, pissed —
//! each a `SoundEntries` kit. A select plays the hello with a free-picked take five times, then the
//! pissed kit's takes in order, then a hello that restarts the cycle (`0x623910`). The counter
//! `[0xc4d970]` belongs to the tracked NPC `[0xc4d908]` and resets when that changes.
//!
//! The gates run in the reference's order: the unit's channel latch (`0x60c28c`), a dead unit
//! (`0x60c2a5`), the reaction pair at least neutral in each direction (`0x60c2d9`, `0x60c2ef`), the
//! local player neither dead nor released (`0x60c31d`), then the re-track (`0x60c326`) — a unit
//! that clears them with no greeting row is tracked and says nothing.
//!
//! The split follows the data: this side answers the gates and the listener's distance, and the
//! audio thread ([`crate::audio`]) owns the counter, the take and the latch, because `0x623910`
//! needs the pissed kit's variation count and only that thread reads `SoundEntries`.
//!
//! Deviations, all of them the lane's rather than the mechanism's:
//!
//! - the greeting fires where the selection *changes* ([`crate::ui_feed`]'s edge, the server's
//!   mirrored descriptor) and not inside the click router (`0x4925d0`), so a `/target` or a Tab
//!   select greets as a left-click does;
//! - the reaction pair is the template comparator alone (`0x606640`): `0x606530`'s reputation leg
//!   has no table in the middle layer, so an inline-faction NPC greets at its template's standing;
//! - the active mover (`0x6006e0`) is the local player — no possess or Eye of Kilrogg lane here;
//! - the line takes the reference's distance attenuation (`0x45cdf0`'s cutoff, the 4.0 rolloff
//!   factor of `0x7a47b1`, the near-field ramp of `0x7a5000`) and no azimuth or EAX: the lane mixes
//!   with no listener transform, and every NPC voice kit is dry (`eax_def` 0) anyway.
//!
//! Not ported: the interaction window's hello and goodbye (`0x60c3b0`), which the reference rides
//! on the gossip latch rather than the selection, and with it the `0x60c3d1` that zeroes the
//! tracked NPC when a window opens.

use benilla_formats::{FactionCatalog, Reaction};
use benilla_protocol::messages::ObjectFields;

use crate::audio;
use crate::net;
use crate::selection_ring::template_of;
use crate::ui_feed::Catalogs;

/// `ObjectType::Unit`: a player, a game object and a book speak nothing (`0x493159`, `0x493182`).
const KIND_UNIT: u8 = 3;

/// Hello takes before the cycle steps into the pissed kit (`0x62394a: cmp ebx,5`).
pub(crate) const HELLO_TAKES: usize = 5;

/// The line one select produces (`0x623910`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GreetLine {
    /// The hello kit, its take drawn from the kit's weighted pool.
    Hello,
    /// The pissed kit's take `n`, in row order.
    Pissed(usize),
}

/// `0x623910`: [`HELLO_TAKES`] hellos, then the pissed kit's takes in order, then a hello that
/// restarts the counter. Returns the line and the next counter.
pub(crate) fn greet_line(seq: usize, pissed_variations: usize) -> (GreetLine, usize) {
    if seq < HELLO_TAKES {
        (GreetLine::Hello, seq + 1)
    } else if seq - HELLO_TAKES < pissed_variations {
        (GreetLine::Pissed(seq - HELLO_TAKES), seq + 1)
    } else {
        // Past the pissed pool: hello, variation -1, counter reset.
        (GreetLine::Hello, 0)
    }
}

/// `0x606640` between two units, Neutral where either side has no template row — the way the
/// selector's rank defaults.
fn rank(factions: Option<&FactionCatalog>, a: &ObjectFields, b: &ObjectFields) -> Reaction {
    template_of(factions, a)
        .zip(template_of(factions, b))
        .map_or(Reaction::Neutral, |(ta, tb)| ta.reaction_toward(&tb))
}

/// The greeting's reaction pair (`0x60c2d9`, `0x60c2ef`): at least neutral in each direction.
fn both_at_least_neutral(
    factions: Option<&FactionCatalog>,
    unit: &ObjectFields,
    own: &ObjectFields,
) -> bool {
    rank(factions, unit, own) >= Reaction::Neutral && rank(factions, own, unit) >= Reaction::Neutral
}

/// `0x45cdf0`'s selection-time audibility: strictly inside the kit's `DistanceCutoff`. A cutoff of
/// 0 is the non-positional sentinel, which the gate skips.
pub(crate) fn audible(d_sq: f32, cutoff: f32) -> bool {
    cutoff <= 0.0 || cutoff * cutoff > d_sq
}

/// FMOD's global 3D rolloff factor, `0x7a47b1` at device init — the `SoundRolloffFactor` default.
const ROLLOFF_FACTOR: f32 = 4.0;

/// The audible gain (`0x458ed0`'s min/max distance through FMOD's inverse rolloff, times the
/// near-field ramp `0x7a5000` across the cutoff's last tenth): full inside `MinDistance`, then
/// `min / (min + 4·(d − min))`, and a linear fade to silence over the final tenth of the cutoff.
pub(crate) fn audible_gain(d_sq: f32, min_dist: f32, cutoff: f32) -> f32 {
    let d = d_sq.sqrt();
    let rolloff = if min_dist <= 0.0 || d <= min_dist {
        1.0
    } else {
        min_dist / (min_dist + ROLLOFF_FACTOR * (d - min_dist))
    };
    let band = cutoff * 0.1;
    let near = if band <= 0.0 {
        1.0
    } else {
        1.0 - (d - cutoff * 0.9).clamp(0.0, band) / band
    };
    rolloff * near
}

/// The select greeting for the unit the selection just moved to, in the reference's gate order.
/// The re-track and the line ride the request: [`crate::audio`] answers them.
pub(crate) fn select(cats: Option<&Catalogs>, guid: u64) {
    let Some(unit) = net::ui_unit(guid) else {
        return;
    };
    if unit.kind != KIND_UNIT {
        return;
    }
    // `0x60c28c`: a unit whose channel is still sounding refuses a new line, and the counter does
    // not move.
    if audio::vocal_busy(guid) {
        return;
    }
    // `0x60c2a5`: a dead unit speaks nothing.
    if unit.fields.unit_reads_dead() {
        return;
    }
    let (Some(own), Some(me)) = (net::ui_self_fields(), net::ui_player()) else {
        return;
    };
    // `0x60c2d9` and `0x60c2ef`: the pair is between the unit and the active mover, which this
    // lane only ever reads as the local player.
    let factions = cats.and_then(|c| c.factions.as_ref());
    if !both_at_least_neutral(factions, &unit.fields, &own) {
        return;
    }
    // `0x60c31d` over `0x605f30`: a corpse or a spirit hears nothing.
    if me.dead != 0 || me.ghost != 0 {
        return;
    }
    // `0x60c326` tracks the unit whatever its display answers; the row is what the audio thread
    // needs to pick a line, so the track travels with it.
    let display = unit.fields.unit_displayid().unwrap_or(-1).max(0) as u32;
    let row = cats
        .and_then(|c| c.greetings.as_ref())
        .and_then(|g| g.for_display(display))
        .copied();
    let d_sq = {
        let (a, b) = (me.pos, unit.pos);
        let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
        dx * dx + dy * dy + dz * dz
    };
    audio::greet_npc(guid, row, d_sq);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `0x623910`: five free-picked hellos, then the pissed takes in row order, then a hello that
    /// puts the counter back to the start of the cycle.
    #[test]
    fn the_cycle_is_five_hellos_then_the_pissed_takes_in_order() {
        let mut seq = 0;
        let mut lines = Vec::new();
        for _ in 0..9 {
            let (line, next) = greet_line(seq, 3);
            lines.push(line);
            seq = next;
        }
        assert_eq!(
            lines,
            vec![
                GreetLine::Hello,
                GreetLine::Hello,
                GreetLine::Hello,
                GreetLine::Hello,
                GreetLine::Hello,
                GreetLine::Pissed(0),
                GreetLine::Pissed(1),
                GreetLine::Pissed(2),
                GreetLine::Hello,
            ]
        );
        // The wrap reset the ninth line came with.
        assert_eq!(seq, 0);
    }

    /// A unit with no pissed takes never leaves the hello arm, and the counter still wraps.
    #[test]
    fn no_pissed_takes_wraps_straight_back_to_hello() {
        assert_eq!(greet_line(5, 0), (GreetLine::Hello, 0));
        assert_eq!(greet_line(4, 0), (GreetLine::Hello, 5));
    }

    /// The cutoff gate is squared and strict; a non-positional kit (0) always passes.
    #[test]
    fn the_cutoff_gate_is_squared_and_strict() {
        assert!(audible(0.0, 41.0));
        assert!(audible(41.0 * 41.0 - 1.0, 41.0));
        assert!(!audible(41.0 * 41.0, 41.0));
        assert!(audible(1.0e6, 0.0));
    }

    /// FMOD's inverse rolloff with the factor 4, then the near-field ramp over the last tenth.
    #[test]
    fn the_audible_gain_rolls_off_from_min_distance_and_fades_at_the_edge() {
        assert_eq!(audible_gain(4.0 * 4.0, 8.0, 0.0), 1.0); // inside min: full
        assert!((audible_gain(16.0 * 16.0, 8.0, 0.0) - 0.2).abs() < 1e-6); // 8/(8+4·8)
        assert_eq!(audible_gain(0.0, 0.0, 0.0), 1.0); // no min, no cutoff
        assert!((audible_gain(36.0 * 36.0, 0.0, 40.0) - 1.0).abs() < 1e-6); // inside 90%
        assert!((audible_gain(38.0 * 38.0, 0.0, 40.0) - 0.5).abs() < 1e-6); // mid-ramp
        assert!(audible_gain(40.0 * 40.0, 0.0, 40.0) < 1e-6); // at the edge: silence
    }

    /// The install's own answer for the Northshire cart vendor (display 11898, the unit the live
    /// probe selects): a greeting row, a hello kit with files to play, and a gain the probe's 4.7
    /// yards still hears.
    #[test]
    fn a_real_npc_display_resolves_to_a_playable_hello_kit() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let greetings =
            benilla_formats::load_npc_greeting_catalog(&mut chain).expect("NPCSounds.dbc");
        let kits = benilla_formats::load_sound_kit_catalog(&mut chain).expect("SoundEntries.dbc");
        let row = greetings.for_display(11898).expect("display 11898 greets");
        assert_ne!(row.hello, 0, "the cart vendor has a hello");
        let kit = kits.get(row.hello).expect("the hello kit is a row");
        assert!(!kit.files.is_empty(), "kit {} has files", row.hello);
        let d_sq = 4.7 * 4.7;
        assert!(
            audible(d_sq, kit.distance_cutoff),
            "cutoff {}",
            kit.distance_cutoff
        );
        let gain = audible_gain(d_sq, kit.min_distance, kit.distance_cutoff);
        assert!(
            gain > 0.2,
            "gain {gain} at min {} cutoff {}",
            kit.min_distance,
            kit.distance_cutoff
        );
    }

    /// A display with no `NPCSoundID` answers no row, so the select tracks the unit and says
    /// nothing — the `0x623910` arm that answers -1.
    #[test]
    fn a_display_without_a_sound_row_greets_with_nothing() {
        let data = benilla_formats::wow_data_or_skip!();
        let mut chain = benilla_formats::open_chain(&data).expect("open chain");
        let greetings =
            benilla_formats::load_npc_greeting_catalog(&mut chain).expect("NPCSounds.dbc");
        assert!(greetings.for_display(0).is_none(), "no display, no voice");
    }
}

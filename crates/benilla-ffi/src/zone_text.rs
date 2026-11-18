//! The zone texts and the `ZONE_CHANGED` family: the FFI port of the updater `0x494780`
//! (`crates/benilla-app/src/area.rs`), whose caches the stock `ZoneText.xml` splash reads and whose
//! event puts the zone's name on the screen.
//!
//! - The leaf area is the MCNK id under the body, asked only when the body crosses a chunk; the
//!   zone is that row's single-hop parent (`AreaTable` field 2, itself when 0).
//! - A zone id change fires `ZONE_CHANGED_NEW_AREA` alone; else a text change fires `ZONE_CHANGED`.
//!   The first world-enter is a zone id change from the empty cache, which is why the zone name
//!   splashes on login.
//! - `GetMinimapZoneText` is subzone-else-zone (`0xb4da28`) with its own `MINIMAP_ZONE_CHANGED`
//!   (`0x494970`), elected independently of the zone event.
//!
//! Two gaps, both from machinery this lane does not carry:
//! - The indoor arm (`0x67e670`: the whole-WMO row overriding the zone slot, the hit group's own row
//!   refilling the subzone, the dedup on `(WMOID, group)` and `ZONE_CHANGED_INDOORS`): no export
//!   answers the WMO group under the body, so an interior keeps the outdoor zone's name.
//! - `GetZonePVPInfo`'s type and faction, which the reference gates on the realm's PvP flag; the
//!   login path holds no realm flags, so both stay nil and the splash keeps its plain colour. The
//!   arena test is the leaf row's own `0x80` and needs no realm, so it is kept.
//! - The reference's poll is an engine update tick, which only runs once the world is on screen;
//!   this lane resolves on the first UI tick after `bn_ui_load`, so the login splash's 3.5 s runs
//!   while the host's loading overlay is still up. An area change mid-session is on screen and
//!   reads the same.

use std::cell::RefCell;

use benilla_ui::script::{UiScript, ZoneTexts};

use crate::net;

/// `0x494780`'s compare targets, the minimap line's cache (`0x494970`) and the chunk the leaf area
/// was read under. It lives per VM, so every login starts empty and its first resolve is a NEW_AREA.
#[derive(Default)]
struct ZoneCache {
    zone_id: Option<u32>,
    zone_text: String,
    subzone_text: String,
    minimap_text: String,
    chunk: Option<(u32, u32)>,
    leaf: Option<u32>,
}

thread_local! {
    static CACHE: RefCell<ZoneCache> = RefCell::new(ZoneCache::default());
}

/// Drop the cache with the VM: every world enter is a first enter, and its zone id change splashes.
pub(crate) fn clear() {
    CACHE.with(|c| *c.borrow_mut() = ZoneCache::default());
}

/// One resolve's compare inputs.
struct Signal {
    zone_id: u32,
    zone_text: String,
    subzone_text: String,
    is_arena: bool,
}

/// The leaf row's zone and names, `None` when the id has no row — the resolver keeps the last state
/// for areaId 0 rather than naming nothing.
fn resolve(leaf: u32) -> Option<Signal> {
    crate::world_map::with_areas(|areas| {
        let row = areas.get(leaf)?;
        let zone = if row.zone_id == 0 { leaf } else { row.zone_id };
        Some(Signal {
            zone_id: zone,
            zone_text: areas.name(zone).unwrap_or_default().to_string(),
            subzone_text: if leaf == zone {
                String::new()
            } else {
                row.name.clone()
            },
            is_arena: row.flags & 0x80 != 0,
        })
    })?
}

/// The event election of `0x494780`; the indoor arm is the gap above, so a text change is always
/// the outdoor `ZONE_CHANGED`.
fn elect(cache: &ZoneCache, next: &Signal) -> Option<&'static str> {
    if cache.zone_id != Some(next.zone_id) {
        Some("ZONE_CHANGED_NEW_AREA")
    } else if cache.zone_text != next.zone_text || cache.subzone_text != next.subzone_text {
        Some("ZONE_CHANGED")
    } else {
        None
    }
}

/// Push the caches, then fire what changed: host globals are written before the event, so a
/// handler's `GetZoneText()` already reads the new state.
pub(crate) fn sync(script: &mut UiScript) {
    let Some(player) = net::ui_player() else {
        return;
    };
    let chunk = benilla_formats::world_to_chunk(player.pos[0], player.pos[1]);
    CACHE.with(|slot| {
        let mut cache = slot.borrow_mut();
        // The MCNK walk runs on a chunk edge, and only once the tile is actually streamed.
        if cache.chunk != Some(chunk) {
            if let Some(area) = crate::audio::player_area() {
                cache.chunk = Some(chunk);
                cache.leaf = Some(area);
            }
        }
        let Some(leaf) = cache.leaf else { return };
        let Some(next) = resolve(leaf) else { return };
        let minimap_text = if next.subzone_text.is_empty() {
            next.zone_text.clone()
        } else {
            next.subzone_text.clone()
        };
        let event = elect(&cache, &next);
        let minimap_changed = cache.minimap_text != minimap_text;
        if event.is_none() && !minimap_changed {
            return;
        }
        script.set_zone_texts(ZoneTexts {
            zone: next.zone_text.clone(),
            real_zone: next.zone_text.clone(),
            subzone: next.subzone_text.clone(),
            minimap: minimap_text.clone(),
            pvp_type: None,
            pvp_faction: None,
            is_arena: next.is_arena,
        });
        if let Some(event) = event {
            script.fire_event(event, vec![]);
            eprintln!(
                "zone: {event} → zone {:?} / sub {:?}",
                next.zone_text, next.subzone_text
            );
        }
        if minimap_changed {
            script.fire_event("MINIMAP_ZONE_CHANGED", vec![]);
        }
        cache.zone_id = Some(next.zone_id);
        cache.zone_text = next.zone_text;
        cache.subzone_text = next.subzone_text;
        cache.minimap_text = minimap_text;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(zone_id: Option<u32>, zone: &str, sub: &str) -> ZoneCache {
        ZoneCache {
            zone_id,
            zone_text: zone.to_string(),
            subzone_text: sub.to_string(),
            minimap_text: if sub.is_empty() {
                zone.to_string()
            } else {
                sub.to_string()
            },
            chunk: None,
            leaf: None,
        }
    }

    fn signal(zone_id: u32, zone: &str, sub: &str) -> Signal {
        Signal {
            zone_id,
            zone_text: zone.to_string(),
            subzone_text: sub.to_string(),
            is_arena: false,
        }
    }

    /// A zone id change is the splash; a text change inside the zone is the silent re-cache, and
    /// nothing at all fires when both agree with the cache.
    #[test]
    fn only_a_zone_id_change_is_a_new_area() {
        let old = cache(Some(45), "Northshire", "Northshire Valley");
        assert_eq!(
            elect(&old, &signal(139, "Dun Morogh", "")),
            Some("ZONE_CHANGED_NEW_AREA")
        );
        assert_eq!(
            elect(&old, &signal(45, "Northshire", "The Salt Farm")),
            Some("ZONE_CHANGED")
        );
        assert_eq!(
            elect(&old, &signal(45, "Northshire", "Northshire Valley")),
            None
        );
    }

    /// The first resolve of a VM is a zone id change from the empty cache, which is why the zone
    /// name splashes on login.
    #[test]
    fn the_first_resolve_splashes() {
        assert_eq!(
            elect(&ZoneCache::default(), &signal(45, "Northshire", "")),
            Some("ZONE_CHANGED_NEW_AREA")
        );
    }
}

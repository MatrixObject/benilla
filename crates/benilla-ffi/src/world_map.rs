//! The world map (`M`) in the stock `WorldMapFrame`: the static catalog the reference builds at
//! `0x4a5d00`, pushed into the VM before the toc walk, and the per-frame feed behind
//! `GetPlayerMapPosition` and `SetMapToCurrentZone` (`0x4a6650`, `0x4a67a0`, `0x4a7360`).
//!
//! `crates/benilla-app/src/ui_world_map.rs` is the same feed on the standalone client's Bevy
//! resources. The projection below is `benilla-world`'s `map_proj`, copied because this crate
//! links no Bevy; the two must stay identical.

use std::cell::RefCell;
use std::collections::HashMap;

use benilla_formats::{
    load_area_poi_catalog, load_area_table_catalog, load_map_catalog, load_world_map_area_catalog,
    load_world_map_continent_catalog, load_world_map_overlay_catalog, load_zone_map, AreaPoi,
    AreaPoiCatalog, AreaTableCatalog, Chain, MapCatalog, WorldMapArea,
};
use benilla_protocol::messages::PLAYER_EXPLORED_ZONES_SLOTS;
use benilla_ui::script::{
    UiScript, WorldMapContinentView, WorldMapLandmarkSource, WorldMapLevel, WorldMapOverlayView,
    WorldMapZoneView,
};

use crate::net;

thread_local! {
    /// The VM's world-map state: the pushed catalog's projection data, the non-`Clone` DBC
    /// catalogs the feed reads, and the memos that keep the pushes on change edges. `seed` builds
    /// it, once per UI load, before the toc walk.
    static MAP: RefCell<Option<Map>> = const { RefCell::new(None) };
}

struct Map {
    data: Data,
    areas: AreaTableCatalog,
    /// `AreaPOI.dbc`; absent when the chain has no row set, which only drops the icons.
    pois: Option<AreaPoiCatalog>,
    /// The inputs the landmark sources are a pure function of, for the change edge.
    landmarks: Option<LandmarkKey>,
    explored: Option<Vec<u32>>,
    /// The world-enter sync has run: the reference's `old == 0` gate at `0x494780`.
    synced: bool,
    /// `GetWorldLocMapPosition`'s projection is installed over this VM.
    projector: bool,
    /// The chunk the cached `top_zone` was resolved under, so the MCNK walk runs only on a chunk
    /// change rather than every frame.
    area_chunk: Option<(u32, u32)>,
    top_zone: Option<u32>,
}

// ---------------------------------------------------------------------------------------------
// The projection (benilla-world's `map_proj`, kept byte-identical)

/// Yards per ADT tile (reference `0x80654c`).
const TILE_YARDS: f32 = 533.333_3;
/// The world sheet's span in tiles, u (`0x806548`) and v (`0x806550`), the 1002×668 art's aspect.
const WORLD_SPAN_U_TILES: f32 = 62.625;
const WORLD_SPAN_V_TILES: f32 = 41.75;
/// Yards to sheet fraction, `1/(span · tile)`, as the reference precomputes it (`0x806554`,
/// `0x806558`).
const K_U: f32 = 1.0 / (WORLD_SPAN_U_TILES * TILE_YARDS);
const K_V: f32 = 1.0 / (WORLD_SPAN_V_TILES * TILE_YARDS);

/// A continent's world-level projection: WorldMapContinent.dbc fields 6 and 7 (its sheet offset
/// in tiles, u and v) and 8 (the yard scale), read at record +0x18/+0x1c/+0x20 (`0x4a72b0`,
/// `0x4a7360`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WorldProj {
    offset_u: f32,
    offset_v: f32,
    scale: f32,
}

/// A WorldMapArea.dbc row's world rect: `left` is the west edge (max wy), `top` the north edge
/// (max wx), so left > right and top > bottom.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ZoneRect {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

/// World to UV on the world sheet (`0x4a7360` world mode): u from wy, v from wx, unclamped as in
/// the reference.
fn world_uv(proj: WorldProj, wx: f32, wy: f32) -> (f32, f32) {
    let u = (proj.offset_u * (1.0 / WORLD_SPAN_U_TILES) + 0.5) - wy * K_U * proj.scale;
    let v = (proj.offset_v * (1.0 / WORLD_SPAN_V_TILES) + 0.5) - wx * K_V * proj.scale;
    (u, v)
}

/// World to UV inside a zone or continent rect (`0x4a7360` zone mode, quirks included). Each axis
/// is `1 − (in − low)/span`, 0 when the span or the numerator is 0, so the east and south edges
/// yield 0, not 1. If either leaves [0,1] both are zeroed, the `(0,0)` FrameXML reads as "not on
/// this map".
fn zone_uv(rect: ZoneRect, wx: f32, wy: f32) -> (f32, f32) {
    fn axis(input: f32, low: f32, span: f32) -> f32 {
        if input - low != 0.0 && span != 0.0 {
            1.0 - (input - low) / span
        } else {
            0.0
        }
    }
    let u = axis(wy, rect.right, rect.left - rect.right);
    let v = axis(wx, rect.bottom, rect.top - rect.bottom);
    if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
        (u, v)
    } else {
        (0.0, 0.0)
    }
}

/// A continent's `(u0, v0, u1, v1)` on the world sheet from its WorldMapContinent tile bounds
/// (fields 2 to 5), the `0x4a5d00` builder: the world-level click's hit test (`0x4a7100`).
fn continent_sheet_rect(bounds: (u32, u32, u32, u32), proj: WorldProj) -> (f32, f32, f32, f32) {
    let (left, right, top, bottom) = bounds;
    let xoff = (31.3125 - proj.scale * 32.0) + proj.offset_u;
    let yoff = (20.875 - proj.scale * 32.0) + proj.offset_v;
    let u0 = (left as f32 * proj.scale + xoff) * (1.0 / WORLD_SPAN_U_TILES);
    let u1 = ((right + 1) as f32 * proj.scale + xoff) * (1.0 / WORLD_SPAN_U_TILES);
    let v0 = (top as f32 * proj.scale + yoff) * (1.0 / WORLD_SPAN_V_TILES);
    let v1 = ((bottom + 1) as f32 * proj.scale + yoff) * (1.0 / WORLD_SPAN_V_TILES);
    (u0, v0, u1, v1)
}

// ---------------------------------------------------------------------------------------------
// The catalog

/// The app's copy of the pushed catalog's projection data (rects, world-sheet constants), in the
/// engine's order: the indices must agree.
#[derive(Clone, Default)]
struct Data {
    continents: Vec<ContinentEntry>,
    /// The instance maps no continent owns, `0x4a5d00`'s third array in `WorldMapArea.dbc` file
    /// order, unsorted: what the `-2` direct-area selection resolves against.
    direct: Vec<DirectAreaEntry>,
}

#[derive(Clone)]
struct ContinentEntry {
    map_id: u32,
    proj: Option<WorldProj>,
    rect: ZoneRect,
    zones: Vec<ZoneEntry>,
}

#[derive(Clone)]
struct ZoneEntry {
    area_id: u32,
    rect: ZoneRect,
}

/// One instance map, selected directly rather than through a continent and zone; 1.12 ships
/// three, the battlegrounds.
#[derive(Clone)]
struct DirectAreaEntry {
    /// The `WorldMapArea` row id, which the third selection cell `[0x845074]` holds (`0x4a6717`);
    /// never a list index.
    id: u32,
    /// Matched against the player's map by the resolver (`0x4a66f6`); the projection admits only a
    /// body on this map (`0x4a7437`).
    map_id: u32,
    /// The row's loc rect, which `0x4a7360`'s direct-area leg projects through as a zone's.
    rect: ZoneRect,
    /// What the engine is handed for this row (`GetMapInfo`'s art folder, the overlays).
    view: WorldMapZoneView,
}

fn zone_rect(a: &WorldMapArea) -> ZoneRect {
    ZoneRect {
        left: a.loc_left,
        right: a.loc_right,
        top: a.loc_top,
        bottom: a.loc_bottom,
    }
}

/// One `WorldMapArea` row as the engine sees it. Zones, cities and instance maps are alike: one
/// `0x4a6cf0` lookup answers `GetMapInfo` for all three.
fn map_row_view(
    wma_id: u32,
    a: &WorldMapArea,
    name: String,
    areas: &AreaTableCatalog,
    wmo: &benilla_formats::WorldMapOverlayCatalog,
) -> WorldMapZoneView {
    WorldMapZoneView {
        name,
        area_id: a.area_id,
        map_file: a.name.clone(),
        loc_rect: (a.loc_left, a.loc_right, a.loc_top, a.loc_bottom),
        // WorldMapOverlay rows by WMA id, each revealed by its covered areas' `exploreFlag` bits.
        overlays: wmo
            .for_area(wma_id)
            .iter()
            .map(|o| WorldMapOverlayView {
                texture: format!("Interface\\WorldMap\\{}\\{}", a.name, o.texture_name),
                width: o.texture_width,
                height: o.texture_height,
                offset_x: o.offset_x,
                offset_y: o.offset_y,
                explore_bits: o
                    .area_id
                    .iter()
                    .filter(|&&aid| aid != 0)
                    .filter_map(|&aid| areas.get(aid).map(|r| r.explore_flag))
                    .collect(),
                hit_rect: (
                    o.hit_rect_top,
                    o.hit_rect_left,
                    o.hit_rect_bottom,
                    o.hit_rect_right,
                ),
                // The hover label is the first area slot's name (`0x4a7fa0` reads `+0x8` only);
                // with no row the overlay is hidden from the hover, not from the draw.
                area_name: areas.name(o.area_id[0]).map(str::to_string),
            })
            .collect(),
    }
}

/// The catalog off the patch chain, the pure half of [`seed`]; `None` when a required DBC fails
/// to load, which disables the map window.
fn build_catalog(
    chain: &mut Chain,
    areas: &AreaTableCatalog,
    maps: &MapCatalog,
) -> Option<(Vec<WorldMapContinentView>, Data)> {
    let loaded = load_world_map_area_catalog(&mut *chain).and_then(|wma| {
        let wmc = load_world_map_continent_catalog(&mut *chain)?;
        let wmo = load_world_map_overlay_catalog(&mut *chain)?;
        Ok((wma, wmc, wmo))
    });
    let (wma, wmc, wmo) = loaded.ok()?;

    // Continents: the areaId 0 rows in WorldMapArea file order, the Lua continent index
    // (`0x4a5d00`'s walk).
    let cont_rows: Vec<(u32, &WorldMapArea)> = wma.iter().filter(|(_, a)| a.area_id == 0).collect();

    let mut entries = Vec::with_capacity(cont_rows.len());
    let mut views = Vec::with_capacity(cont_rows.len());
    for (_, cont) in cont_rows {
        // Zones: this continent's other rows, sorted case-insensitively by AreaTable name, the
        // Lua zone index (`0x4a6390`'s `SStrCmpI`).
        let mut zones: Vec<(u32, &WorldMapArea, String)> = wma
            .iter()
            .filter(|(_, a)| a.map_id == cont.map_id && a.area_id != 0)
            .map(|(id, a)| {
                let name = areas.name(a.area_id).unwrap_or(a.name.as_str()).to_string();
                (id, a, name)
            })
            .collect();
        zones.sort_by_key(|(_, _, name)| name.to_lowercase());

        let proj = wmc.get(cont.map_id).map(|c| WorldProj {
            offset_u: c.offset_x,
            offset_v: c.offset_y,
            scale: c.scale,
        });
        // The world-sheet rect, `0x4a5d00` over the WorldMapContinent tile bounds: the world-level
        // click box, disjoint per continent unlike the art rects.
        let world_rect = wmc
            .get(cont.map_id)
            .zip(proj)
            .map(|(c, p)| {
                continent_sheet_rect(
                    (
                        c.left_boundary,
                        c.right_boundary,
                        c.top_boundary,
                        c.bottom_boundary,
                    ),
                    p,
                )
            })
            .unwrap_or((0.0, 0.0, 0.0, 0.0));
        let rect = zone_rect(cont);

        // The area bitmap as 1-based zone indices: the load-time remap `0x4a6070`, a one-hop
        // parent rollup, then the (mapId, areaId) match.
        let zone_grid: Vec<u16> = match load_zone_map(&mut *chain, &cont.name) {
            Ok(grid) => grid
                .iter()
                .map(|&raw| {
                    let mut area_id = raw;
                    if let Some(row) = areas.get(area_id) {
                        if row.zone_id != 0 {
                            area_id = row.zone_id; // one hop, not a full walk
                        }
                    }
                    zones
                        .iter()
                        .position(|(_, a, _)| a.area_id == area_id)
                        .map(|i| i as u16 + 1)
                        .unwrap_or(0)
                })
                .collect(),
            // The client tolerates a missing bitmap: hover and click are inert.
            Err(_) => Vec::new(),
        };

        views.push(WorldMapContinentView {
            // Map.dbc's localized name, never the art folder (`0x4a65a0`).
            name: maps
                .name(cont.map_id)
                .unwrap_or(cont.name.as_str())
                .to_string(),
            map_file: cont.name.clone(),
            world_rect,
            loc_rect: (cont.loc_left, cont.loc_right, cont.loc_top, cont.loc_bottom),
            zone_grid,
            zones: zones
                .iter()
                .map(|(wma_id, a, name)| map_row_view(*wma_id, a, name.clone(), areas, &wmo))
                .collect(),
        });
        entries.push(ContinentEntry {
            map_id: cont.map_id,
            proj,
            rect,
            zones: zones
                .iter()
                .map(|(_, a, _)| ZoneEntry {
                    area_id: a.area_id,
                    rect: zone_rect(a),
                })
                .collect(),
        });
    }

    // The instance maps: rows with `areaID != 0` on no continent's map, in file order, unsorted
    // (`0x4a5d00`'s passes `0x4a6130` and `0x4a61c9`); the `areaID` test is the reference's,
    // though inert on the shipped data (`0x4a6170`).
    let continent_maps: Vec<u32> = entries.iter().map(|c| c.map_id).collect();
    let direct: Vec<DirectAreaEntry> = wma
        .iter()
        .filter(|(_, a)| a.area_id != 0 && !continent_maps.contains(&a.map_id))
        .map(|(id, a)| DirectAreaEntry {
            id,
            map_id: a.map_id,
            rect: zone_rect(a),
            view: map_row_view(
                id,
                a,
                areas.name(a.area_id).unwrap_or(a.name.as_str()).to_string(),
                areas,
                &wmo,
            ),
        })
        .collect();

    Some((
        views,
        Data {
            continents: entries,
            direct,
        },
    ))
}

// ---------------------------------------------------------------------------------------------
// The feed

/// Which map the player's position selects (`0x4a6650`), in the Lua-visible encoding; at most one
/// leg is set, since the instance-map loop runs only on a continent miss (`0x4a66c3`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct PlayerSelection {
    /// The 1-based `(continent, zone)`, zone 0 for the continent map: `SetMap(continent, -1)`,
    /// which `GetCurrentMapZone` (`0x4a7f00`) reads back as 0.
    zone: Option<(u32, u32)>,
    /// The `WorldMapArea` row id of the instance map the player is on.
    direct: Option<u32>,
}

/// `SetMapToCurrentZone`'s resolver `0x4a6650` (`0x4a7e20`), which the engine's world-enter sync
/// `0x4947ac` also calls: the continent carrying the player's map, then its zone, where a zone
/// miss leaves the continent map; only on a continent miss, the first instance map on the
/// player's map (`SetMap(-2, id)`, `0x4a6717`); else the world view.
fn resolve_player_selection(
    data: &Data,
    map_id: u32,
    top_zone: Option<u32>,
) -> PlayerSelection {
    if let Some(ci) = data.continents.iter().position(|c| c.map_id == map_id) {
        let zone = top_zone
            .and_then(|top| {
                data.continents[ci]
                    .zones
                    .iter()
                    .position(|z| z.area_id == top)
            })
            .map_or(0, |zi| zi as u32 + 1);
        return PlayerSelection {
            zone: Some((ci as u32 + 1, zone)),
            direct: None,
        };
    }
    PlayerSelection {
        zone: None,
        direct: data
            .direct
            .iter()
            .find(|d| d.map_id == map_id)
            .map(|d| d.id),
    }
}

/// A position's UV on the displayed map (`0x4a7360`), `None` off it, which Lua sees as the
/// `(0, 0)` hide: the world sheet projects through the position's own continent constants, a
/// continent or zone through its rect, an instance map through its row's rect (`0x4a73e0`). The
/// last two require the position's own map (`0x4a7437`).
fn project_on_displayed(
    data: &Data,
    selection: (u32, u32, Option<u32>),
    pos_map: u32,
    px: f32,
    py: f32,
) -> Option<(f32, f32)> {
    match selection {
        // The direct cell first: it shares the `(0, 0)` pair with the world sheet.
        (_, _, Some(id)) => data
            .direct
            .iter()
            .find(|d| d.id == id && d.map_id == pos_map)
            .map(|d| zone_uv(d.rect, px, py)),
        (0, _, None) => data
            .continents
            .iter()
            .find(|cont| cont.map_id == pos_map)
            .and_then(|cont| cont.proj)
            .map(|p| world_uv(p, px, py)),
        (c, z, None) => data
            .continents
            .get(c as usize - 1)
            .filter(|cont| cont.map_id == pos_map)
            .and_then(|cont| match z {
                0 => Some(cont.rect),
                z => cont.zones.get(z as usize - 1).map(|zone| zone.rect),
            })
            .map(|rect| zone_uv(rect, px, py)),
    }
}

/// The inputs the landmark sources are a pure function of. Besides a selection change, `0x4a67a0`
/// rebuilds the list on these: a world-state push (`0x48fa0d`) and a `PLAYER_EXPLORED_ZONES`
/// change (`0x4a6477`, an UpdateFields callback on byte `0xe6c`).
#[derive(PartialEq, Eq)]
struct LandmarkKey {
    map: u32,
    /// [`net::ui_world_states`]' generation, which moves on every wire write.
    states: u64,
    explored: Vec<u32>,
}

/// [`WorldMapLandmarkSource::icons`]' order.
const LEVELS: [WorldMapLevel; 3] = [
    WorldMapLevel::World,
    WorldMapLevel::Continent,
    WorldMapLevel::Zone,
];

/// `0x4a67a0`'s AreaPOI gates, in the reference's order:
///
/// 1. Level flag (`0x4a79b0`): `0x04` at zone level, `0x08` at continent, and both `0x10` and
///    `0x08` at world level, since the chain falls through rather than switching.
/// 2. Exploration (`0x4a6890`-`0x4a68f3`): a signed `AreaID > 0` (`-1` is continent-wide) whose
///    `AreaTable` row has `ExplorationLevel >= 0` must be discovered.
/// 3. World state (`0x4a6903`): a row with a `WorldStateID` shows only while that state is
///    non-zero; the Eastern Plaguelands towers are one row per tower, owner and state.
///
/// The loop reads no `ContinentID`, `Importance`, `FactionID` or `Icon`.
fn landmark_gates_pass(
    poi: &AreaPoi,
    level: WorldMapLevel,
    areas: &AreaTableCatalog,
    explored: &[u32],
    world_states: &HashMap<u32, u32>,
) -> bool {
    let level_ok = match level {
        WorldMapLevel::Zone => poi.flags & 0x04 != 0,
        WorldMapLevel::Continent => poi.flags & 0x08 != 0,
        WorldMapLevel::World => poi.flags & 0x10 != 0 && poi.flags & 0x08 != 0,
    };
    if !level_ok {
        return false;
    }
    // A row the table does not carry passes: the bounds check on `[0xc0e048]`/`[0xc0e04c]`
    // guards the read, not the landmark.
    if poi.area_id as i32 > 0 {
        if let Some(area) = areas.get(poi.area_id) {
            if area.exploration_level >= 0 && !explored_bit(explored, area.explore_flag) {
                return false;
            }
        }
    }
    // An absent key reads 0, as the app's `WorldStates::get` does: only a set-nonzero value
    // shows the row.
    poi.world_state_id == 0 || world_states.get(&poi.world_state_id).copied().unwrap_or(0) != 0
}

/// `PLAYER_EXPLORED_ZONES` bit `bit`, which `0x4a9a40` reads bytewise; the little-endian dword
/// slots hold the same bits.
fn explored_bit(explored: &[u32], bit: u32) -> bool {
    let (slot, within) = (bit as usize / 32, bit % 32);
    explored.get(slot).is_some_and(|w| w & (1 << within) != 0)
}

/// `GetMapLandmarkInfo`'s `textureIndex` (`0x4a8848`-`0x4a8877`): the row's `Icon`, but cell 15
/// at zone level for a row without `Flags & 0x80`, which the shipped data sets on exactly the
/// world-state rows.
fn landmark_texture_index(poi: &AreaPoi, level: WorldMapLevel) -> u32 {
    /// The generic zone-level POI cell of `Interface\Minimap\POIIcons`.
    const ZONE_SUBSTITUTE: u32 = 15;
    match poi.flags & 0x80 != 0 || level != WorldMapLevel::Zone {
        true => poi.icon,
        false => ZONE_SUBSTITUTE,
    }
}

/// Build the landmark sources the engine projects onto each selection it stores: the `AreaPOI`
/// rows past the gates in file order, then the guard-directions marker (`0x4a69c7`), which this
/// layer does not track yet.
fn landmark_sources(
    pois: Option<&AreaPoiCatalog>,
    areas: &AreaTableCatalog,
    explored: &[u32],
    world_states: &HashMap<u32, u32>,
) -> Vec<WorldMapLandmarkSource> {
    pois.map(|pois| {
        pois.rows()
            .filter_map(|(_, poi)| {
                let icons = LEVELS.map(|level| {
                    landmark_gates_pass(poi, level, areas, explored, world_states)
                        .then(|| landmark_texture_index(poi, level))
                });
                icons.iter().any(Option::is_some).then(|| WorldMapLandmarkSource {
                    name: poi.name.clone(),
                    description: poi.description.clone(),
                    map: poi.continent_id,
                    pos: (poi.pos[0], poi.pos[1]),
                    icons,
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Push the catalog into the VM before the toc walk (`bn_ui_load`), and seat this load's feed
/// state. A fresh VM starts with the world sheet selected and no memos, exactly as the reference
/// boots.
pub(crate) fn seed(chain: &mut Chain, script: &mut UiScript) {
    MAP.with(|slot| *slot.borrow_mut() = None);
    let Some(areas) = load_area_table_catalog(chain).ok() else {
        eprintln!("world map: AreaTable.dbc missing, map window disabled");
        return;
    };
    let Some(maps) = load_map_catalog(chain).ok() else {
        eprintln!("world map: Map.dbc missing, map window disabled");
        return;
    };
    let Some((views, data)) = build_catalog(chain, &areas, &maps) else {
        eprintln!("world map: DBC load failed, map window disabled");
        return;
    };
    eprintln!(
        "world map: catalog — {} continents, {} zones, {} instance maps",
        views.len(),
        views.iter().map(|c| c.zones.len()).sum::<usize>(),
        data.direct.len()
    );
    script.set_world_map_catalog(views);
    // The instance maps, at the same edge: the reference fills both containers in one walk.
    script.set_world_map_direct_areas(
        data.direct
            .iter()
            .map(|d| (d.id, d.view.clone()))
            .collect(),
    );
    let pois = load_area_poi_catalog(chain).ok();
    MAP.with(|slot| {
        *slot.borrow_mut() = Some(Map {
            data,
            areas,
            pois,
            landmarks: None,
            explored: None,
            synced: false,
            projector: false,
            area_chunk: None,
            top_zone: None,
        });
    });
}

/// The per-frame feed (`feed_world_map` in the app): the discovery bits, the player's selection
/// and projected positions, and the landmark sources when one of their inputs moves.
pub(crate) fn sync(script: &mut UiScript) {
    let Some(player) = net::ui_player() else {
        return;
    };
    let Some(own) = net::ui_self_fields() else {
        return;
    };
    let explored: Vec<u32> = (0..PLAYER_EXPLORED_ZONES_SLOTS)
        .map(|i| own.player_explored_zone_slot(i))
        .collect();
    let (wx, wy) = (player.pos[0], player.pos[1]);
    let chunk = benilla_formats::world_to_chunk(wx, wy);
    MAP.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(map) = slot.as_mut() else {
            return;
        };

        // `PLAYER_EXPLORED_ZONES`, pushed on change; the setter queues `WORLD_MAP_UPDATE` for
        // newly explored art.
        if map.explored.as_ref() != Some(&explored) {
            map.explored = Some(explored.clone());
            script.set_world_map_explored(explored.clone());
        }

        // The current area is the MCNK leaf; its top zone is the zone-level area the reference
        // matches. The tile walk runs only when the body crossed a chunk.
        if map.area_chunk != Some(chunk) {
            if let Some(area) = crate::audio::player_area() {
                map.area_chunk = Some(chunk);
                map.top_zone = map.areas.top_zone(area);
            }
        }
        let top_zone = map.top_zone;
        let player_sel = resolve_player_selection(&map.data, player.map, top_zone);

        // `GetWorldLocMapPosition` and the setter's landmark pass project synchronously with the
        // engine's own selection, so the engine holds the projection over its own copy of the
        // catalog.
        if !map.projector {
            map.projector = true;
            let data = map.data.clone();
            script.set_world_loc_projector(Box::new(move |selection, map_id, x, y| {
                project_on_displayed(&data, selection, map_id, x, y)
            }));
        }

        // First world enter: the zone updater `0x494780`, finding the cached zone id 0, selects
        // the player's zone itself through `0x4a6650` and the setter `0x4a67a0` before any Lua
        // asks. Once per VM, each login being a first enter.
        if !map.synced && top_zone.is_some() {
            map.synced = true;
            let (c, z) = player_sel.zone.unwrap_or((0, 0));
            script.sync_world_map_to_player_zone(c, z, player_sel.direct);
        }

        let selection = script.world_map_selection();
        let project = |pos_map: u32, px: f32, py: f32| {
            project_on_displayed(&map.data, selection, pos_map, px, py)
        };
        let uv = project(player.map, wx, wy);

        // The landmark sources, rebuilt only when a `LandmarkKey` input moves; the engine
        // projects them onto each selection it stores.
        let states = net::world_state_epoch();
        let key = LandmarkKey {
            map: player.map,
            states,
            explored,
        };
        if map.landmarks.as_ref() != Some(&key) {
            let states = net::ui_world_states();
            let sources = landmark_sources(map.pois.as_ref(), &map.areas, &key.explored, &states);
            map.landmarks = Some(key);
            script.set_world_map_landmark_sources(sources);
        }

        // The `party1..4` blips: a streamed member's pose. `SMSG_PARTY_MEMBER_STATS` carries a
        // zone, not a map, so the map is the continent owning that zone, else ours.
        let party = net::ui_party();
        let member_uv = |m: &net::BnNetPartyMember| {
            let pose = net::ui_unit_pose(m.guid)?;
            let member_map = map
                .data
                .continents
                .iter()
                .find(|cont| {
                    m.zone != 0 && cont.zones.iter().any(|z| z.area_id == m.zone)
                })
                .map(|cont| cont.map_id)
                .unwrap_or(player.map);
            project(member_map, pose[0], pose[1]).filter(|uv| *uv != (0.0, 0.0))
        };
        let party_uv: Vec<Option<(f32, f32)>> = party
            .members
            .iter()
            .take(party.count as usize)
            .map(member_uv)
            .collect();

        // No corpse position and no raid roster in this layer yet: both read as the Lua hide.
        script.set_world_map_feed(
            player_sel.zone,
            uv,
            player.facing,
            None,
            party_uv,
            Vec::new(),
        );
        // The instance-map leg for `SetMapToCurrentZone`, which the battlefield minimap calls on
        // `PLAYER_ENTERING_WORLD`.
        script.set_world_map_player_direct_area(player_sel.direct);
    });
}

/// The world map's player arrow, one TSV row in root units (y up):
/// `left \t top \t width \t height \t scale \t facing`. The stock arrow is a `Model` pane
/// (`WorldMapFrame.lua`'s `CreateWorldMapArrowFrame`), which the region dump cannot carry; the
/// host paints it from the minimap's own baked arrow atlas. Empty while the arrow is absent or
/// hidden with its frame chain, or while its file's facts have not landed.
pub(crate) fn arrow_row() -> String {
    crate::BN_UI.with(|slot| {
        let held = slot.borrow();
        let Some(s) = held.as_ref() else {
            return String::new();
        };
        let Some(a) = s.world_map_arrow() else {
            return String::new();
        };
        format!(
            "{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.4}\t{:.4}\n",
            a.rect.left,
            a.rect.top,
            a.rect.width(),
            a.rect.height(),
            a.scale,
            a.facing
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> ZoneRect {
        ZoneRect {
            left: 1.0,
            right: -1.0,
            top: 1.0,
            bottom: -1.0,
        }
    }

    /// Kalimdor (map 1), Azeroth (map 0) with Elwynn at index 9 (Lua zone 10), and Warsong
    /// Gulch's real row (`WorldMapArea` 443, map 489) as the one instance map.
    fn data() -> Data {
        Data {
            continents: vec![
                ContinentEntry {
                    map_id: 1,
                    proj: None,
                    rect: rect(),
                    zones: Vec::new(),
                },
                ContinentEntry {
                    map_id: 0,
                    proj: None,
                    rect: rect(),
                    zones: (0..10)
                        .map(|i| ZoneEntry {
                            area_id: if i == 9 { 12 } else { 900 + i },
                            rect: rect(),
                        })
                        .collect(),
                },
            ],
            direct: vec![DirectAreaEntry {
                id: 443,
                map_id: 489,
                rect: rect(),
                view: WorldMapZoneView::default(),
            }],
        }
    }

    #[test]
    fn an_unresolved_zone_falls_back_to_the_continent_not_the_world() {
        let d = data();
        let on_continent = |c, z| PlayerSelection {
            zone: Some((c, z)),
            direct: None,
        };

        assert_eq!(
            resolve_player_selection(&d, 0, Some(12)),
            on_continent(2, 10)
        );
        assert_eq!(
            resolve_player_selection(&d, 0, Some(4242)),
            on_continent(2, 0)
        );
        assert_eq!(resolve_player_selection(&d, 0, None), on_continent(2, 0));
        assert_eq!(
            resolve_player_selection(&d, 389, Some(12)),
            PlayerSelection::default(),
            "a map with no WorldMapArea row is the world view"
        );
    }

    #[test]
    fn a_battleground_takes_the_orphan_leg_and_projects_on_its_own_rect() {
        let d = data();
        assert_eq!(
            resolve_player_selection(&d, 489, Some(3277)),
            PlayerSelection {
                zone: None,
                direct: Some(443),
            }
        );
        let sel = (0, 0, Some(443));
        assert_eq!(
            project_on_displayed(&d, sel, 489, 0.0, 0.0),
            Some((0.5, 0.5))
        );
        assert_eq!(project_on_displayed(&d, sel, 0, 0.0, 0.0), None);
        assert_eq!(project_on_displayed(&d, (0, 0, None), 489, 0.0, 0.0), None);
        assert_eq!(WorldMapLevel::of(sel), WorldMapLevel::Zone);
        assert_eq!(WorldMapLevel::of((0, 0, None)), WorldMapLevel::World);
    }

    #[test]
    fn the_level_flag_chain_falls_through_at_world_level() {
        let (areas, states, explored) = (
            AreaTableCatalog::from_rows(Vec::new()),
            HashMap::new(),
            Vec::new(),
        );
        let poi = |flags| AreaPoi {
            importance: 0,
            icon: 4,
            faction_id: 0,
            pos: [0.0; 3],
            continent_id: 0,
            flags,
            area_id: 0,
            name: String::new(),
            description: String::new(),
            world_state_id: 0,
        };
        let pass = |flags, level| {
            landmark_gates_pass(&poi(flags), level, &areas, &explored, &states)
        };

        assert!(pass(0x0d, WorldMapLevel::Zone));
        assert!(pass(0x0d, WorldMapLevel::Continent));
        assert!(!pass(0x0d, WorldMapLevel::World));
        assert!(pass(0x1d, WorldMapLevel::World));
        assert!(pass(0x87, WorldMapLevel::Zone));
        assert!(!pass(0x87, WorldMapLevel::Continent));
        assert!(!pass(0x10, WorldMapLevel::World));
    }

    #[test]
    fn a_world_state_row_appears_only_while_its_key_is_set() {
        let (areas, explored) = (AreaTableCatalog::from_rows(Vec::new()), Vec::new());
        let mut states = HashMap::new();
        let poi = |world_state_id| AreaPoi {
            importance: 0,
            icon: 4,
            faction_id: 0,
            pos: [0.0; 3],
            continent_id: 0,
            flags: 0x87,
            area_id: 0,
            name: String::new(),
            description: String::new(),
            world_state_id,
        };
        let pass = |p: &AreaPoi, st: &HashMap<u32, u32>| {
            landmark_gates_pass(p, WorldMapLevel::Zone, &areas, &explored, st)
        };

        assert!(!pass(&poi(2372), &states));
        states.insert(2372, 1);
        assert!(pass(&poi(2372), &states));
        assert!(!pass(&poi(2373), &states));
        states.insert(2372, 0);
        assert!(!pass(&poi(2372), &states), "a zero value hides the row again");
    }

    #[test]
    fn the_zone_level_icon_substitution_spares_the_world_state_rows() {
        let poi = |flags| AreaPoi {
            importance: 0,
            icon: 4,
            faction_id: 0,
            pos: [0.0; 3],
            continent_id: 0,
            flags,
            area_id: 0,
            name: String::new(),
            description: String::new(),
            world_state_id: 0,
        };
        let town = poi(0x0d);
        let tower = poi(0x87);
        assert_eq!(landmark_texture_index(&town, WorldMapLevel::Zone), 15);
        assert_eq!(landmark_texture_index(&town, WorldMapLevel::Continent), 4);
        for level in LEVELS {
            assert_eq!(landmark_texture_index(&tower, level), 4);
        }
    }

    /// The 5875 WorldMapContinent rows, and the world-sheet spans evaluated by hand.
    #[test]
    fn continent_sheet_rects_disjoint() {
        let ek = WorldProj {
            offset_u: 14.5,
            offset_v: -7.0,
            scale: 0.75,
        };
        let kal = WorldProj {
            offset_u: -19.0,
            offset_v: -0.322_498,
            scale: 0.75,
        };
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let ek_rect = continent_sheet_rect((23, 47, 15, 61), ek);
        let kal_rect = continent_sheet_rect((23, 48, 9, 52), kal);
        assert!(
            near(ek_rect.0, 0.62375)
                && near(ek_rect.1, 0.02695)
                && near(ek_rect.2, 0.92315)
                && near(ek_rect.3, 0.87126),
            "{ek_rect:?}"
        );
        assert!(
            near(kal_rect.0, 0.08882)
                && near(kal_rect.1, 0.07910)
                && near(kal_rect.2, 0.40020)
                && near(kal_rect.3, 0.86952),
            "{kal_rect:?}"
        );
        assert!(kal_rect.2 < ek_rect.0, "the sheet rects are disjoint on u");
    }

    #[test]
    fn zone_uv_matches_the_reference_edges_and_hides_off_map() {
        let elwynn = ZoneRect {
            left: 1535.4166,
            right: -1935.4166,
            top: -7939.583,
            bottom: -10254.166,
        };
        let (u, v) = zone_uv(elwynn, -9450.0, 60.0);
        assert!((u - 0.42509).abs() < 1e-4 && (v - 0.65256).abs() < 1e-4);
        assert_eq!(zone_uv(elwynn, -9450.0, elwynn.left + 10.0), (0.0, 0.0));
        assert_eq!(zone_uv(elwynn, elwynn.top + 10.0, 60.0), (0.0, 0.0));
    }

    #[test]
    fn world_uv_matches_stormwind() {
        let ek = WorldProj {
            offset_u: 14.5,
            offset_v: -7.0,
            scale: 0.75,
        };
        let (u, v) = world_uv(ek, -8842.0, 626.0);
        assert!((u - 0.71748).abs() < 1e-4 && (v - 0.63016).abs() < 1e-4);
    }
}

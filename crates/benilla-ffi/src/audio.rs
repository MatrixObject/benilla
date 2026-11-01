//! The client's soundscape in the middle layer: the zone's background music, its intro fanfare,
//! the looping ambience bed, and the UI's `PlaySound` / `PlayMusic` verbs.
//!
//! The rules are the reference's — the music transport `0x460040`, the zone-music change
//! `0x4602e0`, the due start `0x460240`, the weighted pick `0x45bb70` and the ambience selector
//! `0x460bd0` — scheduled over kira's primitives, the way `benilla-app` does it, with no Bevy.
//!
//! All of it runs on one thread of ours: the catalogs, every MPQ read and every stream open. The
//! game thread only publishes the `AreaTable` id under the player (from the MCNK chunks of the
//! tile it is already streaming) and the Lua verbs it queued, then never touches audio again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use benilla_adt::McnkChunk;
use benilla_formats::sound_kit_flags::LOOPING;
use benilla_formats::{
    load_area_sound_catalog, load_sound_kit_catalog, AreaSoundCatalog, Chain, SoundKitCatalog,
    ZoneIntroEntry, ZoneMusicEntry, CHUNK_SIZE, TILE_SIZE,
};
use benilla_ui::script::UiScript;
use kira::sound::static_sound::StaticSoundData;
use kira::sound::streaming::{StreamingSoundData, StreamingSoundHandle};
use kira::sound::{FromFileError, PlaybackState};
use kira::{AudioManager, AudioManagerSettings, Decibels, Tween};

use crate::audio_out::CpalBackend;
use crate::net;

/// The music a slot gives up over: the reference's `0x7a5a10(4.0f)` on a zone change (`0x4604d4`).
const MUSIC_FADE_OUT_MS: u64 = 4000;
/// The ambience crossfade (`0x460bd0`'s fade leg).
const AMBIENCE_FADE_MS: u64 = 5000;
/// A `PlayMusic` rearms the zone pump this far out (`0x460450`'s `+6000`).
const LUA_MUSIC_SCHEDULE_SECS: f64 = 6.0;
/// `0x460450` opens its own track at full category volume.
const LUA_MUSIC_VOLUME: f32 = 1.0;
/// The volume ramp one beat applies: kira holds a track's volume constant per chunk, so a stepped
/// feed clicks.
const GLIDE_MS: u64 = 15;
/// The de-click fade on a teardown.
const DECLOCK_MS: u64 = 250;
/// Which half of each `[day, night]` row pair to read. Deviation: the middle layer has no game
/// clock yet (day/night is its own open task), so every zone plays its day set.
const PHASE: usize = 0;
/// The request queue's ceiling: a soundscape that cannot keep up drops the oldest verb.
const REQUEST_CAP: usize = 256;
/// The published tiles kept: a long session must not hold every tile it ever walked over.
const TILE_CAP: usize = 96;
/// The decoded one-shots kept by path.
const SFX_CACHE_CAP: usize = 128;

// ---------------------------------------------------------------------------------------------
// The game thread's side

/// One chunk's `AreaTable` id and the WoW coords of its north-west corner.
struct ChunkArea {
    corner: [f32; 3],
    area_id: u32,
}

/// Which of the reference's three channels a gain belongs to.
enum Channel {
    Sfx,
    Music,
    Ambience,
}

/// What the audio thread needs each beat: where the player is and how loud things are.
#[derive(Clone, Copy)]
struct Tick {
    /// A live world session; off it the soundscape is torn down.
    in_world: bool,
    /// The `AreaTable` id under the player, once its tile is streamed.
    area: Option<u32>,
    ghost: bool,
    master: f32,
    music: f32,
    ambience: f32,
    sfx: f32,
    enabled: bool,
    music_on: bool,
    ambience_on: bool,
    no_delay: bool,
}

impl Tick {
    /// The reference's category gain: a disabled channel, or one the player switched off, is
    /// silence (`0x7a5dc0`'s channel volume).
    fn amp(&self, cat: Channel) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        match cat {
            Channel::Sfx => self.sfx,
            Channel::Music if self.music_on => self.music,
            Channel::Ambience if self.ambience_on => self.ambience,
            _ => 0.0,
        }
    }
}

/// The stock 1.12 defaults, until the first tick arrives.
impl Default for Tick {
    fn default() -> Self {
        Self {
            in_world: false,
            area: None,
            ghost: false,
            master: 1.0,
            music: 0.4,
            ambience: 0.6,
            sfx: 1.0,
            enabled: true,
            music_on: true,
            ambience_on: true,
            no_delay: false,
        }
    }
}

/// One queued sound verb, in call order.
enum Request {
    /// `PlaySound("name")`: a kit name, a number's decimal spelling included.
    SoundKit(String),
    /// `PlaySoundFile("path")`: a file, no kit.
    SoundFile(String),
    /// `PlayMusic("path")`.
    PlayMusic(String),
    /// `StopMusic()`.
    StopMusic,
}

struct Inner {
    data_dir: String,
    /// The audio thread's one attempt at starting has been made.
    started: bool,
    /// The map whose tiles are published; a continent swap drops the old grid.
    map_dir: String,
    tiles: HashMap<(i32, i32), Arc<Vec<ChunkArea>>>,
    tick: Option<Tick>,
    requests: VecDeque<Request>,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

static SHARED: LazyLock<Arc<Shared>> = LazyLock::new(|| {
    Arc::new(Shared {
        inner: Mutex::new(Inner {
            data_dir: String::new(),
            started: false,
            map_dir: String::new(),
            tiles: HashMap::new(),
            tick: None,
            requests: VecDeque::new(),
        }),
        wake: Condvar::new(),
    })
});

fn lock(sh: &Shared) -> std::sync::MutexGuard<'_, Inner> {
    sh.inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Data directory the audio thread opens its own chain from, from [`crate::bn_init`].
pub(crate) fn set_data_dir(dir: &str) {
    let sh = shared();
    let mut inner = lock(&sh);
    inner.data_dir = dir.to_owned();
}

fn shared() -> Arc<Shared> {
    Arc::clone(&SHARED)
}

/// Publish the area grid of the tile `World\Maps\<map>\<map>_<x>_<z>.adt` the renderer just
/// parsed: no extra read, and the audio thread finds the player's zone for free.
pub(crate) fn note_tile(map: &str, x: i32, z: i32, mcnk: &[McnkChunk]) {
    if mcnk.is_empty() {
        return;
    }
    let chunks: Vec<ChunkArea> = mcnk
        .iter()
        .map(|c| ChunkArea {
            corner: c.header.position,
            area_id: c.header.area_id,
        })
        .collect();
    let sh = shared();
    {
        let mut inner = lock(&sh);
        if inner.map_dir != map {
            // Another continent: nothing streamed for the old one can answer for the new.
            inner.map_dir = map.to_owned();
            inner.tiles.clear();
        }
        if inner.tiles.len() >= TILE_CAP {
            inner.tiles.clear();
        }
        inner.tiles.insert((x, z), Arc::new(chunks));
        static TILE_NOTES: AtomicBool = AtomicBool::new(false);
        if !TILE_NOTES.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let c = mcnk.first().map(|c| (c.header.position, c.header.area_id));
            let last = mcnk.last().map(|c| (c.header.position, c.header.area_id));
            eprintln!(
                "audio: published tile {map} {x},{z}: first {c:?} last {last:?} count {}",
                mcnk.len()
            );
        }
    }
    sh.wake.notify_all();
}

/// The game thread's beat, from [`crate::bn_ui_tick`]: publish what changed and wake the audio
/// thread. Never reads an archive, never waits on audio.
pub(crate) fn ui_tick(script: &mut UiScript) {
    let in_world = script.vm_kind() == benilla_ui::script::VmKind::InGame;
    let sounds = script.take_sounds();
    let music = script.take_music();
    let mut requests: Vec<Request> = Vec::with_capacity(sounds.len() + music.len());
    if in_world {
        for r in sounds {
            requests.push(match r {
                benilla_ui::script::SoundRequest::KitName(name) => Request::SoundKit(name),
                benilla_ui::script::SoundRequest::File(path) => Request::SoundFile(path),
            });
        }
        for m in music {
            requests.push(match m {
                benilla_ui::script::MusicRequest::Play(path) => Request::PlayMusic(path),
                benilla_ui::script::MusicRequest::Stop => Request::StopMusic,
            });
        }
    }

    let area = if in_world { player_area() } else { None };
    probe(area, in_world);
    let ghost = net::ui_player().is_some_and(|p| p.ghost != 0);
    let value = |name: &str| script.cvar(name).unwrap_or_default();
    let number = |name: &str, default: f32| {
        value(name)
            .trim()
            .parse::<f32>()
            .unwrap_or(default)
    };
    let toggle = |name: &str| number(name, 0.0) != 0.0;
    let tick = Tick {
        in_world,
        area,
        ghost,
        master: number("MasterVolume", 1.0).clamp(0.0, 1.0),
        music: number("MusicVolume", 0.4).clamp(0.0, 1.0),
        ambience: number("AmbienceVolume", 0.6).clamp(0.0, 1.0),
        sfx: number("SoundVolume", 1.0).clamp(0.0, 1.0),
        enabled: toggle("MasterSoundEffects"),
        music_on: toggle("EnableMusic"),
        ambience_on: toggle("EnableAmbience"),
        no_delay: toggle("SoundZoneMusicNoDelay"),
    };

    let sh = shared();
    {
        let mut inner = lock(&sh);
        inner.tick = Some(tick);
        for r in requests.into_iter() {
            if inner.requests.len() >= REQUEST_CAP {
                inner.requests.pop_front();
            }
            inner.requests.push_back(r);
        }
        if !inner.started && !inner.data_dir.is_empty() {
            inner.started = true;
            let thread_sh = Arc::clone(&sh);
            let data_dir = inner.data_dir.clone();
            // Silent on any failure: a device that will not open must not cost the frame.
            if let Err(e) = std::thread::Builder::new()
                .name("bn-audio".to_owned())
                .spawn(move || run(thread_sh, data_dir))
            {
                eprintln!("audio: {e}");
            }
        }
    }
    sh.wake.notify_all();
}

/// The `AreaTable` id under the net player, from the tile the renderer already parsed. Shared
/// with the world map's zone resolution (`crate::world_map`), which memoizes it per chunk.
pub(crate) fn player_area() -> Option<u32> {
    let p = net::ui_player()?;
    let (fx, fy) = benilla_formats::world_to_chunk(p.pos[0], p.pos[1]);
    let key = (
        fx as i32 / crate::STREAM_CHUNKS_PER_TILE,
        fy as i32 / crate::STREAM_CHUNKS_PER_TILE,
    );
    let tile = {
        let sh = shared();
        let inner = lock(&sh);
        inner.tiles.get(&key).cloned()
    }?;
    let per_chunk = TILE_SIZE / CHUNK_SIZE;
    tile.iter().find_map(|c| {
        // `ChunkMesh::area_at`: the chunk owns the yard square whose north-west corner it carries.
        let south = (c.corner[0] - p.pos[0]) / TILE_SIZE * per_chunk;
        let east = (c.corner[1] - p.pos[1]) / TILE_SIZE * per_chunk;
        if (0.0..=1.0).contains(&south) && (0.0..=1.0).contains(&east) {
            Some(c.area_id)
        } else {
            None
        }
    })
}

/// One line a second for the first twenty in-world seconds: the numbers the scheduler keys on, so
/// a silent soundscape is settled from them rather than from what is audible.
fn probe(area: Option<u32>, in_world: bool) {
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
    static BEAT: AtomicU32 = AtomicU32::new(0);
    if !in_world {
        return;
    }
    let n = BEAT.fetch_add(1, Relaxed);
    if n % 60 != 0 || n >= 1200 {
        return;
    }
    let p = net::ui_player();
    let key = p.map(|p| {
        let (fx, fy) = benilla_formats::world_to_chunk(p.pos[0], p.pos[1]);
        (
            fx as i32 / crate::STREAM_CHUNKS_PER_TILE,
            fy as i32 / crate::STREAM_CHUNKS_PER_TILE,
        )
    });
    let sh = shared();
    let inner = lock(&sh);
    eprintln!(
        "audio: beat {n} player {} map {:?} pos {:?} key {key:?} map {:?} tiles {:?} area {area:?}",
        p.is_some(),
        p.map(|p| p.map),
        p.map(|p| p.pos),
        inner.map_dir,
        inner.tiles.keys().take(4).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------------------------
// The audio thread

/// The thread's whole life: build the soundscape, then pump it. An engine that cannot be built
/// (no output device, no patch chain) leaves the client silent and the thread exits, so nothing
/// retries against a dead device every beat.
fn run(sh: Arc<Shared>, data_dir: String) {
    match Engine::new(&data_dir) {
        Ok(mut engine) => {
            say("audio: output open");
            let start = Instant::now();
            let mut last: Option<Tick> = None;
            loop {
                let requests: Vec<Request> = {
                    let mut inner = lock(&sh);
                    if inner.tick.is_none() && inner.requests.is_empty() {
                        let (guard, _) = sh
                            .wake
                            .wait_timeout(inner, Duration::from_millis(100))
                            .unwrap_or_else(|e| e.into_inner());
                        inner = guard;
                    }
                    if let Some(t) = inner.tick.take() {
                        last = Some(t);
                    }
                    inner.requests.drain(..).collect()
                };
                engine.pump(start.elapsed().as_secs_f64(), last.as_ref(), requests);
            }
        }
        Err(e) => {
            say(format!("audio: staying silent — {e}"));
        }
    }
}

/// One soundscape line, where a run can read it: this thread's stderr, which the host redirects,
/// and the middle layer's event queue, which `bn.NetEvents` drains.
fn say(line: impl std::fmt::Display) {
    let line = line.to_string();
    eprintln!("{line}");
    net::note(line);
}

/// A weighted pick's remaining pool, `0x45bb70`'s per-kit variation state.
struct PickState {
    weights: Vec<u32>,
    remaining: Vec<u32>,
}

/// The catalogs, the mix and the scheduler's state — all of it on this thread only.
struct Engine {
    chain: Chain,
    data_dir: String,
    areas: AreaSoundCatalog,
    kits: SoundKitCatalog,
    manager: AudioManager<CpalBackend>,
    /// The last published gains, so a start and the per-frame feed agree.
    vol: Tick,
    area: Option<u32>,
    zone_music: u32,
    /// The zone track (`0xb06cc4`).
    music: Option<StreamingSoundHandle<FromFileError>>,
    music_kit: u32,
    music_kit_vol: f32,
    next_track_at: Option<f64>,
    /// The Lua `PlayMusic` slot (`0xb06ccc`), beside the zone track's.
    lua_music: Option<StreamingSoundHandle<FromFileError>>,
    lua_music_path: Option<String>,
    /// Paths already reported, so a per-beat retry of a bad pick says it once.
    unplayable: HashSet<String>,
    ambience_kit: u32,
    ambience: Option<StreamingSoundHandle<FromFileError>>,
    ambience_kit_vol: f32,
    ambience_fade_in: Option<(f64, f64)>,
    intro_last: HashMap<u32, f64>,
    /// Beats pumped since the engine opened, for the one-shot first-beat line.
    beats: u64,
    picks: HashMap<u32, PickState>,
    pick_rng: u32,
    silence_rng: u32,
    sfx_cache: HashMap<String, StaticSoundData>,
}

impl Engine {
    fn new(data_dir: &str) -> Result<Self, String> {
        let mut chain = Chain::open(std::path::Path::new(data_dir)).map_err(|e| format!("{e:#}"))?;
        let areas = load_area_sound_catalog(&mut chain)
            .map_err(|e| format!("the zone audio tables — {e:#}"))?;
        let kits =
            load_sound_kit_catalog(&mut chain).map_err(|e| format!("SoundEntries — {e:#}"))?;
        let manager = AudioManager::<CpalBackend>::new(AudioManagerSettings {
            backend_settings: (),
            ..Default::default()
        })
        .map_err(|e| format!("{e}"))?;
        Ok(Self {
            chain,
            data_dir: data_dir.to_owned(),
            areas,
            kits,
            manager,
            vol: Tick::default(),
            area: None,
            zone_music: 0,
            music: None,
            music_kit: 0,
            music_kit_vol: 1.0,
            next_track_at: None,
            lua_music: None,
            lua_music_path: None,
            unplayable: HashSet::new(),
            ambience_kit: 0,
            ambience: None,
            ambience_kit_vol: 1.0,
            ambience_fade_in: None,
            intro_last: HashMap::new(),
            beats: 0,
            picks: HashMap::new(),
            pick_rng: 0x9e37_79b9,
            silence_rng: 0x1234_5677,
            sfx_cache: HashMap::new(),
        })
    }

    /// One beat: the gains, the sound verbs, the area change, the bed, the music transport.
    fn pump(&mut self, now: f64, tick: Option<&Tick>, requests: Vec<Request>) {
        if let Some(t) = tick {
            self.vol = *t;
        }
        for r in requests {
            self.request(r, now);
        }
        // The master slider lives on the main track, upstream of every channel.
        self.manager
            .main_track()
            .set_volume(amp_to_db(self.vol.master), glide());
        if !self.vol.in_world {
            self.leave_world();
            return;
        }
        if self.beats == 0 {
            say(format!(
                "audio: first beat area {:?} ghost {} volumes m {} a {}",
                self.vol.area, self.vol.ghost, self.vol.music, self.vol.ambience
            ));
        }
        self.beats += 1;

        // ---- the zone-music change, `0x4602e0` ----
        if self.vol.area != self.area {
            self.area = self.vol.area;
            let resolved = self.area.and_then(|id| self.areas.resolve(id));
            let music_row = resolved
                .as_ref()
                .and_then(|r| r.music.map(|m| m.id))
                .unwrap_or(0);
            let intro = resolved.as_ref().and_then(|r| r.intro).map(|i| i.id);
            say(format!(
                "audio: area {:?} row {music_row} intro {:?} amb {:?}",
                self.area.map(|a| (a, resolved.as_ref().map(|r| r.area_name).unwrap_or("?"))),
                intro,
                resolved.as_ref().and_then(|r| r.ambience.map(|a| a.kits[PHASE]))
            ));
            if music_row != self.zone_music {
                self.zone_music = music_row;
                let kit = self.zone_row(music_row).map(|m| m.sounds[PHASE]);
                // The fade finishes on the device after the handle drops (`0x460346`).
                if let Some(mut h) = self.music.take() {
                    h.stop(fade(MUSIC_FADE_OUT_MS));
                }
                self.begin_zone_music(intro, kit, now);
            }
        }

        // ---- the bed: `0x460bd0` ranks the ghost bed over the zone's ----
        let desired = if self.vol.ghost {
            self.kits.ghost_bed().unwrap_or(0)
        } else {
            self.area
                .and_then(|id| self.areas.resolve(id))
                .and_then(|r| r.ambience.map(|a| a.kits[PHASE]))
                .unwrap_or(0)
        };
        if desired != self.ambience_kit {
            self.swap_ambience(desired, now);
        }

        // ---- the music transport: reap a finished track, then start the due one ----
        if let Some(h) = &self.music {
            if h.state() == PlaybackState::Stopped {
                self.music = None;
                let row = self.zone_music;
                self.next_track_at = self.next_track_time(row, now, self.vol.no_delay);
            }
        }
        if self.next_track_at.is_some_and(|t| now >= t) && self.music.is_none() {
            let kit = self.zone_row(self.zone_music).map(|m| m.sounds[PHASE]);
            self.pump_zone_music(kit, now);
        }
        // The Lua slot is not opened with a loop count (`0x7a5592`'s `-1`), so the scheduler
        // restarts it at its natural end.
        if let Some(h) = &self.lua_music {
            if h.state() == PlaybackState::Stopped {
                self.lua_music = None;
            }
        }
        if self.lua_music.is_none() && self.lua_music_path.is_some() {
            let path = self.lua_music_path.take().unwrap_or_default();
            self.open_lua_music(&path);
        }

        // ---- the per-frame volume feed: the sliders are live ----
        if let Some(h) = &mut self.music {
            let amp = self.vol.amp(Channel::Music) * self.music_kit_vol;
            h.set_volume(amp_to_db(amp), glide());
        }
        if let Some(h) = &mut self.lua_music {
            let amp = self.vol.amp(Channel::Music) * LUA_MUSIC_VOLUME;
            h.set_volume(amp_to_db(amp), glide());
        }
        let gain = self.ambience_gain(now);
        if let Some(h) = &mut self.ambience {
            let amp = self.vol.amp(Channel::Ambience) * self.ambience_kit_vol * gain;
            h.set_volume(amp_to_db(amp), glide());
        }
    }

    fn request(&mut self, r: Request, now: f64) {
        match r {
            Request::SoundKit(name) => self.play_kit(&name),
            Request::SoundFile(path) => self.play_file(&path),
            Request::PlayMusic(path) => self.set_lua_music(Some(&path), now),
            Request::StopMusic => self.set_lua_music(None, now),
        }
    }

    fn zone_row(&self, id: u32) -> Option<&ZoneMusicEntry> {
        if id == 0 {
            None
        } else {
            self.areas.zone_music(id)
        }
    }

    /// `0x4601f0`: when the next track starts after this one ends — the row's random per-phase
    /// silence, or `now` under `SoundZoneMusicNoDelay` (`0x42c010`), which removes only the
    /// same-zone gap. No row is no schedule.
    fn next_track_time(&mut self, row: u32, now: f64, no_delay: bool) -> Option<f64> {
        if row == 0 {
            return None;
        }
        if no_delay {
            return Some(now);
        }
        match self.zone_row(row).map(|m| (m.silence_min[PHASE], m.silence_max[PHASE])) {
            Some((min, max)) => Some(now + self.silence_ms(min, max) as f64 / 1000.0),
            None => Some(now + 6.0),
        }
    }

    /// A uniform interval in `[min, max]` ms.
    fn silence_ms(&mut self, min: u32, max: u32) -> u32 {
        if max > min {
            min + xorshift(&mut self.silence_rng) % (max - min + 1)
        } else {
            min
        }
    }

    /// A zone-music change's start: the intro if it is off cooldown, else the zone track now. A
    /// start that plays nothing leaves the pump armed (`0x460240` returns without writing the
    /// deadline), which is also how a phase with no kit waits for the other one.
    fn begin_zone_music(&mut self, intro: Option<u32>, kit: Option<u32>, now: f64) {
        self.next_track_at = None;
        if let Some(id) = intro {
            if let Some(row) = self.areas.intro(id) {
                let ZoneIntroEntry {
                    sound_id,
                    min_delay_minutes,
                    ..
                } = *row;
                let ok_at = self
                    .intro_last
                    .get(&id)
                    .map(|t| t + f64::from(min_delay_minutes) * 60.0);
                if sound_id != 0 && ok_at.is_none_or(|t| now >= t) && self.start_music(sound_id) {
                    self.intro_last.insert(id, now);
                    return;
                }
            }
        }
        let Some(kit) = kit else {
            return;
        };
        if kit == 0 || !self.start_music(kit) {
            self.next_track_at = Some(now);
        }
    }

    /// The due start (`0x460240`): a fresh weighted pick per attempt, the deadline spent only by a
    /// stream that opened.
    fn pump_zone_music(&mut self, kit: Option<u32>, now: f64) {
        match kit {
            None => self.next_track_at = None,
            Some(0) => {}
            Some(kit) => {
                if self.start_music(kit) {
                    self.next_track_at = None;
                } else {
                    self.next_track_at = Some(now);
                }
            }
        }
    }

    /// `0x460240` → `0x7a5dc0`: open a music kit on the zone slot, replacing what is there.
    fn start_music(&mut self, kit_id: u32) -> bool {
        if self.lua_music_path.is_some() {
            // A Lua track holds the override slot, so the zone pump refuses (`0x460050`).
            return false;
        }
        let Some((path, kit_vol)) = self.pick_stream(kit_id) else {
            return false;
        };
        let data = match self.open_stream(&path) {
            Ok(d) => d,
            Err(e) => return self.unplayable(&path, e),
        };
        let amp = self.vol.amp(Channel::Music) * kit_vol;
        match self.manager.play(data.volume(amp_to_db(amp))) {
            Ok(h) => {
                say(format!("audio: zone music {path}"));
                // One stream per slot: a dropped kira handle keeps playing, so the old one fades.
                if let Some(mut outgoing) = self.music.replace(h) {
                    outgoing.stop(fade(MUSIC_FADE_OUT_MS));
                }
                self.music_kit = kit_id;
                self.music_kit_vol = kit_vol;
                true
            }
            Err(e) => self.unplayable(&path, e.to_string()),
        }
    }

    /// Report a bad path once and answer that nothing started, as the reference retries silently.
    fn unplayable(&mut self, path: &str, error: String) -> bool {
        if self.unplayable.insert(path.to_owned()) {
            say(format!("audio: {path} — {error}"));
        }
        false
    }

    /// `0x45bb70`: the kit's file to play, with its base volume.
    fn pick_stream(&mut self, kit_id: u32) -> Option<(String, f32)> {
        let kit = self.kits.get(kit_id)?;
        if kit.files.is_empty() {
            return None;
        }
        let volume = kit.volume;
        let weights: Vec<u32> = kit.files.iter().map(|(_, w)| *w).collect();
        let pick = self.pick_variation(kit_id, &weights);
        let path = self.kits.get(kit_id)?.files[pick].0.clone();
        Some((path, volume))
    }

    /// The weighted pick over the kit's *remaining* pool, refilled when exhausted (`0x45bd40`).
    fn pick_variation(&mut self, kit: u32, weights: &[u32]) -> usize {
        if weights.len() == 1 {
            return 0;
        }
        let roll = xorshift(&mut self.pick_rng);
        let state = self.picks.entry(kit).or_insert_with(|| PickState {
            weights: weights.to_vec(),
            remaining: weights.to_vec(),
        });
        let mut total: u32 = state.remaining.iter().sum();
        if total == 0 {
            state.remaining.copy_from_slice(&state.weights);
            total = state.remaining.iter().sum();
        }
        if total == 0 {
            // All-zero weights are degenerate data: take the first.
            return 0;
        }
        let x = roll % total;
        let mut acc = 0u32;
        for (i, w) in state.remaining.iter().enumerate() {
            acc += w;
            if acc > x {
                state.remaining[i] -= 1;
                return i;
            }
        }
        state.remaining.len() - 1
    }

    /// Crossfade the looping bed to a new kit (0 is silence), `0x460b00` / `0x460bd0`.
    fn swap_ambience(&mut self, kit_id: u32, now: f64) {
        if kit_id == self.ambience_kit {
            return;
        }
        self.ambience_kit = kit_id;
        let pick = if kit_id != 0 {
            self.pick_stream(kit_id)
        } else {
            None
        };
        let Some((path, kit_vol)) = pick else {
            // Nothing opens, so the old bed goes now: the fade precedes the open (`0x460b31`).
            self.fade_out_bed();
            return;
        };
        let data = match self.open_stream(&path) {
            Ok(d) => d.loop_region(..),
            Err(e) => {
                self.unplayable(&path, e);
                self.fade_out_bed();
                return;
            }
        };
        self.fade_out_bed();
        // The bed comes in under the per-beat envelope rather than a handle ramp, because the
        // volume feed would override it (`0x460b76`).
        match self.manager.play(data.volume(amp_to_db(0.0))) {
            Ok(h) => {
                say(format!("audio: ambience {path}"));
                self.ambience = Some(h);
                self.ambience_kit_vol = kit_vol;
                self.ambience_fade_in = Some((now, AMBIENCE_FADE_MS as f64 / 1000.0));
            }
            Err(e) => {
                self.unplayable(&path, e.to_string());
            }
        }
    }

    /// Release the playing bed to fade out on the device.
    fn fade_out_bed(&mut self) {
        if let Some(mut h) = self.ambience.take() {
            h.stop(fade(AMBIENCE_FADE_MS));
        }
        self.ambience_fade_in = None;
    }

    /// The incoming leg of the bed's crossfade, in `[0, 1]`.
    fn ambience_gain(&mut self, now: f64) -> f32 {
        match self.ambience_fade_in {
            Some((start, dur)) => {
                let g = (((now - start) / dur) as f32).clamp(0.0, 1.0);
                if g >= 1.0 {
                    self.ambience_fade_in = None;
                }
                g
            }
            None => 1.0,
        }
    }

    /// `0x460450`: `PlayMusic(path)` on the override slot, `StopMusic()` with nothing. A path that
    /// does not resolve leaves the zone track playing.
    fn set_lua_music(&mut self, path: Option<&str>, now: f64) {
        if let Some(mut h) = self.lua_music.take() {
            h.stop(fade(MUSIC_FADE_OUT_MS));
        }
        self.lua_music_path = None;
        self.next_track_at = Some(now + LUA_MUSIC_SCHEDULE_SECS);
        let Some(path) = path else {
            say("audio: lua music stopped");
            return;
        };
        if self.open_lua_music(path) {
            // The zone track fades under it, the same `0x7a5a10(4.0f)` a zone change makes.
            if let Some(mut h) = self.music.take() {
                h.stop(fade(MUSIC_FADE_OUT_MS));
            }
        }
    }

    /// Open a Lua track by path and loop it (`0x4604a7` → `0x7a5620`'s looping arm).
    fn open_lua_music(&mut self, path: &str) -> bool {
        let data = match self.open_stream(path) {
            Ok(d) => d.loop_region(..),
            Err(e) => return self.unplayable(path, e),
        };
        let amp = self.vol.amp(Channel::Music) * LUA_MUSIC_VOLUME;
        match self.manager.play(data.volume(amp_to_db(amp))) {
            Ok(h) => {
                say(format!("audio: lua music {path}"));
                // Only the loop's reopen finds a stream here; the verb already faded the old one.
                if let Some(mut outgoing) = self.lua_music.replace(h) {
                    outgoing.stop(glide());
                }
                self.lua_music_path = Some(path.to_owned());
                true
            }
            Err(e) => self.unplayable(path, e.to_string()),
        }
    }

    /// A UI one-shot by `SoundEntries` name: the kit's weighted variation at the SFX gain.
    fn play_kit(&mut self, name: &str) {
        let Some(kit_id) = self.kits.by_name(name).map(|k| k.id) else {
            return;
        };
        let Some((path, kit_vol)) = self.pick_stream(kit_id) else {
            return;
        };
        let looping = self
            .kits
            .get(kit_id)
            .is_some_and(|k| k.flags & LOOPING != 0);
        let amp = self.vol.amp(Channel::Sfx) * kit_vol;
        match self.sfx_data(&path, amp, looping) {
            Ok(data) => {
                let _ = self.manager.play(data);
            }
            Err(e) => {
                self.unplayable(&path, e);
            }
        }
    }

    fn play_file(&mut self, path: &str) {
        let amp = self.vol.amp(Channel::Sfx);
        if let Ok(data) = self.sfx_data(path, amp, false) {
            let _ = self.manager.play(data);
        }
    }

    /// A decoded one-shot, from the cache when this path has played before.
    fn sfx_data(&mut self, path: &str, amp: f32, looping: bool) -> Result<StaticSoundData, String> {
        let key = path.to_ascii_lowercase();
        let data = match self.sfx_cache.get(&key) {
            Some(d) => d.clone(),
            None => {
                let bytes = self.read(path)?;
                let mut d = StaticSoundData::from_cursor(Cursor::new(bytes))
                    .map_err(|e| format!("{e}"))?;
                if looping {
                    d = d.loop_region(..);
                }
                if self.sfx_cache.len() >= SFX_CACHE_CAP {
                    self.sfx_cache.clear();
                }
                self.sfx_cache.insert(key, d.clone());
                d
            }
        };
        Ok(data.volume(amp_to_db(amp)))
    }

    /// The patch chain, then the loose files: an addon's own track lives under the Data directory,
    /// not in an MPQ.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        match self.chain.read_file(path) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                let loose = std::path::Path::new(&self.data_dir).join(path);
                std::fs::read(loose).map_err(|_| format!("{e:#}"))
            }
        }
    }

    /// Bytes to a kira stream; the header is read here, the rest on kira's decode thread.
    fn open_stream(&mut self, path: &str) -> Result<StreamingSoundData<FromFileError>, String> {
        let bytes = self.read(path)?;
        StreamingSoundData::from_cursor(Cursor::new(bytes)).map_err(|e| format!("{e}"))
    }

    /// Leaving the world: everything the soundscape held goes, so the next entry starts fresh.
    fn leave_world(&mut self) {
        if let Some(mut h) = self.music.take() {
            h.stop(fade(DECLOCK_MS));
        }
        if let Some(mut h) = self.lua_music.take() {
            h.stop(fade(DECLOCK_MS));
        }
        self.lua_music_path = None;
        if let Some(mut h) = self.ambience.take() {
            h.stop(fade(DECLOCK_MS));
        }
        self.ambience_fade_in = None;
        self.area = None;
        self.zone_music = 0;
        self.ambience_kit = 0;
        self.next_track_at = None;
    }
}

/// Linear amplitude to dB; at or under one 1/255 FMOD step is kira's silence floor.
fn amp_to_db(amp: f32) -> Decibels {
    if amp <= 1e-3 {
        Decibels::SILENCE
    } else {
        Decibels(20.0 * amp.log10())
    }
}

/// A ramp over `ms`, like the reference's constant per-tick volume decrement (`0x7a5a50`).
fn fade(ms: u64) -> Tween {
    Tween {
        duration: Duration::from_millis(ms),
        ..Default::default()
    }
}

/// The per-beat volume feed's ramp.
fn glide() -> Tween {
    fade(GLIDE_MS)
}

/// The client's own generator: the reference's weighted pick and silence interval both draw from
/// a 32-bit xorshift, as benilla-app does.
fn xorshift(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

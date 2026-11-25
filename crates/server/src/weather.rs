//! Server-authoritative weather: phase machine, persistence, precipitation and
//! deterministic lightning.
//!
//! The host owns exactly one [`WeatherState`] per world. It is advanced once per
//! simulation tick by [`Host::tick_weather`] and is mirrored to clients through
//! protocol v5 `WeatherSync`/`LightningStrike`; clients never advance or mutate
//! it.
//!
//! # State machine
//!
//! | Phase | Duration | Intensity target | Next phase |
//! | --- | --- | --- | --- |
//! | `clear` | 12,000–72,000 ticks | ramps to 0 | `precipitating` |
//! | `precipitating` | 3,000–12,000 ticks | ramps to 0.65–1.0 | 15% thunder, 85% clear |
//! | `thunder` | 3,600–12,000 ticks | ramps to 0.85–1.0 | 30% rain, 70% clear |
//!
//! Duration bounds are inclusive and chosen from
//! `splitmix64(seed ^ WEATHER_STREAM ^ epoch)`. The epoch starts at 0 and is
//! incremented on every transition, so a save/reload resumes the same stream.
//! Intensity is stored as integer permille and ramps by
//! [`WEATHER_RAMP_PERMILLE_PER_TICK`] (5) per tick: at most
//! [`WEATHER_RAMP_TICKS`] (200) ticks to any target. Fixed-point intensity keeps
//! saves and [`Host::state_hash`] byte/reload stable.
//!
//! `Host::set_weather_cycle(false)` (the gamerule equivalent) freezes the
//! machine for tests and admin pause; manual [`Host::set_weather_state`] still
//! works.
//!
//! # Persistence
//!
//! Weather and the world clock live in an additive `level.txt` v1 sidecar:
//!
//! ```text
//! version=1
//! time=123456
//! weather_phase=precipitating
//! weather_remaining=5000
//! weather_intensity=750
//! weather_epoch=7
//! ```
//!
//! A world without `level.txt` keeps its `player.txt` tick count and starts a
//! fresh deterministic clear phase; the first save writes `level.txt`, and from
//! then on its `time` overrides `player.txt` (which remains the 1.0/1.2
//! compatibility clock). Unknown keys are ignored, malformed files fall back
//! to the old clock with a warning, and a save/reload reproduces the phase,
//! remaining ticks, intensity and epoch exactly.
//!
//! # Precipitation
//!
//! [`Host::precipitation_at`] is a pure worldgen query (`Generator::biome_at`
//! plus the surface height): desert receives nothing, cold biomes snow, the
//! rest rain. It never generates a chunk. `Host::weather_factor` returns the
//! gameplay-light multiplier (`1.0` clear, `0.8` rain, `0.6` thunder) used by
//! crop growth, hostile spawning and fire checks; stored light is untouched.
//!
//! # Lightning
//!
//! During thunder, every tick rolls
//! `mix64(seed ^ WEATHER_STREAM ^ epoch ^ tick ^ LIGHTNING_ROLL_STREAM)`
//! against `intensity / 2,400` (intensity as a fraction, so a full storm
//! averages one strike per 2,400 ticks). A strike picks one entry from the
//! sorted, resident, precipitating, sky-exposed surface columns within
//! [`LIGHTNING_SEARCH_RADIUS`] blocks of an active player using a second hash
//! keyed by the same seed/epoch/tick and [`LIGHTNING_TARGET_STREAM`]; candidate
//! order and the event sequence are deterministic. It damages players and mobs
//! with a linear 10 HP falloff inside [`LIGHTNING_DAMAGE_RADIUS`] and may place
//! a fire block in the air above the struck solid block (subject to the fire
//! caps). The strike is queued as one [`LightningEvent`] for the network layer.

use std::collections::BTreeSet;
use std::path::Path;

use ob_math::{BlockPos, WORLD_MAX_Y, WorldVec, mix64};
use ob_protocol::{PrecipitationKind, WeatherPhase};

use crate::{Host, read_sidecar};

/// World-directory filename for the additive level/weather sidecar.
pub(crate) const LEVEL_FILE: &str = "level.txt";
/// Current `level.txt` schema version.
pub const LEVEL_VERSION: u32 = 1;
/// Domain separator for every weather hash and RNG draw.
pub const WEATHER_STREAM: u64 = 0x5745_4154_4845_5201;
/// Shortest clear phase, in ticks (10 minutes).
pub const CLEAR_MIN_TICKS: u32 = 12_000;
/// Longest clear phase, in ticks (60 minutes).
pub const CLEAR_MAX_TICKS: u32 = 72_000;
/// Shortest precipitation phase, in ticks (2.5 minutes).
pub const PRECIPITATING_MIN_TICKS: u32 = 3_000;
/// Longest precipitation phase, in ticks (10 minutes).
pub const PRECIPITATING_MAX_TICKS: u32 = 12_000;
/// Shortest thunder phase, in ticks (3 minutes).
pub const THUNDER_MIN_TICKS: u32 = 3_600;
/// Longest thunder phase, in ticks (10 minutes).
pub const THUNDER_MAX_TICKS: u32 = 12_000;
/// Fixed-point intensity step per tick (5 permille).
pub const WEATHER_RAMP_PERMILLE_PER_TICK: u16 = 5;
/// Ticks to ramp from zero to full intensity (200).
pub const WEATHER_RAMP_TICKS: u32 = 200;
/// Lowest rain intensity target, in permille.
pub const RAIN_INTENSITY_MIN_PERMILLE: u16 = 650;
/// Highest rain intensity target, in permille.
pub const RAIN_INTENSITY_MAX_PERMILLE: u16 = 1_000;
/// Lowest thunder intensity target, in permille.
pub const THUNDER_INTENSITY_MIN_PERMILLE: u16 = 850;
/// Highest thunder intensity target, in permille.
pub const THUNDER_INTENSITY_MAX_PERMILLE: u16 = 1_000;
/// Gameplay-light factor in clear weather.
pub const CLEAR_WEATHER_FACTOR: f32 = 1.0;
/// Gameplay-light factor while precipitating.
pub const RAIN_WEATHER_FACTOR: f32 = 0.8;
/// Gameplay-light factor during thunder.
pub const THUNDER_WEATHER_FACTOR: f32 = 0.6;
/// Lightning roll denominator: per-tick probability is `intensity / 2,400`.
pub const LIGHTNING_RATE_DENOMINATOR: u64 = 2_400;
/// Domain separator for the per-tick lightning probability roll.
pub const LIGHTNING_ROLL_STREAM: u64 = 0x9E37_79B9_7F4A_7C15;
/// Domain separator for lightning target selection among candidate columns.
pub const LIGHTNING_TARGET_STREAM: u64 = 0xD1B5_4A32_D192_ED03;
/// Horizontal radius around an active player in which strikes may land.
pub const LIGHTNING_SEARCH_RADIUS: i32 = 48;
/// Maximum lightning damage at zero distance, in health points.
pub const LIGHTNING_DAMAGE: f32 = 10.0;
/// Distance at which lightning damage reaches zero, in metres.
pub const LIGHTNING_DAMAGE_RADIUS: f64 = 3.0;

/// One authoritative weather state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeatherState {
    /// Current phase.
    pub phase: WeatherPhase,
    /// Ticks left in the phase (at least 1).
    pub remaining_ticks: u32,
    /// Intensity in permille (`0..=1_000`).
    pub intensity_permille: u16,
    /// Transition counter; incremented once per phase change and never reset
    /// by a save/reload.
    pub epoch: u64,
}

impl WeatherState {
    /// Creates a fresh clear phase for `seed`.
    ///
    /// The first clear duration is deterministic
    /// (`splitmix64(seed ^ WEATHER_STREAM)`), so two new worlds with the same
    /// seed see the same first storm.
    pub fn new(seed: u64) -> Self {
        let phase = WeatherPhase::Clear;
        let rng = mix64(seed ^ WEATHER_STREAM);
        Self {
            phase,
            remaining_ticks: duration_for(phase, rng),
            intensity_permille: 0,
            epoch: 0,
        }
    }

    /// Current intensity as a fraction `0.0..=1.0`.
    pub fn intensity(&self) -> f32 {
        f32::from(self.intensity_permille) / 1_000.0
    }

    /// Whether the phase is thunder.
    pub fn is_thunder(&self) -> bool {
        self.phase == WeatherPhase::Thunder
    }

    /// Whether the phase produces any precipitation.
    pub fn is_precipitating(&self) -> bool {
        self.phase != WeatherPhase::Clear
    }

    /// Light factor for the gameplay-light query.
    pub const fn light_factor(&self) -> f32 {
        match self.phase {
            WeatherPhase::Clear => CLEAR_WEATHER_FACTOR,
            WeatherPhase::Precipitating => RAIN_WEATHER_FACTOR,
            WeatherPhase::Thunder => THUNDER_WEATHER_FACTOR,
        }
    }

    /// Intensity target of the current epoch/phase in permille.
    pub fn target_permille(&self, seed: u64) -> u16 {
        target_permille(seed, self.epoch, self.phase)
    }

    /// Advances one tick. `enabled = false` freezes the machine completely.
    ///
    /// Intensity ramps toward the phase target before the remaining counter is
    /// decremented; on expiry the epoch is incremented, the next phase and its
    /// duration are drawn, and the intensity target changes for the next tick.
    pub fn tick(&mut self, seed: u64, enabled: bool) {
        if !enabled {
            return;
        }
        let target = self.target_permille(seed);
        if self.intensity_permille < target {
            self.intensity_permille = self
                .intensity_permille
                .saturating_add(WEATHER_RAMP_PERMILLE_PER_TICK)
                .min(target);
        } else if self.intensity_permille > target {
            self.intensity_permille = self
                .intensity_permille
                .saturating_sub(WEATHER_RAMP_PERMILLE_PER_TICK)
                .max(target);
        }
        self.remaining_ticks = self.remaining_ticks.saturating_sub(1);
        if self.remaining_ticks == 0 {
            self.transition(seed);
        }
    }

    /// Applies the next-phase draw for one transition.
    fn transition(&mut self, seed: u64) {
        self.epoch = self.epoch.saturating_add(1);
        let rng = mix64(seed ^ WEATHER_STREAM ^ self.epoch);
        let percent = (rng >> 24) % 100;
        self.phase = match self.phase {
            WeatherPhase::Clear => WeatherPhase::Precipitating,
            WeatherPhase::Precipitating => {
                if percent < 15 {
                    WeatherPhase::Thunder
                } else {
                    WeatherPhase::Clear
                }
            }
            WeatherPhase::Thunder => {
                if percent < 30 {
                    WeatherPhase::Precipitating
                } else {
                    WeatherPhase::Clear
                }
            }
        };
        self.remaining_ticks = duration_for(self.phase, rng);
        debug_assert!(self.remaining_ticks >= 1);
    }

    /// Forces a state for tests/admin commands, clamping every field.
    pub fn force(&mut self, phase: WeatherPhase, remaining_ticks: u32, intensity_permille: u16) {
        self.phase = phase;
        self.remaining_ticks = remaining_ticks.max(1);
        self.intensity_permille = intensity_permille.min(1_000);
    }
}

/// Duration of one phase, in ticks, drawn from `rng`.
fn duration_for(phase: WeatherPhase, rng: u64) -> u32 {
    match phase {
        WeatherPhase::Clear => CLEAR_MIN_TICKS + (rng % u64::from(CLEAR_MAX_TICKS - CLEAR_MIN_TICKS + 1)) as u32,
        WeatherPhase::Precipitating => {
            PRECIPITATING_MIN_TICKS
                + ((rng >> 16) % u64::from(PRECIPITATING_MAX_TICKS - PRECIPITATING_MIN_TICKS + 1)) as u32
        }
        WeatherPhase::Thunder => {
            THUNDER_MIN_TICKS + ((rng >> 32) % u64::from(THUNDER_MAX_TICKS - THUNDER_MIN_TICKS + 1)) as u32
        }
    }
}

/// Intensity target for one `(seed, epoch, phase)` in permille.
fn target_permille(seed: u64, epoch: u64, phase: WeatherPhase) -> u16 {
    let rng = mix64(seed ^ WEATHER_STREAM ^ epoch ^ 0xA5A5_5A5A_C3C3_3C3C);
    match phase {
        WeatherPhase::Clear => 0,
        WeatherPhase::Precipitating => {
            RAIN_INTENSITY_MIN_PERMILLE
                + (rng % u64::from(RAIN_INTENSITY_MAX_PERMILLE - RAIN_INTENSITY_MIN_PERMILLE + 1)) as u16
        }
        WeatherPhase::Thunder => {
            THUNDER_INTENSITY_MIN_PERMILLE
                + (rng % u64::from(THUNDER_INTENSITY_MAX_PERMILLE - THUNDER_INTENSITY_MIN_PERMILLE + 1)) as u16
        }
    }
}

/// Whether the lightning roll for one thunder tick succeeds.
///
/// The probability is exactly `intensity_permille / (2,400 * 1,000)`.
pub(crate) fn lightning_roll_succeeds(seed: u64, epoch: u64, tick: u64, intensity_permille: u16) -> bool {
    if intensity_permille == 0 {
        return false;
    }
    let roll =
        mix64(seed ^ WEATHER_STREAM ^ epoch ^ tick ^ LIGHTNING_ROLL_STREAM) % (LIGHTNING_RATE_DENOMINATOR * 1_000);
    roll < u64::from(intensity_permille)
}

/// One strike cue drained by the network layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightningEvent {
    /// Strike centre in world space (block centre above the struck block).
    pub pos: WorldVec,
    /// Struck solid block position.
    pub block: BlockPos,
    /// Whether the strike placed fire.
    pub ignited: bool,
    /// Whether clients should render a flash.
    pub flash: bool,
}

/// Serialises `level.txt` v1.
pub(crate) fn level_state(ticks: u64, weather: &WeatherState) -> String {
    format!(
        "version={LEVEL_VERSION}\ntime={ticks}\nweather_phase={}\nweather_remaining={}\nweather_intensity={}\nweather_epoch={}\n",
        weather.phase.name(),
        weather.remaining_ticks,
        weather.intensity_permille,
        weather.epoch,
    )
}

/// Parses `level.txt` v1. Returns `None` for an unsupported version or a
/// missing/invalid `time` key; individual weather keys fall back to a fresh
/// clear state instead of sacrificing the clock.
pub(crate) fn parse_level(text: &str, seed: u64) -> Option<(WeatherState, u64)> {
    let mut version = None;
    let mut time = None;
    let mut phase = None;
    let mut remaining = None;
    let mut intensity = None;
    let mut epoch = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        match key.trim() {
            "version" => version = value.trim().parse::<u32>().ok(),
            "time" => time = value.trim().parse::<u64>().ok(),
            "weather_phase" => phase = WeatherPhase::from_name(value.trim()),
            "weather_remaining" => remaining = value.trim().parse::<u32>().ok(),
            "weather_intensity" => intensity = value.trim().parse::<u16>().ok(),
            "weather_epoch" => epoch = value.trim().parse::<u64>().ok(),
            _ => {}
        }
    }
    if version != Some(LEVEL_VERSION) {
        return None;
    }
    let time = time?;
    let mut weather = WeatherState::new(seed);
    // A missing or unknown phase invalidates the whole weather block (but not
    // the clock): start from the deterministic fresh clear state instead of
    // attaching a bogus remaining/intensity pair to Clear. With a valid phase,
    // each missing field falls back to a coherent default for that phase.
    if let Some(phase) = phase {
        let epoch = epoch.unwrap_or(0);
        weather.phase = phase;
        weather.epoch = epoch;
        weather.remaining_ticks = remaining
            .filter(|remaining| *remaining > 0)
            .unwrap_or_else(|| duration_for(phase, mix64(seed ^ WEATHER_STREAM ^ epoch)));
        weather.intensity_permille = intensity.unwrap_or(0).min(1_000);
    }
    Some((weather, time))
}

/// Loads `level.txt` if present and valid.
///
/// A missing file returns the deterministic fresh weather and `fallback_ticks`
/// (`player.txt` time) so 1.4 and older worlds keep their clock. Malformed files
/// log a warning and take the same path.
pub(crate) fn load_level(world_dir: &Path, seed: u64, fallback_ticks: u64) -> (WeatherState, u64, bool) {
    let path = world_dir.join(LEVEL_FILE);
    match read_sidecar(&path) {
        Ok(text) => match parse_level(&text, seed) {
            Some((weather, ticks)) => (weather, ticks, true),
            None => {
                log::warn!(
                    "ignoring malformed or unsupported level state in {}; keeping the player clock",
                    path.display()
                );
                (WeatherState::new(seed), fallback_ticks, false)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (WeatherState::new(seed), fallback_ticks, false),
        Err(error) => {
            log::warn!("could not read {}: {error}; keeping the player clock", path.display());
            (WeatherState::new(seed), fallback_ticks, false)
        }
    }
}

impl Host {
    /// The authoritative weather state.
    pub fn weather(&self) -> &WeatherState {
        &self.weather
    }

    /// Current weather phase.
    pub fn weather_phase(&self) -> WeatherPhase {
        self.weather.phase
    }

    /// Current intensity in `0.0..=1.0`.
    pub fn weather_intensity(&self) -> f32 {
        self.weather.intensity()
    }

    /// Ticks left in the current phase.
    pub fn weather_remaining_ticks(&self) -> u32 {
        self.weather.remaining_ticks
    }

    /// Monotonic storm epoch (transitions since world creation).
    pub fn storm_epoch(&self) -> u64 {
        self.weather.epoch
    }

    /// Whether the current phase is thunder.
    pub fn is_thunder(&self) -> bool {
        self.weather.is_thunder()
    }

    /// Whether the current phase produces precipitation.
    pub fn is_precipitating(&self) -> bool {
        self.weather.is_precipitating()
    }

    /// Gameplay-light multiplier (`1.0` clear, `0.8` rain, `0.6` thunder).
    ///
    /// This is the value passed to [`ob_world::World::gameplay_light`] by crops
    /// and hostile spawning. It never mutates stored light.
    pub fn weather_factor(&self) -> f32 {
        self.weather.light_factor()
    }

    /// Whether weather transitions/ramp are enabled (`do_weather_cycle`).
    pub fn weather_cycle_enabled(&self) -> bool {
        self.weather_cycle
    }

    /// Enables or freezes the weather machine (host-config equivalent of the
    /// `do_weather_cycle` gamerule). Freezing keeps the current phase/intensity.
    pub fn set_weather_cycle(&mut self, enabled: bool) {
        self.weather_cycle = enabled;
    }

    /// Forces a weather state for tests/admin commands. Remaining ticks are
    /// clamped to at least one and intensity to `0..=1`.
    pub fn set_weather_state(&mut self, phase: WeatherPhase, remaining_ticks: u32, intensity_permille: u16) {
        self.weather.force(phase, remaining_ticks, intensity_permille);
        self.mark_player_dirty();
    }

    /// Biome name at a column, without generating a chunk.
    pub fn biome_name_at(&self, x: i32, z: i32) -> &'static str {
        self.generator.biome_at(x, z).name()
    }

    /// Precipitation at a column for the current phase, without generating a
    /// chunk. Clear weather is always [`PrecipitationKind::None`].
    pub fn precipitation_at(&self, x: i32, z: i32) -> PrecipitationKind {
        if !self.weather.is_precipitating() {
            return PrecipitationKind::None;
        }
        match self.generator.precipitation_at(x, z) {
            ob_worldgen::Precipitation::None => PrecipitationKind::None,
            ob_worldgen::Precipitation::Rain => PrecipitationKind::Rain,
            ob_worldgen::Precipitation::Snow => PrecipitationKind::Snow,
        }
    }

    /// Precipitation at the column containing `pos`.
    pub fn precipitation_at_pos(&self, pos: BlockPos) -> PrecipitationKind {
        self.precipitation_at(pos.x, pos.z)
    }

    /// Whether rain reaches `pos`: the phase precipitates, the column is wet
    /// (rain, not snow/dry) and the block sees open sky.
    pub fn rain_reaches(&self, pos: BlockPos) -> bool {
        self.precipitation_at_pos(pos) == PrecipitationKind::Rain && self.sky_exposed(pos)
    }

    /// Whether a block sees open sky (used by rain hydration/extinguishing).
    ///
    /// This asks the column heightmap, not the derived light field: a block at
    /// or above the highest non-air cell is exposed, while any roof, leaf canopy
    /// or overhang above it is not. Using the heightmap keeps the rule stable
    /// while a relight is queued and avoids treating fire's own emission as sky.
    pub fn sky_exposed(&self, pos: BlockPos) -> bool {
        pos.in_world()
            && self.world.chunk(pos.chunk()).is_some()
            && self.world.height_at(pos.x, pos.z).is_some_and(|height| pos.y >= height)
    }

    /// Advances the weather machine and resolves lightning for this tick.
    ///
    /// `Host::tick` and `Host::tick_network` call this after incrementing the
    /// world clock and before crops/fire. Exposed for unit tests that need to
    /// advance weather without players or mobs.
    pub fn tick_weather(&mut self) {
        let seed = self.world.seed();
        self.weather.tick(seed, self.weather_cycle);
        self.tick_lightning();
    }

    /// Drains strike cues produced since the last call.
    pub fn take_lightning_events(&mut self) -> Vec<LightningEvent> {
        std::mem::take(&mut self.lightning_events)
    }

    /// Forces one strike on an already-validated surface `top` block.
    ///
    /// Returns the event, or `None` when `top` is not a solid, non-fluid,
    /// sky-exposed surface block with air above. This is the shared effect path
    /// used by the automatic scheduler and by tests/admin commands.
    pub fn force_lightning_at(&mut self, top: BlockPos) -> Option<LightningEvent> {
        self.strike_lightning_at(top)
    }

    /// Deterministic lightning scheduler for one thunder tick.
    fn tick_lightning(&mut self) {
        if !self.weather.is_thunder() {
            return;
        }
        let seed = self.world.seed();
        if !lightning_roll_succeeds(
            seed,
            self.weather.epoch,
            self.tick_count,
            self.weather.intensity_permille,
        ) {
            return;
        }
        let candidates = self.lightning_candidates();
        if candidates.is_empty() {
            return;
        }
        let index = (mix64(seed ^ WEATHER_STREAM ^ self.weather.epoch ^ self.tick_count ^ LIGHTNING_TARGET_STREAM)
            % candidates.len() as u64) as usize;
        self.strike_lightning_at(candidates[index]);
    }

    /// Sorted resident, precipitating, sky-exposed surface columns near active
    /// players.
    fn lightning_candidates(&self) -> Vec<BlockPos> {
        let mut columns: BTreeSet<(i32, i32)> = BTreeSet::new();
        if self.network_players.is_empty() {
            let center = self.player.pos;
            add_lightning_columns(&mut columns, center);
        } else {
            for player in self.network_players.values() {
                add_lightning_columns(&mut columns, player.pos);
            }
        }
        let mut candidates = Vec::new();
        for (x, z) in columns {
            if self.precipitation_at(x, z) == PrecipitationKind::None {
                continue;
            }
            if self.world.chunk(BlockPos::new(x, 0, z).chunk()).is_none() {
                continue;
            }
            let Some(height) = self.world.height_at(x, z) else {
                continue;
            };
            if height < ob_math::WORLD_MIN_Y || height + 1 >= WORLD_MAX_Y {
                continue;
            }
            let top = BlockPos::new(x, height, z);
            let above = BlockPos::new(x, height + 1, z);
            let top_block = self.world.get_block(top);
            if top_block == ob_content::BlockId::AIR
                || !self.registry.block(top_block).solid
                || self.registry.block(top_block).fluid
            {
                continue;
            }
            if self.world.get_block(above) != ob_content::BlockId::AIR {
                continue;
            }
            // The heightmap is authoritative for "nothing above"; the sky-light
            // check additionally rejects overhangs/roofs inside the column.
            if self.world.sky_light(above) == 0 {
                continue;
            }
            candidates.push(top);
        }
        candidates
    }

    /// Applies one validated strike: damage falloff, optional ignition and one
    /// event. Returns `None` when `top` is not a valid strike surface.
    fn strike_lightning_at(&mut self, top: BlockPos) -> Option<LightningEvent> {
        if !top.in_world() {
            return None;
        }
        let above = BlockPos::new(top.x, top.y + 1, top.z);
        if !above.in_world() || self.world.chunk(top.chunk()).is_none() {
            return None;
        }
        let top_block = self.world.get_block(top);
        if top_block == ob_content::BlockId::AIR || !self.registry.block(top_block).solid {
            return None;
        }
        if self.world.get_block(above) != ob_content::BlockId::AIR {
            return None;
        }
        let center = WorldVec::new(f64::from(top.x) + 0.5, f64::from(above.y), f64::from(top.z) + 0.5);
        // Damage players first (deterministic id order for network players).
        if self.network_players.is_empty() {
            if !self.player.dead && self.game_mode == crate::GameMode::Survival {
                let distance = self.player.pos.distance(center);
                let damage = lightning_damage_at(distance);
                if damage > 0.0 {
                    self.damage_with_outcome(damage);
                }
            }
        } else {
            let ids: Vec<u64> = self.network_players.keys().copied().collect();
            for id in ids {
                let Some(player) = self.network_players.get(&id) else {
                    continue;
                };
                let distance = player.pos.distance(center);
                let damage = lightning_damage_at(distance);
                if damage > 0.0 {
                    self.damage_network_player(id, damage, "lightning");
                }
            }
        }
        // Mobs: direct health loss; a lightning death drops nothing (documented
        // in the weather plan). Removal is mirrored to the network layer.
        let mut dead_mobs = Vec::new();
        for entity in &mut self.mobs {
            let distance = entity.mob.pos.distance(center);
            let damage = lightning_damage_at(distance);
            if damage > 0.0 {
                entity.mob.health = (entity.mob.health - damage).max(0.0);
                entity.mob.hurt_timer = ob_sim::MOB_HURT_FLEE_TIME;
                if entity.mob.health <= 0.0 {
                    dead_mobs.push(entity.id);
                }
            }
        }
        if !dead_mobs.is_empty() {
            self.mobs.retain(|entity| !dead_mobs.contains(&entity.id));
            for id in dead_mobs {
                self.network_entity_removals.push(crate::net_players::RemovedEntity {
                    id,
                    item: false,
                    reason: ob_protocol::DespawnReason::Despawned,
                });
            }
        }
        let ignited = self.ignite_fire(above);
        let event = LightningEvent {
            pos: center,
            block: top,
            ignited,
            flash: true,
        };
        self.lightning_events.push(event);
        Some(event)
    }
}

/// Adds every column inside the lightning search circle to `columns`.
fn add_lightning_columns(columns: &mut BTreeSet<(i32, i32)>, center: WorldVec) {
    let center_x = center.x.floor() as i32;
    let center_z = center.z.floor() as i32;
    let radius = LIGHTNING_SEARCH_RADIUS;
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dz * dz > radius * radius {
                continue;
            }
            columns.insert((center_x + dx, center_z + dz));
        }
    }
}

/// Linear lightning damage falloff: 10 HP at zero distance, 0 at 3 m.
pub(crate) fn lightning_damage_at(distance: f64) -> f32 {
    if !distance.is_finite() || distance >= LIGHTNING_DAMAGE_RADIUS {
        return 0.0;
    }
    let fraction = 1.0 - distance / LIGHTNING_DAMAGE_RADIUS;
    (f64::from(LIGHTNING_DAMAGE) * fraction) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_a_deterministic_clear_phase() {
        let first = WeatherState::new(42);
        let second = WeatherState::new(42);
        let other = WeatherState::new(43);
        assert_eq!(first, second);
        assert_eq!(first.phase, WeatherPhase::Clear);
        assert_eq!(first.epoch, 0);
        assert_eq!(first.intensity_permille, 0);
        assert!((CLEAR_MIN_TICKS..=CLEAR_MAX_TICKS).contains(&first.remaining_ticks));
        assert_ne!(first.remaining_ticks, other.remaining_ticks);
    }

    #[test]
    fn phases_stay_inside_their_documented_duration_and_probability_bands() {
        let seed = 42;
        let mut state = WeatherState::new(seed);
        let mut transitions: Vec<(WeatherPhase, WeatherPhase, u32, u16)> = Vec::new();
        let mut previous = state.phase;
        for _ in 0..2_000_000 {
            state.tick(seed, true);
            if state.phase != previous {
                transitions.push((previous, state.phase, state.remaining_ticks, state.intensity_permille));
                previous = state.phase;
            }
        }
        assert!(
            transitions.len() > 30,
            "expected many transitions, saw {}",
            transitions.len()
        );
        for (from, to, duration, intensity) in &transitions {
            // `duration` belongs to the destination phase entered by this
            // transition.
            match to {
                WeatherPhase::Clear => assert!((CLEAR_MIN_TICKS..=CLEAR_MAX_TICKS).contains(duration)),
                WeatherPhase::Precipitating => {
                    assert!((PRECIPITATING_MIN_TICKS..=PRECIPITATING_MAX_TICKS).contains(duration))
                }
                WeatherPhase::Thunder => assert!((THUNDER_MIN_TICKS..=THUNDER_MAX_TICKS).contains(duration)),
            }
            match from {
                WeatherPhase::Clear => assert_eq!(*to, WeatherPhase::Precipitating),
                WeatherPhase::Precipitating => assert!(matches!(to, WeatherPhase::Thunder | WeatherPhase::Clear)),
                WeatherPhase::Thunder => assert!(matches!(to, WeatherPhase::Precipitating | WeatherPhase::Clear)),
            }
            assert!(*intensity <= 1_000);
        }
        // Transition probability bands, sampled over the deterministic stream.
        let precip_to_thunder = transitions
            .iter()
            .filter(|(from, to, _, _)| *from == WeatherPhase::Precipitating && *to == WeatherPhase::Thunder)
            .count();
        let precip_total = transitions
            .iter()
            .filter(|(from, _, _, _)| *from == WeatherPhase::Precipitating)
            .count();
        let thunder_to_rain = transitions
            .iter()
            .filter(|(from, to, _, _)| *from == WeatherPhase::Thunder && *to == WeatherPhase::Precipitating)
            .count();
        let thunder_total = transitions
            .iter()
            .filter(|(from, _, _, _)| *from == WeatherPhase::Thunder)
            .count();
        if precip_total >= 40 {
            let share = precip_to_thunder as f64 / precip_total as f64;
            assert!((0.08..=0.23).contains(&share), "rain->thunder share {share}");
        }
        if thunder_total >= 40 {
            let share = thunder_to_rain as f64 / thunder_total as f64;
            assert!((0.21..=0.40).contains(&share), "thunder->rain share {share}");
        }
    }

    #[test]
    fn intensity_ramps_in_about_two_hundred_ticks_and_freezing_holds_state() {
        let seed = 7;
        let mut state = WeatherState::new(seed);
        state.force(WeatherPhase::Thunder, 10_000, 0);
        let target = state.target_permille(seed);
        assert!((THUNDER_INTENSITY_MIN_PERMILLE..=THUNDER_INTENSITY_MAX_PERMILLE).contains(&target));
        for tick in 0..WEATHER_RAMP_TICKS {
            state.tick(seed, true);
            if tick + 1 == WEATHER_RAMP_TICKS {
                assert_eq!(state.intensity_permille, target);
            }
        }
        let frozen = state.clone();
        for _ in 0..1_000 {
            state.tick(seed, false);
        }
        assert_eq!(state, frozen);
        // Clear ramps back down over at most 200 ticks.
        let mut state = WeatherState::new(seed);
        state.force(WeatherPhase::Clear, 10_000, 1_000);
        for _ in 0..WEATHER_RAMP_TICKS {
            state.tick(seed, true);
        }
        assert_eq!(state.intensity_permille, 0);
    }

    #[test]
    fn light_factor_and_precipitation_predicates_follow_the_phase() {
        let mut state = WeatherState::new(1);
        assert_eq!(state.light_factor(), 1.0);
        assert!(!state.is_precipitating());
        state.force(WeatherPhase::Precipitating, 100, 700);
        assert_eq!(state.light_factor(), 0.8);
        assert!(state.is_precipitating());
        assert!(!state.is_thunder());
        state.force(WeatherPhase::Thunder, 100, 900);
        assert_eq!(state.light_factor(), 0.6);
        assert!(state.is_thunder());
        assert!(state.is_precipitating());
    }

    #[test]
    fn level_sidecar_round_trips_and_falls_back_safely() {
        let seed = 42;
        let mut weather = WeatherState::new(seed);
        weather.force(WeatherPhase::Precipitating, 5_432, 733);
        weather.epoch = 9;
        let text = level_state(123_456, &weather);
        assert!(text.contains("version=1"));
        assert!(text.contains("time=123456"));
        let parsed = parse_level(&text, seed).expect("level parses");
        assert_eq!(parsed.0, weather);
        assert_eq!(parsed.1, 123_456);
        // Missing weather keys keep the clock and default to clear.
        let minimal = "version=1\ntime=77\n";
        let parsed = parse_level(minimal, seed).expect("clock-only level parses");
        assert_eq!(parsed.1, 77);
        assert_eq!(parsed.0.phase, WeatherPhase::Clear);
        // A valid phase with missing weather fields gets coherent defaults:
        // a duration inside its own band and the ramp start at zero.
        let partial = parse_level("version=1\ntime=88\nweather_phase=thunder\n", seed).expect("partial level parses");
        assert_eq!(partial.1, 88);
        assert_eq!(partial.0.phase, WeatherPhase::Thunder);
        assert_eq!(partial.0.epoch, 0);
        assert_eq!(partial.0.intensity_permille, 0);
        assert!((THUNDER_MIN_TICKS..=THUNDER_MAX_TICKS).contains(&partial.0.remaining_ticks));
        // An unknown phase invalidates the weather block but keeps the clock;
        // the bogus remaining/intensity must not leak into Clear.
        let bogus = parse_level(
            "version=1\ntime=89\nweather_phase=hail\nweather_remaining=999\nweather_intensity=900\n",
            seed,
        )
        .expect("unknown phase keeps the clock");
        assert_eq!(bogus.1, 89);
        assert_eq!(bogus.0, WeatherState::new(seed));
        // Wrong version / malformed time / junk lines are rejected.
        assert!(parse_level("version=2\ntime=1\n", seed).is_none());
        assert!(parse_level("version=1\n", seed).is_none());
        assert!(parse_level("version=1\ntime=abc\n", seed).is_none());
        assert!(parse_level("version=1\ntime=1\nbroken\n", seed).is_none());
        // Unknown keys survive a parse and out-of-range intensity clamps.
        let extended = format!("{text}future_key=42\nweather_intensity=1500\n");
        let parsed = parse_level(&extended, seed).expect("extended level parses");
        assert_eq!(parsed.0.intensity_permille, 1_000);
    }

    #[test]
    fn lightning_roll_is_deterministic_and_scales_with_intensity() {
        let mut count = 0u32;
        let ticks = 240_000u64;
        for tick in 0..ticks {
            if lightning_roll_succeeds(42, 3, tick, 1_000) {
                count += 1;
            }
        }
        let rate = f64::from(count) / ticks as f64;
        let expected = 1.0 / 2_400.0;
        assert!(
            (rate - expected).abs() < expected * 0.12,
            "full-intensity rate {rate} is far from {expected}"
        );
        assert!(!lightning_roll_succeeds(42, 3, 17, 0));
        for tick in 0..100 {
            assert_eq!(
                lightning_roll_succeeds(42, 3, tick, 500),
                lightning_roll_succeeds(42, 3, tick, 500)
            );
        }
        // Half intensity has half the rate within sampling tolerance.
        let mut half = 0u32;
        for tick in 0..ticks {
            if lightning_roll_succeeds(7, 11, tick, 500) {
                half += 1;
            }
        }
        let half_rate = f64::from(half) / ticks as f64;
        assert!(
            (half_rate - expected / 2.0).abs() < expected * 0.12,
            "half-intensity rate {half_rate} is far from {}",
            expected / 2.0
        );
        assert_eq!(lightning_damage_at(0.0), LIGHTNING_DAMAGE);
        assert!(lightning_damage_at(1.5) < LIGHTNING_DAMAGE);
        assert_eq!(lightning_damage_at(LIGHTNING_DAMAGE_RADIUS), 0.0);
        assert_eq!(lightning_damage_at(9.0), 0.0);
    }
}

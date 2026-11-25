//! Client-side weather presentation: authoritative state to visuals and audio.
//!
//! Everything in this module is a pure function of the server's weather
//! messages, the local camera and elapsed time. The client never advances the
//! weather machine or decides that lightning should strike; it only renders the
//! state it was sent and schedules the corresponding presentation cues.
//!
//! Responsibilities:
//!
//! * [`WeatherSnapshot`] mirrors `WeatherSync` (phase, intensity, remaining,
//!   epoch, precipitation, biome, gameplay factor) with sanitised scalars.
//! * [`WeatherPresentation`] owns the local lightning flash envelope, camera
//!   shake, speed-of-sound thunder queue and ambient audio targets.
//! * [`WeatherHud`] formats the compact HUD chip and the detailed F3 lines.
//! * [`sky_exposure`] and [`nearby_fires`] are read-only world queries the
//!   renderer and audio mixer need (indoor rain suppression and fire crackle).
//!
//! The module deliberately does not touch sockets, winit or the GPU. That keeps
//! the state-to-visual/audio mapping testable without a server, adapter or
//! sound device.

use std::collections::VecDeque;

use ob_audio::{ClipCue, ClipId, explosion_cue, rain_intensity_gains, thunder_cue, world_pan};
use ob_content::Registry;
use ob_math::{BlockPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
use ob_protocol::{PrecipitationKind, ServerMessage, WeatherPhase};
use ob_render::{UiFrame, WeatherPrecipitation, WeatherVisual};
use ob_world::World;

/// Speed of sound in air at 20 °C, metres per second.
pub const SPEED_OF_SOUND: f32 = 343.0;
/// Maximum number of queued thunder claps. A strike every 2,400 ticks can never
/// queue this many, but a hostile server must not grow the queue.
pub const MAX_PENDING_THUNDER: usize = 16;
/// Flash envelope decay rate, per second.
pub const FLASH_DECAY: f32 = 6.5;
/// Camera-shake decay rate, per second.
pub const SHAKE_DECAY: f32 = 5.0;
/// Radius in blocks in which the client counts burning blocks for the fire bed.
pub const FIRE_SCAN_RADIUS: i32 = 8;

/// Authoritative weather state for the local player column.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherSnapshot {
    /// Phase name (`clear`, `precipitating`, `thunder`).
    pub phase: WeatherPhase,
    /// Ramp intensity `0..=1`.
    pub intensity: f32,
    /// Ticks left in the phase.
    pub remaining_ticks: u32,
    /// Storm epoch, incremented on every phase transition.
    pub epoch: u64,
    /// Precipitation at the local column.
    pub precipitation: PrecipitationKind,
    /// Worldgen biome name at the local column.
    pub biome: String,
    /// Gameplay-light factor (`1.0`, `0.8`, `0.6`).
    pub weather_factor: f32,
}

impl Default for WeatherSnapshot {
    fn default() -> Self {
        Self::clear()
    }
}

impl WeatherSnapshot {
    /// The clear-weather default used before the first sync and after a reset.
    pub fn clear() -> Self {
        Self {
            phase: WeatherPhase::Clear,
            intensity: 0.0,
            remaining_ticks: 0,
            epoch: 0,
            precipitation: PrecipitationKind::None,
            biome: String::new(),
            weather_factor: 1.0,
        }
    }

    /// Builds a sanitised snapshot from wire fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        phase: WeatherPhase,
        intensity: f64,
        remaining_ticks: u32,
        epoch: u64,
        precipitation: PrecipitationKind,
        biome: impl Into<String>,
        weather_factor: f64,
    ) -> Self {
        let intensity = if intensity.is_finite() {
            intensity.clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let weather_factor = if weather_factor.is_finite() {
            weather_factor.clamp(0.0, 1.0) as f32
        } else {
            1.0
        };
        let mut biome = biome.into();
        if biome.len() > 64 {
            biome.truncate(64);
        }
        Self {
            phase,
            intensity,
            remaining_ticks,
            epoch,
            precipitation,
            biome,
            weather_factor,
        }
    }

    /// Applies a `weather_sync` message. Non-weather messages return `None`.
    pub fn from_message(message: &ServerMessage) -> Option<Self> {
        let ServerMessage::WeatherSync {
            phase,
            intensity,
            remaining_ticks,
            epoch,
            precipitation,
            biome,
            weather_factor,
        } = message
        else {
            return None;
        };
        Some(Self::new(
            *phase,
            *intensity,
            *remaining_ticks,
            *epoch,
            *precipitation,
            biome.clone(),
            *weather_factor,
        ))
    }

    /// Whether precipitation is falling at this column.
    pub fn is_precipitating(&self) -> bool {
        self.phase != WeatherPhase::Clear && self.precipitation.is_some()
    }

    /// Whether the phase is a thunderstorm.
    pub fn is_thunder(&self) -> bool {
        self.phase == WeatherPhase::Thunder
    }

    /// Storm weight for [`WeatherVisual`]: `1` in thunder, else `0`.
    pub fn storm_weight(&self) -> f32 {
        if self.is_thunder() { 1.0 } else { 0.0 }
    }

    /// Precipitation mapped to the renderer enum.
    pub fn render_precipitation(&self) -> WeatherPrecipitation {
        match self.precipitation {
            PrecipitationKind::None => WeatherPrecipitation::None,
            PrecipitationKind::Rain => WeatherPrecipitation::Rain,
            PrecipitationKind::Snow => WeatherPrecipitation::Snow,
        }
    }

    /// Remaining phase time in whole seconds (ceil).
    pub fn remaining_seconds(&self) -> u32 {
        self.remaining_ticks.div_ceil(20)
    }

    /// Localised phase label for the HUD.
    pub fn phase_label(&self) -> &'static str {
        match self.phase {
            WeatherPhase::Clear => "Clear",
            WeatherPhase::Precipitating => match self.precipitation {
                PrecipitationKind::Snow => "Snow",
                PrecipitationKind::Rain => "Rain",
                PrecipitationKind::None => "Overcast",
            },
            WeatherPhase::Thunder => match self.precipitation {
                PrecipitationKind::Snow => "Thunder snow",
                _ => "Thunderstorm",
            },
        }
    }

    /// Renderer weather state with a caller-supplied flash envelope.
    pub fn visual(&self, flash: f32) -> WeatherVisual {
        WeatherVisual::new(self.intensity, self.storm_weight(), self.render_precipitation(), flash)
    }

    /// Applies the snapshot's precipitation to the local particle system.
    pub fn particles(&self, wind: [f32; 2], exposure: f32, ground_y: f32) -> ob_render::WeatherParticles {
        ob_render::WeatherParticles {
            precipitation: match self.precipitation {
                PrecipitationKind::None => ob_render::Precipitation::None,
                PrecipitationKind::Rain => ob_render::Precipitation::Rain,
                PrecipitationKind::Snow => ob_render::Precipitation::Snow,
            },
            intensity: if self.phase == WeatherPhase::Clear {
                0.0
            } else {
                self.intensity
            },
            wind,
            exposure,
            ground_y,
        }
    }
}

/// One thunder clap waiting for the speed-of-sound delay.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingThunder {
    due_in: f32,
    cue: ClipCue,
    pan: f32,
}

/// Immediate and delayed audio requested by the weather presentation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WeatherAudioFrame {
    /// Light rain bed target (`0..=1`).
    pub rain_light: f32,
    /// Heavy rain bed target (`0..=1`).
    pub rain_heavy: f32,
    /// Snow/wind bed target (`0..=1`).
    pub snow: f32,
    /// Fire crackle bed target (`0..=1`).
    pub fire: f32,
    /// One-shot cues due this update (thunder, explosion, lightning crack).
    pub cues: Vec<(ClipCue, Option<f32>)>,
    /// Whether any ambient loop target changed since the previous frame.
    pub loops_changed: bool,
}

impl WeatherAudioFrame {
    /// Adds a cue unless its gain is inaudible.
    fn push_cue(&mut self, cue: ClipCue, pan: f32) {
        if cue.gain > 0.0 {
            self.cues.push((cue, Some(pan.clamp(-1.0, 1.0))));
        }
    }
}

/// Client weather presentation state.
///
/// `update` is called once per frame. `apply_sync` is called when a
/// `weather_sync` arrives (or every frame from the cached snapshot; it is
/// idempotent). Lightning and explosion triggers are called from the matching
/// server events.
#[derive(Debug, Clone)]
pub struct WeatherPresentation {
    /// Latest authoritative state.
    pub snapshot: WeatherSnapshot,
    /// Local flash envelope `0..=1`.
    pub flash: f32,
    /// Local camera-shake amplitude in radians.
    pub shake: f32,
    /// Elapsed presentation time in seconds.
    pub elapsed: f32,
    /// Number of lightning events rendered.
    pub lightning_events: u64,
    /// Number of lightning events with a visible flash.
    pub flash_events: u64,
    /// Number of queued thunder claps (diagnostics).
    pub pending_thunder: usize,
    /// Whether the player is under cover (updated by the client each frame).
    pub sheltered: bool,
    /// Nearest fire count used for the crackle bed.
    pub fire_count: usize,
    /// Wind vector derived from the storm epoch, m/s.
    pub wind: [f32; 2],
    /// Last lightning world position for diagnostics.
    pub last_strike: Option<[f64; 3]>,
    /// Position of the most recent explosion, for HUD/debug.
    pub last_explosion: Option<[f64; 3]>,
    pending: VecDeque<PendingThunder>,
    previous_rain_light: f32,
    previous_rain_heavy: f32,
    previous_snow: f32,
    previous_fire: f32,
}

impl Default for WeatherPresentation {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherPresentation {
    /// Creates a clear-weather presentation with no pending events.
    pub fn new() -> Self {
        Self {
            snapshot: WeatherSnapshot::clear(),
            flash: 0.0,
            shake: 0.0,
            elapsed: 0.0,
            lightning_events: 0,
            flash_events: 0,
            pending_thunder: 0,
            sheltered: false,
            fire_count: 0,
            wind: [0.0, 0.0],
            last_strike: None,
            last_explosion: None,
            pending: VecDeque::new(),
            previous_rain_light: f32::NAN,
            previous_rain_heavy: f32::NAN,
            previous_snow: f32::NAN,
            previous_fire: f32::NAN,
        }
    }

    /// Resets to clear weather on disconnect/reconnect.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Applies an authoritative snapshot. Returns whether the phase/epoch
    /// changed (which is what triggers a weather-transition toast/sound).
    pub fn apply_snapshot(&mut self, snapshot: WeatherSnapshot) -> bool {
        let changed = snapshot.epoch != self.snapshot.epoch || snapshot.phase != self.snapshot.phase;
        self.snapshot = snapshot;
        self.wind = ob_render::weather_wind(
            self.snapshot.epoch,
            self.snapshot.intensity,
            self.snapshot.precipitation == PrecipitationKind::Snow,
        );
        changed
    }

    /// Clears the presentation when the server is unreachable.
    pub fn clear(&mut self) {
        self.reset();
    }

    /// Advances local envelopes and resolves due thunder.
    ///
    /// `nearby_fire` is the number of burning blocks around the player; the
    /// caller may pass `0` when audio is disabled.
    pub fn update(&mut self, dt: f32, nearby_fire: usize) -> WeatherAudioFrame {
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.25) } else { 0.0 };
        self.elapsed = (self.elapsed + dt).max(0.0);
        self.fire_count = nearby_fire;
        self.flash = (self.flash * (-FLASH_DECAY * dt).exp()).clamp(0.0, 1.0);
        self.shake = (self.shake * (-SHAKE_DECAY * dt).exp()).clamp(0.0, 1.0);
        if self.flash < 0.002 {
            self.flash = 0.0;
        }
        if self.shake < 0.0002 {
            self.shake = 0.0;
        }

        let mut frame = WeatherAudioFrame::default();
        // Ambient beds. Snow is near-silent except for the wind bed; rain
        // crossfades between the light and heavy beds by intensity.
        if self.snapshot.phase == WeatherPhase::Clear || !self.snapshot.is_precipitating() {
            frame.rain_light = 0.0;
            frame.rain_heavy = 0.0;
            frame.snow = 0.0;
        } else if self.snapshot.precipitation == PrecipitationKind::Snow {
            frame.snow = 0.55 * self.snapshot.intensity;
        } else {
            let (light, heavy) = rain_intensity_gains(self.snapshot.intensity);
            frame.rain_light = light;
            frame.rain_heavy = heavy;
            frame.snow = 0.0;
        }
        frame.fire = fire_loop_target(nearby_fire);
        frame.loops_changed = !same_target(self.previous_rain_light, frame.rain_light)
            || !same_target(self.previous_rain_heavy, frame.rain_heavy)
            || !same_target(self.previous_snow, frame.snow)
            || !same_target(self.previous_fire, frame.fire);
        self.previous_rain_light = frame.rain_light;
        self.previous_rain_heavy = frame.rain_heavy;
        self.previous_snow = frame.snow;
        self.previous_fire = frame.fire;

        // Thunder is delayed by distance / 343 s. Pop due cues in queue order.
        let mut index = 0;
        while index < self.pending.len() {
            let due = {
                let pending = &mut self.pending[index];
                pending.due_in -= dt;
                pending.due_in <= 0.0
            };
            if due {
                if let Some(pending) = self.pending.remove(index) {
                    frame.push_cue(pending.cue, pending.pan);
                }
            } else {
                index += 1;
            }
        }
        self.pending_thunder = self.pending.len();
        frame
    }

    /// Renders one lightning strike.
    ///
    /// Returns the immediate crack cue (if any) plus an optional delayed thunder
    /// cue that is queued internally and emitted by a later [`Self::update`].
    pub fn trigger_lightning(
        &mut self,
        pos: [f64; 3],
        flash: bool,
        listener: [f32; 3],
        yaw: f32,
    ) -> Vec<(ClipCue, Option<f32>)> {
        if !pos.iter().all(|value| value.is_finite()) {
            return Vec::new();
        }
        self.lightning_events = self.lightning_events.saturating_add(1);
        self.last_strike = Some(pos);
        if flash {
            self.flash_events = self.flash_events.saturating_add(1);
            self.flash = 1.0;
        }
        let strike = [pos[0] as f32, pos[1] as f32, pos[2] as f32];
        let distance = listener
            .iter()
            .zip(strike)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt();
        let pan = world_pan(listener, yaw, strike);
        let mut immediate = Vec::new();
        let cue = thunder_cue(distance);
        // Distant thunder is only a rumble; a close-but-far strike (< 96 m)
        // still has an audible crack now while the rumble arrives with the
        // sound delay.
        if cue.clip == ClipId::ThunderDistant && distance < 96.0 {
            immediate.push((
                ClipCue::new(ClipId::LightningCrack, 0.9, 1.0),
                Some(pan.clamp(-1.0, 1.0)),
            ));
        }
        if self.pending.len() < MAX_PENDING_THUNDER {
            self.pending.push_back(PendingThunder {
                due_in: distance / SPEED_OF_SOUND,
                cue,
                pan,
            });
            self.pending_thunder = self.pending.len();
        }
        // Nearby strikes shake the camera. The amplitude is tiny (radians) and
        // client-only; reduced-motion mode is applied by the caller.
        let proximity = (1.0 - distance / 64.0).clamp(0.0, 1.0);
        self.shake = self.shake.max(0.014 * proximity * proximity);
        immediate
    }

    /// Renders one explosion: immediate sound, flash and camera shake.
    pub fn trigger_explosion(
        &mut self,
        pos: [f64; 3],
        radius: f32,
        listener: [f32; 3],
        yaw: f32,
    ) -> Option<(ClipCue, Option<f32>)> {
        if !pos.iter().all(|value| value.is_finite()) {
            return None;
        }
        self.last_explosion = Some(pos);
        let blast = [pos[0] as f32, pos[1] as f32, pos[2] as f32];
        let distance = listener
            .iter()
            .zip(blast)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt();
        let radius = if radius.is_finite() { radius.max(0.0) } else { 3.0 };
        // A close blast briefly lifts the exposure and shakes the camera. It is
        // not as bright as lightning; the particles carry most of the read.
        let proximity = (1.0 - distance / (12.0 + radius * 3.0)).clamp(0.0, 1.0);
        self.flash = self.flash.max(0.22 * proximity);
        self.shake = self.shake.max(0.018 * proximity * proximity);
        let cue = explosion_cue(distance);
        if cue.gain <= 0.0 {
            return None;
        }
        Some((cue, Some(world_pan(listener, yaw, blast).clamp(-1.0, 1.0))))
    }

    /// Local camera pitch shake in radians at a deterministic time sample.
    ///
    /// The caller adds this to `pitch`. `reduced_motion` disables it.
    pub fn shake_angle(&self, time: f32, reduced_motion: bool) -> f32 {
        if reduced_motion || self.shake <= 0.0 {
            return 0.0;
        }
        (time * 72.0).sin() * self.shake
    }

    /// Renderer state with the current flash.
    pub fn visual(&self) -> WeatherVisual {
        self.snapshot.visual(self.flash)
    }

    /// Whether any storm presentation is active (used for the HUD chip).
    pub fn is_active(&self) -> bool {
        self.snapshot.phase != WeatherPhase::Clear || self.flash > 0.0 || self.snapshot.precipitation.is_some()
    }

    /// Formats the compact HUD chip, or `None` when the weather is clear.
    pub fn hud(&self) -> Option<WeatherHud> {
        if !self.is_active() {
            return None;
        }
        Some(WeatherHud {
            phase: self.snapshot.phase_label(),
            precipitation: self.snapshot.precipitation.name(),
            biome: self.snapshot.biome.clone(),
            intensity: self.snapshot.intensity,
            remaining_seconds: self.snapshot.remaining_seconds(),
            weather_factor: self.snapshot.weather_factor,
            sheltered: self.sheltered,
            flash: self.flash > 0.15,
            lightning_events: self.lightning_events,
            flash_events: self.flash_events,
            pending_thunder: self.pending_thunder,
            fire_count: self.fire_count,
        })
    }

    /// Detailed F3 lines: phase, intensity, precipitation, remaining time,
    /// storm epoch/factor, lightning/flash counts, shelter and thunder queue.
    pub fn debug_lines(&self) -> Vec<String> {
        let snapshot = &self.snapshot;
        let cover = if self.sheltered { "sheltered" } else { "open sky" };
        let mut lines = vec![
            format!(
                "Weather: {} ({})  intensity {:.2}  factor {:.2}",
                snapshot.phase_label(),
                snapshot.phase.name(),
                snapshot.intensity,
                snapshot.weather_factor,
            ),
            format!(
                "Precip: {} at {}  {}  epoch {}",
                snapshot.precipitation.name(),
                if snapshot.biome.is_empty() {
                    "unknown biome"
                } else {
                    &snapshot.biome
                },
                format_remaining(snapshot.remaining_seconds()),
                snapshot.epoch,
            ),
            format!(
                "Storms: {} lightning / {} flashes / {} thunder queued  ({cover})",
                self.lightning_events, self.flash_events, self.pending_thunder,
            ),
        ];
        if self.fire_count > 0 {
            lines.push(format!(
                "Fire: {} burning block{} nearby",
                self.fire_count,
                if self.fire_count == 1 { "" } else { "s" }
            ));
        }
        lines
    }
}

/// Formatted weather HUD state.
#[derive(Debug, Clone, PartialEq)]
pub struct WeatherHud {
    /// Localised phase/precipitation label.
    pub phase: &'static str,
    /// Protocol precipitation tag.
    pub precipitation: &'static str,
    /// Biome at the player column.
    pub biome: String,
    /// Intensity `0..=1`.
    pub intensity: f32,
    /// Remaining phase seconds.
    pub remaining_seconds: u32,
    /// Gameplay-light factor.
    pub weather_factor: f32,
    /// Whether the player is sheltered.
    pub sheltered: bool,
    /// Whether the flash chip is lit.
    pub flash: bool,
    /// Rendered lightning events.
    pub lightning_events: u64,
    /// Rendered visible flashes.
    pub flash_events: u64,
    /// Thunder claps still scheduled.
    pub pending_thunder: usize,
    /// Nearby burning blocks.
    pub fire_count: usize,
}

impl WeatherHud {
    /// Compact text shown above the hotbar.
    pub fn compact_line(&self) -> String {
        let shelter = if self.sheltered { "  sheltered" } else { "" };
        let flash = if self.flash { "  FLASH" } else { "" };
        format!(
            "{} {}%  {} left  factor {:.2}{shelter}{flash}",
            self.phase,
            (self.intensity * 100.0).round() as u32,
            format_remaining(self.remaining_seconds),
            self.weather_factor,
        )
    }

    /// One-line F3 summary (also usable in the debug screenshot).
    pub fn summary_line(&self) -> String {
        format!(
            "{} {} ({}) {}% {}",
            self.phase,
            self.precipitation,
            if self.biome.is_empty() { "?" } else { &self.biome },
            (self.intensity * 100.0).round() as u32,
            format_remaining(self.remaining_seconds),
        )
    }
}

/// Draws the compact weather chip at the top-centre of the screen.
///
/// It sits below the top edge and above the crosshair, never covering the
/// hotbar or the vitals row. A semi-opaque backing keeps it readable over both
/// snow and a night sky.
pub fn draw_weather_hud(frame: &mut UiFrame, hud: &WeatherHud, screen: [f32; 2]) {
    let scale = 1.15;
    let text = hud.compact_line();
    let max_chars = ((screen[0] - 32.0) / ob_render::glyph_advance(scale)).max(8.0) as usize;
    let text = if text.len() > max_chars {
        // Byte-truncate on a char boundary; the HUD strings are ASCII apart
        // from a biome name, which is not in this compact line.
        let mut cut = max_chars.saturating_sub(1);
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}..", &text[..cut])
    } else {
        text
    };
    let width = frame.text_width(scale, &text);
    let height = ob_render::line_height(scale);
    let x = ((screen[0] - width) * 0.5).max(8.0);
    let y = 12.0;
    frame.rect(x - 6.0, y - 3.0, width + 12.0, height + 6.0, [0.02, 0.03, 0.05, 0.62]);
    let color = if hud.flash {
        [0.85, 0.92, 1.0, 1.0]
    } else if hud.sheltered {
        [0.72, 0.78, 0.86, 0.92]
    } else {
        [0.95, 0.97, 1.0, 0.96]
    };
    frame.text(x, y, scale, color, &text);
}

/// Gameplay-light factor from phase (matches the server's gameplay query).
pub fn gameplay_light_factor(phase: WeatherPhase) -> f32 {
    match phase {
        WeatherPhase::Clear => 1.0,
        WeatherPhase::Precipitating => 0.8,
        WeatherPhase::Thunder => 0.6,
    }
}

/// Fire crackle bed target for a nearby fire count.
///
/// The audio plan suggests `fires / 4` capped at one with a 0.8 bed ceiling.
pub fn fire_loop_target(fires: usize) -> f32 {
    (fires as f32 / 4.0).clamp(0.0, 1.0) * 0.8
}

/// Ambient wind for particles, derived from the authoritative storm epoch.
pub fn presentation_wind(snapshot: &WeatherSnapshot) -> [f32; 2] {
    ob_render::weather_wind(
        snapshot.epoch,
        if snapshot.phase == WeatherPhase::Clear {
            0.0
        } else {
            snapshot.intensity
        },
        snapshot.precipitation == PrecipitationKind::Snow,
    )
}

/// Result of sampling the world around the eye for precipitation occlusion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkySample {
    /// `0` under a solid roof, `1` with open sky above the eye.
    pub exposure: f32,
    /// First solid surface height below the eye, or `WORLD_MIN_Y` when none.
    pub ground_y: f32,
    /// Whether any solid block covers the eye column.
    pub sheltered: bool,
}

/// Samples the world column above/below the eye.
///
/// `exposure` is binary: any solid block above eye level means rain cannot
/// reach the player. That is intentionally conservative (leaves and slabs
/// count), matching the server's heightmap-based `sky_exposed` query closely
/// enough for presentation. The scan is bounded by the world height and only
/// called a few times per second by the client.
pub fn sky_exposure(world: &World, registry: &Registry, eye: WorldVec) -> SkySample {
    let x = eye.x.floor() as i32;
    let z = eye.z.floor() as i32;
    let eye_y = eye.y.floor() as i32;
    let mut sheltered = false;
    for y in (eye_y + 1).max(WORLD_MIN_Y)..WORLD_MAX_Y {
        let pos = BlockPos::new(x, y, z);
        if !pos.in_world() {
            break;
        }
        let block = world.get_block(pos);
        if registry.block(block).solid {
            sheltered = true;
            break;
        }
    }
    let mut ground_y = WORLD_MIN_Y as f32;
    for y in (WORLD_MIN_Y..=eye_y).rev() {
        let pos = BlockPos::new(x, y, z);
        if !pos.in_world() {
            continue;
        }
        let block = world.get_block(pos);
        if registry.block(block).solid {
            ground_y = (y + 1) as f32;
            break;
        }
    }
    SkySample {
        exposure: if sheltered { 0.0 } else { 1.0 },
        ground_y,
        sheltered,
    }
}

/// Collects burning block centres around the player, nearest first.
///
/// The scan is a small box instead of a world query because fire has no client
/// registry of its own: the authoritative state is the block palette itself.
/// At radius 8 that is at most 17×17×9 block reads; the caller runs it a few
/// times per second, not every frame.
pub fn nearby_fires(world: &World, registry: &Registry, center: WorldVec, radius: i32, limit: usize) -> Vec<[f32; 3]> {
    let radius = radius.clamp(1, 16);
    let x0 = center.x.floor() as i32;
    let y0 = center.y.floor() as i32;
    let z0 = center.z.floor() as i32;
    let mut fires = Vec::new();
    for y in (y0 - 5)..=(y0 + 5) {
        for z in (z0 - radius)..=(z0 + radius) {
            for x in (x0 - radius)..=(x0 + radius) {
                let pos = BlockPos::new(x, y, z);
                if !pos.in_world() {
                    continue;
                }
                let block = world.get_block(pos);
                if registry.block(block).id != "openblock:fire" {
                    continue;
                }
                fires.push([x as f32 + 0.5, y as f32 + 0.2, z as f32 + 0.5]);
                if fires.len() >= limit {
                    return fires;
                }
            }
        }
    }
    fires
}

/// Formats a duration as `M:SS` (or `H:MM:SS` for very long phases).
pub fn format_remaining(seconds: u32) -> String {
    let seconds = seconds.min(24 * 60 * 60);
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn same_target(previous: f32, next: f32) -> bool {
    if previous.is_nan() {
        return next == 0.0;
    }
    (previous - next).abs() < 0.005
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_audio::ClipId;

    fn snapshot(phase: WeatherPhase, intensity: f32, precipitation: PrecipitationKind) -> WeatherSnapshot {
        WeatherSnapshot::new(
            phase,
            f64::from(intensity),
            6_000,
            3,
            precipitation,
            "plains",
            f64::from(gameplay_light_factor(phase)),
        )
    }

    #[test]
    fn snapshot_maps_protocol_tags_and_sanitises() {
        let message = ServerMessage::WeatherSync {
            phase: WeatherPhase::Thunder,
            intensity: 2.5,
            remaining_ticks: 4_200,
            epoch: 9,
            precipitation: PrecipitationKind::Snow,
            biome: "snowy_taiga".to_string(),
            weather_factor: f64::INFINITY,
        };
        let snapshot = WeatherSnapshot::from_message(&message).expect("weather sync");
        assert_eq!(snapshot.phase, WeatherPhase::Thunder);
        assert_eq!(snapshot.intensity, 1.0);
        assert_eq!(snapshot.weather_factor, 1.0);
        assert_eq!(snapshot.remaining_seconds(), 210);
        assert_eq!(snapshot.render_precipitation(), WeatherPrecipitation::Snow);
        assert_eq!(snapshot.storm_weight(), 1.0);
        assert_eq!(snapshot.phase_label(), "Thunder snow");
        assert_eq!(
            WeatherSnapshot::from_message(&ServerMessage::TimeSync {
                tick: 1,
                day_phase: 2,
                day_ticks: 24_000
            }),
            None
        );
        assert_eq!(WeatherSnapshot::clear().particles([0.0; 2], 1.0, 0.0).intensity, 0.0);
    }

    #[test]
    fn clear_weather_has_no_hud_and_no_loops() {
        let mut presentation = WeatherPresentation::new();
        let audio = presentation.update(1.0 / 60.0, 0);
        assert_eq!(audio.rain_light, 0.0);
        assert_eq!(audio.rain_heavy, 0.0);
        assert_eq!(audio.snow, 0.0);
        assert_eq!(audio.fire, 0.0);
        assert!(audio.cues.is_empty());
        assert!(presentation.hud().is_none());
        assert!(!presentation.is_active());
        assert_eq!(presentation.visual(), WeatherVisual::clear());
    }

    #[test]
    fn rain_maps_to_light_and_heavy_beds() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Precipitating, 0.75, PrecipitationKind::Rain));
        let audio = presentation.update(1.0 / 60.0, 0);
        let (light, heavy) = rain_intensity_gains(0.75);
        assert!((audio.rain_light - light).abs() < 1e-6);
        assert!((audio.rain_heavy - heavy).abs() < 1e-6);
        assert_eq!(audio.snow, 0.0);
        assert!(audio.loops_changed);
        // Repeating the same inputs does not report a loop change.
        let again = presentation.update(1.0 / 60.0, 0);
        assert!(!again.loops_changed);
        let hud = presentation.hud().expect("rain HUD");
        assert_eq!(hud.phase, "Rain");
        assert_eq!(hud.precipitation, "rain");
        assert!(hud.compact_line().contains("factor 0.80"));
    }

    #[test]
    fn snow_uses_the_wind_bed_and_no_rain() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Precipitating, 0.6, PrecipitationKind::Snow));
        let audio = presentation.update(0.016, 0);
        assert_eq!(audio.rain_light, 0.0);
        assert_eq!(audio.rain_heavy, 0.0);
        assert!((audio.snow - 0.33).abs() < 1e-6);
        assert_eq!(presentation.hud().unwrap().phase, "Snow");
    }

    #[test]
    fn storm_dims_the_environment_and_changes_fog_factor() {
        let clear = WeatherPresentation::new().visual();
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 1.0, PrecipitationKind::Rain));
        let storm = presentation.visual();
        assert_eq!(storm.storm, 1.0);
        let clear_visual = ob_render::Environment::default().visual_with_weather(clear);
        let storm_visual = ob_render::Environment::default().visual_with_weather(storm);
        assert!(storm_visual.sun_intensity < clear_visual.sun_intensity);
        assert!(storm_visual.ambient_light < clear_visual.ambient_light);
        assert!(storm_visual.daylight < clear_visual.daylight);
        assert!(storm_visual.star_visibility <= clear_visual.star_visibility);
        assert!(luminance(storm_visual.horizon) < luminance(clear_visual.horizon));
        // The storm fog wall sits closer than the clear-weather one.
        let clear_camera = ob_render::Camera::new(WorldVec::new(0.0, 64.0, 0.0), 0.0, 0.0);
        let clear_uniform = camera_fog(&clear_camera, WeatherVisual::clear());
        let storm_uniform = camera_fog(
            &clear_camera,
            WeatherVisual::new(1.0, 1.0, WeatherPrecipitation::Rain, 0.0),
        );
        assert!(storm_uniform.1 < clear_uniform.1, "storm fog ends closer");
        assert!(storm_uniform.0 < clear_uniform.0, "storm fog starts closer");
    }

    fn luminance(color: [f32; 3]) -> f32 {
        0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2]
    }

    /// Tests the fog bounds through the public camera helper without exposing
    /// the private uniform: the uniform copies `Camera::fog_range` and then
    /// applies the storm factor documented in `pipeline.rs`.
    fn camera_fog(camera: &ob_render::Camera, weather: WeatherVisual) -> (f32, f32) {
        let (start, end) = camera.fog_range();
        let weather = weather.sanitised();
        let haze = weather.cloud_weight();
        if haze <= 0.0 {
            return (start, end);
        }
        let end = (end * (1.0 - 0.35 * haze)).max(start + 1.0);
        let start = (start * (1.0 - 0.45 * haze)).max(16.0).min(end - 1.0);
        (start, end)
    }

    #[test]
    fn fire_bed_scales_with_nearby_fire_count() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Precipitating, 0.8, PrecipitationKind::Rain));
        let audio = presentation.update(0.016, 4);
        assert!((audio.fire - 0.8).abs() < 1e-6);
        assert_eq!(fire_loop_target(0), 0.0);
        assert_eq!(fire_loop_target(1), 0.2);
        assert_eq!(fire_loop_target(16), 0.8);
        assert_eq!(presentation.hud().unwrap().fire_count, 4);
        assert!(
            presentation
                .debug_lines()
                .iter()
                .any(|line| line.contains("4 burning blocks"))
        );
    }

    #[test]
    fn thunder_is_delayed_by_speed_of_sound_and_flash_decays() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 1.0, PrecipitationKind::Rain));
        let listener = [0.0, 64.0, 0.0];
        let strike = [0.0, 70.0, 343.0];
        let immediate = presentation.trigger_lightning(strike, true, listener, 0.0);
        assert!(immediate.is_empty(), "distant thunder has no immediate crack");
        assert_eq!(presentation.flash, 1.0);
        assert_eq!(presentation.flash_events, 1);
        assert_eq!(presentation.lightning_events, 1);
        assert_eq!(presentation.pending_thunder, 1);
        // The update step is clamped to 0.25 s; feed two frames for half the
        // sound delay. No cue yet and the flash has decayed most of the way.
        assert!(presentation.update(0.25, 0).cues.is_empty());
        let audio = presentation.update(0.25, 0);
        assert!(audio.cues.is_empty());
        assert!(presentation.flash < 0.05);
        // The remaining half arrives just after one second total; updates are
        // clamped to 0.25 s, so step frames until the queue empties.
        let mut cues = Vec::new();
        for _ in 0..8 {
            cues.extend(presentation.update(0.25, 0).cues);
        }
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].0.clip, ClipId::ThunderDistant);
        assert_eq!(presentation.pending_thunder, 0);
    }

    #[test]
    fn close_lightning_plays_a_crack_now_and_near_thunder_later() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 1.0, PrecipitationKind::Rain));
        let immediate = presentation.trigger_lightning([0.0, 66.0, 40.0], true, [0.0, 64.0, 0.0], 0.0);
        assert_eq!(immediate.len(), 0, "40 m selects ThunderNear, no extra crack");
        assert!(presentation.shake > 0.0);
        let mut cues = Vec::new();
        for _ in 0..20 {
            cues.extend(presentation.update(0.02, 0).cues);
        }
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].0.clip, ClipId::ThunderNear);

        // 70 m is distant thunder plus an immediate crack.
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 1.0, PrecipitationKind::Rain));
        let immediate = presentation.trigger_lightning([0.0, 66.0, 70.0], true, [0.0, 64.0, 0.0], 0.0);
        assert_eq!(immediate.len(), 1);
        assert_eq!(immediate[0].0.clip, ClipId::LightningCrack);
    }

    #[test]
    fn explosion_cue_and_shake_follow_distance() {
        let mut presentation = WeatherPresentation::new();
        let near = presentation
            .trigger_explosion([1.0, 64.0, 1.0], 3.0, [0.0, 64.0, 0.0], 0.0)
            .expect("near explosion cue");
        assert_eq!(near.0.clip, ClipId::Explosion);
        assert!(near.0.gain > 0.0);
        assert!(presentation.shake > 0.0);
        assert!(presentation.flash > 0.0);
        let far = presentation
            .trigger_explosion([0.0, 64.0, 40.0], 4.0, [0.0, 64.0, 0.0], 0.0)
            .expect("audible at 40 m");
        assert!(far.0.gain < near.0.gain);
        let silent = presentation.trigger_explosion([0.0, 64.0, 200.0], 4.0, [0.0, 64.0, 0.0], 0.0);
        assert!(silent.is_none());
        assert!(
            presentation
                .trigger_explosion([f64::NAN, 0.0, 0.0], 3.0, [0.0; 3], 0.0)
                .is_none()
        );
    }

    #[test]
    fn reconnect_resets_weather_and_pending_thunder() {
        let mut presentation = WeatherPresentation::new();
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 1.0, PrecipitationKind::Rain));
        presentation.trigger_lightning([100.0, 70.0, 0.0], true, [0.0, 64.0, 0.0], 0.0);
        assert!(presentation.is_active());
        presentation.clear();
        assert!(!presentation.is_active());
        assert_eq!(presentation.snapshot.phase, WeatherPhase::Clear);
        assert_eq!(presentation.pending_thunder, 0);
        assert_eq!(presentation.flash, 0.0);
        assert_eq!(presentation.lightning_events, 0);
    }

    #[test]
    fn hud_lines_and_duration_formatting() {
        assert_eq!(format_remaining(0), "0:00");
        assert_eq!(format_remaining(59), "0:59");
        assert_eq!(format_remaining(61), "1:01");
        assert_eq!(format_remaining(3_600), "1:00:00");
        let mut presentation = WeatherPresentation::new();
        presentation.sheltered = true;
        presentation.apply_snapshot(snapshot(WeatherPhase::Thunder, 0.87, PrecipitationKind::Rain));
        let hud = presentation.hud().expect("hud");
        assert!(hud.compact_line().contains("sheltered"));
        assert!(hud.summary_line().contains("87%"));
        let lines = presentation.debug_lines();
        assert!(lines[0].contains("thunder"));
        assert!(lines[1].contains("plains"));
        assert!(lines[2].contains("sheltered"));
        assert_eq!(gameplay_light_factor(WeatherPhase::Clear), 1.0);
        assert_eq!(gameplay_light_factor(WeatherPhase::Precipitating), 0.8);
        assert_eq!(gameplay_light_factor(WeatherPhase::Thunder), 0.6);
        assert_eq!(presentation.shake_angle(0.0, true), 0.0);
    }

    #[test]
    fn weather_hud_is_drawn_inside_the_screen() {
        let mut frame = UiFrame::new(1280.0, 720.0);
        let hud = WeatherHud {
            phase: "Thunderstorm",
            precipitation: "rain",
            biome: "plains".to_string(),
            intensity: 0.9,
            remaining_seconds: 123,
            weather_factor: 0.6,
            sheltered: false,
            flash: false,
            lightning_events: 3,
            flash_events: 3,
            pending_thunder: 0,
            fire_count: 0,
        };
        draw_weather_hud(&mut frame, &hud, [1280.0, 720.0]);
        let (vertices, quads) = frame.measure();
        assert!(vertices > 0 && quads > 0);
        // A narrow screen still produces a clipped, valid frame.
        let mut narrow = UiFrame::new(320.0, 240.0);
        draw_weather_hud(&mut narrow, &hud, [320.0, 240.0]);
    }
}

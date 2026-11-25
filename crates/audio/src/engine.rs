//! Engine lifecycle, the platform backend and the public API.

use std::sync::Arc;
use std::time::Instant;

use crate::mixer::{CommandQueue, Control, Mixer, PlayCommand, group_info};
use crate::synth::{self, GROUP_COUNT};
use crate::{Material, Sfx};

/// Nominal synthesis rate. All clips are generated at this rate; the Pulse
/// backend requests exactly this rate and the mixer can resample offline.
pub const SAMPLE_RATE: u32 = 48_000;
/// Output channel count of the nominal configuration.
pub const CHANNELS: u16 = 2;
/// Maximum number of simultaneously active sample voices.
pub const MAX_VOICES: usize = 16;

/// Why the audio engine could not open an output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// The host has no default output device or no reachable sound server
    /// (headless CI, container, no PipeWire/PulseAudio session).
    NoOutputDevice,
    /// A device exists but exposes no configuration this engine can use.
    UnsupportedConfiguration(String),
    /// The platform backend failed while loading symbols, opening the stream or
    /// starting the writer thread.
    Backend(String),
    /// This platform has no real-time output backend in the 1.0 slice (the
    /// silent stub used on Windows and macOS).
    BackendDisabled,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoOutputDevice => formatter.write_str("no output device or reachable audio server"),
            AudioError::UnsupportedConfiguration(reason) => {
                write!(formatter, "no supported audio output configuration: {reason}")
            }
            AudioError::Backend(reason) => write!(formatter, "audio backend error: {reason}"),
            AudioError::BackendDisabled => {
                formatter.write_str("no real-time audio backend on this platform (silent stub)")
            }
        }
    }
}

impl std::error::Error for AudioError {}

/// Runtime description of the audio output, safe to show in settings and the
/// F3 overlay.
#[derive(Debug, Clone)]
pub struct AudioStatus {
    /// `pulse` for a real Linux stream, `headless` for the offline mixer.
    pub backend: &'static str,
    /// Device name reported by the backend (`pulse: default` on Linux), if any.
    pub device_name: Option<String>,
    /// Output sample rate.
    pub sample_rate: u32,
    /// Output channel count.
    pub channels: u16,
    /// Requested server buffer, or `0` for an unspecified backend default.
    pub buffer_frames: u32,
    /// Estimated buffer latency in milliseconds.
    pub latency_ms: f32,
    /// Number of generated clips, including the two loops.
    pub clip_count: usize,
    /// Wall-clock time spent synthesising the bank at startup.
    pub synthesis_ms: f32,
}

impl AudioStatus {
    /// Short one-line description for logs and the F3 overlay.
    pub fn description(&self) -> String {
        if self.backend == "headless" {
            return format!(
                "headless / {} Hz / {} ch / {} clips",
                self.sample_rate, self.channels, self.clip_count
            );
        }
        match self.device_name.as_deref() {
            Some(name) => format!(
                "{name} / {} Hz / {} ch / {:.1} ms",
                self.sample_rate, self.channels, self.latency_ms
            ),
            None => format!(
                "{} / {} Hz / {} ch / {:.1} ms",
                self.backend, self.sample_rate, self.channels, self.latency_ms
            ),
        }
    }
}

/// State of the optional real-time backend.
enum Backend {
    /// A running output writer thread. Only built on Linux; other platforms
    /// compile the silent stub and never construct this variant.
    #[cfg(target_os = "linux")]
    Device(crate::backend::Output),
    Headless(Box<Mixer>),
    None,
}

/// The public audio engine.
///
/// # Thread model
///
/// The main thread owns this value and pushes small `Copy` play commands into a
/// bounded ring (capacity 256). On Linux a dedicated writer thread owns the
/// mixer and the PulseAudio stream: it renders 512-frame stereo blocks and
/// blocks in `pa_simple_write`, using fixed-size state only. The mixer
/// `try_lock`s the ring and never allocates, logs, blocks or performs I/O; error
/// logging happens only when a write fails, outside the steady-state path.
/// Without a device the engine is [`AudioEngine::headless`] and renders only
/// when [`AudioEngine::render_offline`] is called.
///
/// Volume changes are atomic and apply within one block. The engine is not
/// `Sync`; keep it on the main thread and call [`AudioEngine::shutdown`] (or
/// drop it) when leaving the game.
pub struct AudioEngine {
    backend: Backend,
    control: Arc<Control>,
    queue: Arc<CommandQueue>,
    status: AudioStatus,
    elapsed: f64,
    last_play: [f32; GROUP_COUNT],
    seed: u32,
    footstep_distance: f32,
    footstep_cooldown: f32,
    listener_position: [f32; 3],
    listener_yaw: f32,
    skipped_commands: u64,
}

impl AudioEngine {
    /// Opens the default output and synthesises the clip bank.
    ///
    /// On Linux the PulseAudio simple library is loaded dynamically and the
    /// default PipeWire/PulseAudio sink is opened. Windows and macOS use the
    /// silent stub and always return [`AudioError::BackendDisabled`].
    ///
    /// Any error means "run silently": the caller logs it and continues. When
    /// no server is reachable this returns [`AudioError::NoOutputDevice`] (or
    /// another backend error for a broken installation). Construction never
    /// panics.
    #[cfg(target_os = "linux")]
    pub fn new() -> Result<Self, AudioError> {
        let synthesis_started = Instant::now();
        let bank = Arc::new(synth::build_bank());
        let synthesis_ms = synthesis_started.elapsed().as_secs_f32() * 1000.0;
        let control = Arc::new(Control::new(0.8, 1.0, 0.55));
        let queue = Arc::new(CommandQueue::new());
        let started = crate::backend::start(Arc::clone(&bank), Arc::clone(&control), Arc::clone(&queue))?;
        let status = AudioStatus {
            backend: "pulse",
            device_name: started.device_name,
            sample_rate: started.sample_rate,
            channels: started.channels,
            buffer_frames: started.buffer_frames,
            latency_ms: started.latency_ms,
            clip_count: bank.clip_count(),
            synthesis_ms,
        };
        log::debug!(
            "audio: pulse {} ({} clips synthesised in {:.1} ms)",
            status.description(),
            status.clip_count,
            status.synthesis_ms
        );
        Ok(Self {
            backend: Backend::Device(started.output),
            control,
            queue,
            status,
            elapsed: 0.0,
            last_play: [-1.0e9; GROUP_COUNT],
            seed: 0x1234_5678,
            footstep_distance: 0.0,
            footstep_cooldown: 0.0,
            listener_position: [0.0; 3],
            listener_yaw: 0.0,
            skipped_commands: 0,
        })
    }

    /// Builds the silent stub engine.
    ///
    /// There is no real-time output on this platform in the 1.0 slice. The
    /// caller treats the error as "run silently"; [`AudioEngine::headless`] is
    /// still available for tests and offline rendering.
    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Result<Self, AudioError> {
        Err(crate::backend::stub::error())
    }

    /// Builds a mixer that never opens a device, for tests, offline rendering
    /// and environments that must stay completely silent.
    pub fn headless() -> Self {
        let synthesis_started = Instant::now();
        let bank = Arc::new(synth::build_bank());
        let synthesis_ms = synthesis_started.elapsed().as_secs_f32() * 1000.0;
        let control = Arc::new(Control::new(0.8, 1.0, 0.55));
        let queue = Arc::new(CommandQueue::new());
        let mixer = Mixer::new(
            Arc::clone(&bank),
            Arc::clone(&control),
            Arc::clone(&queue),
            f64::from(SAMPLE_RATE),
        );
        let status = AudioStatus {
            backend: "headless",
            device_name: None,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            buffer_frames: 0,
            latency_ms: 0.0,
            clip_count: bank.clip_count(),
            synthesis_ms,
        };
        log::debug!(
            "audio: headless mixer ({} clips synthesised in {:.1} ms)",
            status.clip_count,
            status.synthesis_ms
        );
        Self {
            backend: Backend::Headless(Box::new(mixer)),
            control,
            queue,
            status,
            elapsed: 0.0,
            last_play: [-1.0e9; GROUP_COUNT],
            seed: 0x1234_5678,
            footstep_distance: 0.0,
            footstep_cooldown: 0.0,
            listener_position: [0.0; 3],
            listener_yaw: 0.0,
            skipped_commands: 0,
        }
    }

    /// Metadata about the active backend.
    pub fn status(&self) -> &AudioStatus {
        &self.status
    }

    /// Whether a usable backend is currently attached.
    ///
    /// A PulseAudio writer that gave up after repeated write errors reports
    /// `false`, which turns every later play request into a no-op.
    pub fn is_available(&self) -> bool {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Device(output) => !output.is_failed(),
            Backend::Headless(_) => true,
            Backend::None => false,
        }
    }

    /// Sets the master volume, clamped to `0.0..=1.0`.
    pub fn set_master_volume(&mut self, volume: f32) {
        self.control.set_master(volume);
    }

    /// Sets the SFX bus volume, clamped to `0.0..=1.0`.
    pub fn set_sfx_volume(&mut self, volume: f32) {
        self.control.set_sfx(volume);
    }

    /// Sets the music/ambience bus volume, clamped to `0.0..=1.0`.
    pub fn set_music_volume(&mut self, volume: f32) {
        self.control.set_music(volume);
    }

    /// Enables or disables all output. Disabling fades to silence; it never
    /// stops the device thread.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.control.set_enabled(enabled);
    }

    /// Plays a non-positional sound at unity gain and centre pan.
    pub fn play_sfx(&mut self, sfx: Sfx) {
        self.play_sfx_at(sfx, 1.0, None);
    }

    /// Plays a sound with an explicit gain and optional constant-power pan.
    ///
    /// `gain` is clamped to `0.0..=4.0` and `pan` to `-1.0..=1.0`. Requests
    /// for the same group within its cooldown are dropped before they reach the
    /// ring.
    pub fn play_sfx_at(&mut self, sfx: Sfx, gain: f32, pan: Option<f32>) {
        if !self.is_available() || !self.control.enabled() {
            return;
        }
        let gain = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 0.0 };
        if gain <= 0.0 {
            return;
        }
        let (group, cooldown) = group_info(sfx);
        let now = self.elapsed as f32;
        if now - self.last_play[group] < cooldown {
            self.skipped_commands = self.skipped_commands.saturating_add(1);
            return;
        }
        self.last_play[group] = now;
        self.seed = self.seed.wrapping_add(0x9e37_79b9);
        let random = synth::hash_u64(u64::from(self.seed) ^ group as u64);
        let unit = ((random >> 32) as f32) / 4_294_967_296.0;
        let rate = 0.955 + 0.09 * unit;
        let pan = pan.map_or(
            0.0,
            |value| if value.is_finite() { value.clamp(-1.0, 1.0) } else { 0.0 },
        );
        let command = PlayCommand {
            group: group as u16,
            seed: self.seed,
            gain,
            pan,
            rate,
        };
        self.queue.push(command, &self.control);
    }

    /// Updates the listener used by [`AudioEngine::play_sfx_world`].
    pub fn set_listener(&mut self, position: [f32; 3], yaw: f32) {
        if position.iter().all(|value| value.is_finite()) {
            self.listener_position = position;
        }
        if yaw.is_finite() {
            self.listener_yaw = yaw;
        }
    }

    /// Plays `sfx` at a world position with linear distance attenuation (24 m)
    /// and a yaw-relative pan. The client normally passes the block or item
    /// position here.
    pub fn play_sfx_world(&mut self, sfx: Sfx, position: [f32; 3], gain: f32) {
        if !position.iter().all(|value| value.is_finite()) {
            return;
        }
        let dx = position[0] - self.listener_position[0];
        let dz = position[2] - self.listener_position[2];
        let distance = (dx * dx + dz * dz).sqrt().max(0.25);
        let attenuation = (1.0 - distance / 24.0).clamp(0.0, 1.0);
        let right = dx * self.listener_yaw.cos() - dz * self.listener_yaw.sin();
        let pan = (right / distance * 0.8).clamp(-1.0, 1.0);
        self.play_sfx_at(sfx, gain * attenuation, Some(pan));
    }

    /// Advances the engine clock and footstep cooldown.
    ///
    /// Call this once per rendered frame. The ambient and music beds are
    /// continuous and are faded by the mixer; this clock schedules the
    /// distance-gated footsteps and the per-group rate limits.
    pub fn tick(&mut self, dt: f32) {
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.25) } else { 0.0 };
        self.elapsed += f64::from(dt);
        self.footstep_cooldown = (self.footstep_cooldown - dt).max(0.0);
    }

    /// Accumulates ground distance and emits a footstep at most every ~0.36 s.
    ///
    /// `distance` is horizontal metres moved since the previous call; the
    /// client must pass `on_ground = false` while paused, flying, swimming or
    /// dead so the accumulator resets.
    pub fn update_footsteps(&mut self, distance: f32, material: Material, on_ground: bool) {
        if !on_ground || !self.is_available() || !self.control.enabled() {
            self.footstep_distance = 0.0;
            return;
        }
        if !distance.is_finite() || distance <= 0.0 {
            return;
        }
        self.footstep_distance += distance.min(1.0);
        if self.footstep_distance > 3.4 {
            self.footstep_distance = 1.7;
        }
        if self.footstep_distance >= 1.7 && self.footstep_cooldown <= 0.0 {
            self.play_sfx_at(Sfx::Footstep(material), 0.72, Some(0.0));
            self.footstep_distance -= 1.7;
            self.footstep_cooldown = 0.36;
        }
    }

    /// Renders `frames` stereo frames through the offline mixer.
    ///
    /// Only a headless engine produces non-silent data; a device engine returns
    /// a zeroed buffer because its mixer is owned by the writer thread. This is
    /// the hook CI uses to exercise synthesis without a sound card.
    pub fn render_offline(&mut self, frames: usize) -> Vec<f32> {
        let frames = frames.min(SAMPLE_RATE as usize * 10);
        let mut output = vec![0.0f32; frames * usize::from(CHANNELS)];
        if let Backend::Headless(mixer) = &mut self.backend {
            mixer.render(&mut output, usize::from(CHANNELS));
        }
        output
    }

    /// Number of voices active after the most recent mixer block.
    pub fn active_voices(&self) -> usize {
        self.control.active_voices()
    }

    /// Commands accepted into the play ring.
    pub fn accepted_commands(&self) -> u64 {
        self.control.accepted_commands()
    }

    /// Requests dropped by the per-group rate limiter.
    pub fn skipped_commands(&self) -> u64 {
        self.skipped_commands
    }

    /// Requests dropped because the command ring was full.
    pub fn dropped_commands(&self) -> u64 {
        self.control.dropped_commands()
    }

    /// Write failures reported by the PulseAudio writer thread. The backend
    /// gives up after three consecutive failures.
    pub fn device_errors(&self) -> usize {
        self.control.device_errors()
    }

    /// Stops and joins the output writer. A healthy PulseAudio stream is
    /// drained and freed by the writer as it exits. Subsequent play calls are
    /// no-ops until the engine is replaced.
    pub fn shutdown(&mut self) {
        let backend = std::mem::replace(&mut self.backend, Backend::None);
        match backend {
            #[cfg(target_os = "linux")]
            Backend::Device(mut output) => output.stop(),
            Backend::Headless(_) | Backend::None => {}
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

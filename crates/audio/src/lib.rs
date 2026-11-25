//! OpenBlock's procedural audio engine.
//!
//! The crate synthesises every sound into in-memory PCM at startup (there are
//! no external audio assets), mixes up to [`MAX_VOICES`] sample voices plus a
//! music pad, a legacy wind bed and four 1.5 weather/fire loop beds, and
//! optionally feeds real-time output. A machine with no device is a first-class
//! state: construct the engine with [`AudioEngine::headless`] or treat
//! [`AudioEngine::new`]'s error as "run silently".
//!
//! # Output backends
//!
//! * **Linux:** the PulseAudio *simple* API is loaded at runtime with `dlopen`;
//!   there is no build-time ALSA/PulseAudio dependency. PipeWire's PulseAudio
//!   compatibility socket works. A dedicated writer thread renders 512-frame
//!   stereo `f32` blocks and writes them through `pa_simple_write`.
//! * **Windows/macOS:** a silent stub with the same public API. [`AudioEngine::new`]
//!   returns [`AudioError::BackendDisabled`]; no system audio library is linked.
//!
//! # Architecture
//!
//! * [`Material`] selects the timbre family for footsteps, breaks and places.
//! * [`Sfx`] is the small, closed set of gameplay events from the 1.0 bank.
//! * [`ClipId`] is the extended 1.5 registry for rain/snow/wind, fire, thunder,
//!   lightning, explosions, creeper and subtle mob cues. [`ClipKind`] tells
//!   looping beds from one-shots, and [`rain_intensity_gains`], [`thunder_cue`]
//!   and [`explosion_cue`] fold intensity and distance into gain, pitch and pan.
//! * [`AudioEngine`] applies category/master volumes, rate limits duplicate
//!   events, ramps ambient loops, and owns either the output writer or the
//!   offline mixer.
//!
//! Synthesis uses only integer hashes and plain arithmetic (no `rand`, no
//! `sin`/`exp`/`powf`), so the generated buffers are byte-identical across
//! platforms. The unit tests hash the whole bank to catch accidental changes.
//!
//! # Thread model and latency
//!
//! The main thread owns [`AudioEngine`]. It writes atomic gains and pushes
//! small `Copy` commands into a bounded 256-entry ring. On Linux the writer
//! thread owns the mixer: the mixer `try_lock`s the ring, never allocates,
//! logs or touches the filesystem, and renders interleaved stereo `f32`. The
//! writer then blocks in `pa_simple_write`, which is the intended
//! back-pressure mechanism; error logging happens only outside the steady
//! state. Blocks are 512 frames (10.67 ms at 48 kHz) and the
//! server buffer is requested at 25 ms; music/ambience loops have a 0.4-0.65 s
//! baked crossfade. The mixer itself targets well under 0.5 ms of CPU per 60 Hz
//! frame.
//!
//! The only unsafe code in the crate lives in the Linux backend module and is
//! limited to `dlopen`/`dlsym` plus the C ABI calls; the rest of the crate is
//! `#![deny(unsafe_code)]`.

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod backend;
mod catalog;
mod engine;
mod mixer;
mod synth;

pub use catalog::{
    ClipCue, ClipId, ClipKind, LOOP_FADE_SECONDS, constant_power_pan, distance_attenuation, explosion_cue,
    rain_intensity_gains, thunder_cue, world_pan,
};
pub use engine::{AudioEngine, AudioError, AudioStatus, CHANNELS, MAX_VOICES, SAMPLE_RATE};

/// Physical material family used to select footstep and block timbres.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Material {
    /// Grass and leaves: soft, airy noise.
    Grass,
    /// Dirt and mud: dark, dull thud.
    Dirt,
    /// Stone, cobblestone and bedrock: hard, bright click.
    Stone,
    /// Logs, planks and crafting tables: hollow wooden knock.
    Wood,
    /// Sand: high, granular hiss.
    Sand,
    /// Snow: soft crunch.
    Snow,
    /// Gravel: multiple small clicks.
    Gravel,
    /// Fire: dry crackle and a soft snuff when a fire block is broken or placed.
    Fire,
}

impl Material {
    /// Every material, in stable order.
    pub const ALL: [Material; 8] = [
        Material::Grass,
        Material::Dirt,
        Material::Stone,
        Material::Wood,
        Material::Sand,
        Material::Snow,
        Material::Gravel,
        Material::Fire,
    ];

    /// Stable lowercase name used by logs and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Material::Grass => "grass",
            Material::Dirt => "dirt",
            Material::Stone => "stone",
            Material::Wood => "wood",
            Material::Sand => "sand",
            Material::Snow => "snow",
            Material::Gravel => "gravel",
            Material::Fire => "fire",
        }
    }
}

/// A requestable sound effect.
///
/// Variants are synthesised in groups; the engine picks a variant and a small
/// pitch offset deterministically per call, so repeated requests never sound
/// mechanically identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sfx {
    /// A footstep on the given material (four variants).
    Footstep(Material),
    /// A block breaking (three variants per material).
    BlockBreak(Material),
    /// A block being placed (two variants per material).
    BlockPlace(Material),
    /// An item entering the inventory.
    Pickup,
    /// The player took damage (two variants).
    Hurt,
    /// The death screen appeared (once per death).
    Death,
    /// A bite of food (two variants).
    Eat,
    /// A drink.
    Drink,
    /// A successful craft.
    Craft,
    /// A craft that failed or lacked ingredients.
    CraftFail,
    /// A tool's durability reached zero.
    ToolBreak,
    /// Entering water (two variants).
    Splash,
    /// A quiet UI focus/click tick.
    UiClick,
    /// A UI confirmation chime.
    UiConfirm,
}

impl Sfx {
    /// Stable group name for diagnostics; includes the material where relevant.
    pub fn name(self) -> &'static str {
        match self {
            Sfx::Footstep(_) => "footstep",
            Sfx::BlockBreak(_) => "block_break",
            Sfx::BlockPlace(_) => "block_place",
            Sfx::Pickup => "pickup",
            Sfx::Hurt => "hurt",
            Sfx::Death => "death",
            Sfx::Eat => "eat",
            Sfx::Drink => "drink",
            Sfx::Craft => "craft",
            Sfx::CraftFail => "craft_fail",
            Sfx::ToolBreak => "tool_break",
            Sfx::Splash => "splash",
            Sfx::UiClick => "ui_click",
            Sfx::UiConfirm => "ui_confirm",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_names_cover_every_variant() {
        let names: Vec<&str> = Material::ALL.iter().map(|material| material.name()).collect();
        assert_eq!(
            names,
            ["grass", "dirt", "stone", "wood", "sand", "snow", "gravel", "fire"]
        );
        for material in Material::ALL {
            assert!(!Sfx::Footstep(material).name().is_empty());
            assert!(!Sfx::BlockBreak(material).name().is_empty());
            assert!(!Sfx::BlockPlace(material).name().is_empty());
        }
    }

    #[test]
    fn headless_engine_renders_finite_stereo_output() {
        let mut engine = AudioEngine::headless();
        assert!(engine.is_available());
        assert_eq!(engine.status().backend, "headless");
        assert_eq!(engine.status().sample_rate, SAMPLE_RATE);
        assert_eq!(engine.status().channels, CHANNELS);
        assert!(
            engine.status().clip_count > 100,
            "expected the legacy 79 clips plus the 1.5 bank, got {}",
            engine.status().clip_count
        );
        engine.set_master_volume(1.0);
        engine.set_sfx_volume(1.0);
        engine.set_music_volume(0.55);
        engine.play_sfx(Sfx::Pickup);
        let output = engine.render_offline(4_800);
        assert_eq!(output.len(), 4_800 * usize::from(CHANNELS));
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
        assert!(
            output.iter().any(|sample| sample.abs() > 1.0e-5),
            "a pickup must produce audible samples"
        );
    }

    #[test]
    fn headless_output_is_deterministic_for_the_same_commands() {
        let mut first = AudioEngine::headless();
        let mut second = AudioEngine::headless();
        for engine in [&mut first, &mut second] {
            engine.set_master_volume(0.9);
            engine.set_sfx_volume(1.0);
            engine.set_music_volume(0.3);
            engine.tick(0.016);
            engine.play_sfx(Sfx::Pickup);
            engine.tick(0.016);
            engine.play_sfx_at(Sfx::BlockBreak(Material::Stone), 0.8, Some(-0.3));
            engine.tick(0.016);
            engine.play_sfx_at(Sfx::Hurt, 0.6, Some(0.5));
        }
        let a = first.render_offline(8_192);
        let b = second.render_offline(8_192);
        assert_eq!(a, b);
    }

    #[test]
    fn new_clips_overlap_and_loops_are_rejected_by_play_clip() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(1.0);
        engine.set_sfx_volume(1.0);
        engine.set_music_volume(0.0);
        let accepted = engine.accepted_commands();
        assert!(engine.play_clip(ClipId::Explosion, 1.0, Some(0.0)));
        assert!(engine.play_clip(ClipId::CreeperHiss, 0.8, Some(-0.5)));
        assert!(engine.play_clip(ClipId::XpPickup, 0.7, None));
        assert!(!engine.play_clip(ClipId::RainLoop, 1.0, None));
        assert_eq!(engine.accepted_commands(), accepted + 3);
        engine.tick(0.016);
        let output = engine.render_offline(2_048);
        assert_eq!(output.len(), 2_048 * usize::from(CHANNELS));
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
        assert!(output.iter().any(|sample| sample.abs() > 1.0e-4));
        assert!(
            engine.active_voices() >= 2,
            "different one-shots must overlap: {} active",
            engine.active_voices()
        );
        assert_eq!(engine.loop_volume(ClipId::Explosion), None);
    }

    #[test]
    fn distance_cues_and_world_helpers_are_bounded() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(1.0);
        engine.set_sfx_volume(1.0);
        engine.set_listener([0.0, 0.0, 0.0], 0.0);
        let near = thunder_cue(16.0);
        let far = thunder_cue(256.0);
        assert_eq!(near.clip, ClipId::ThunderNear);
        assert_eq!(far.clip, ClipId::ThunderDistant);
        assert!(near.gain > far.gain);
        assert!(engine.play_cue(near, Some(world_pan([0.0; 3], 0.0, [20.0, 0.0, 0.0]))));
        assert!(engine.play_clip_world(ClipId::ArrowImpact, [1.0, 0.0, 3.0], 1.0));
        assert!(!engine.play_clip_world(ClipId::ArrowImpact, [f32::NAN, 0.0, 0.0], 1.0));
        assert_eq!(distance_attenuation(0.0, 24.0), 1.0);
        assert_eq!(distance_attenuation(24.0, 24.0), 0.0);
        assert!(engine.play_cue(ClipCue::new(ClipId::Explosion, 0.6, 1.05), None));
        assert!(!engine.play_cue(ClipCue::new(ClipId::RainHeavyLoop, 1.0, 1.0), None));
        let output = engine.render_offline(4_096);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
    }

    #[test]
    fn ambient_loops_ramp_up_and_stop_through_the_engine() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(1.0);
        engine.set_sfx_volume(0.0);
        engine.set_music_volume(0.0);
        engine.set_ambient_volume(1.0);
        assert!(engine.set_loop_volume(ClipId::RainLoop, 1.0));
        assert!(engine.set_loop_volume(ClipId::FireCrackleLoop, 0.5));
        assert_eq!(engine.loop_volume(ClipId::RainLoop), Some(1.0));
        assert_eq!(engine.loop_volume(ClipId::FireCrackleLoop), Some(0.5));
        assert!(!engine.set_loop_volume(ClipId::Explosion, 1.0));
        assert!(!engine.stop_loop(ClipId::XpPickup));

        let fade = (LOOP_FADE_SECONDS * SAMPLE_RATE as f32) as usize;
        let output = engine.render_offline(fade + 4_800);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
        assert!(
            output.iter().any(|sample| sample.abs() > 1.0e-4),
            "the rain/fire beds must become audible"
        );

        assert!(engine.stop_loop(ClipId::RainLoop));
        assert!(engine.stop_loop(ClipId::FireCrackleLoop));
        let tail = engine.render_offline(fade + 4_800);
        assert!(
            tail.iter().skip(tail.len() - 2_000).all(|sample| *sample == 0.0),
            "stopped loops must reach silence after the ramp"
        );
    }

    #[test]
    fn zero_master_volume_is_silent() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(0.0);
        engine.play_sfx(Sfx::Death);
        let output = engine.render_offline(4_800);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn disabling_fades_to_silence_without_stopping_the_mixer() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(1.0);
        engine.set_enabled(false);
        engine.play_sfx(Sfx::UiClick);
        let output = engine.render_offline(2_048);
        assert!(output.iter().all(|sample| *sample == 0.0));
        assert!(engine.is_available(), "muting must not drop the device");
        engine.set_enabled(true);
    }

    #[test]
    fn footsteps_are_distance_and_time_gated() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(1.0);
        engine.update_footsteps(0.9, Material::Grass, true);
        engine.update_footsteps(0.9, Material::Grass, true);
        let accepted = engine.accepted_commands();
        assert_eq!(accepted, 1, "1.8 m of walking is one stride");
        // No tick: cooldown remains, so the next stride is suppressed.
        engine.update_footsteps(0.9, Material::Grass, true);
        engine.update_footsteps(0.9, Material::Grass, true);
        assert_eq!(engine.accepted_commands(), accepted, "cooldown suppresses the stride");
        // After the cooldown, a stride is accepted again.
        engine.tick(0.2);
        engine.tick(0.2);
        engine.update_footsteps(0.9, Material::Grass, true);
        engine.update_footsteps(0.9, Material::Grass, true);
        assert_eq!(engine.accepted_commands(), accepted + 1);
        // Airborne movement resets the accumulator.
        engine.update_footsteps(10.0, Material::Grass, false);
        assert_eq!(engine.accepted_commands(), accepted + 1);
    }

    #[test]
    fn world_position_calls_are_sanitised_and_shutdown_is_final() {
        let mut engine = AudioEngine::headless();
        engine.set_listener([0.0, 0.0, 0.0], 0.0);
        engine.play_sfx_world(Sfx::BlockPlace(Material::Wood), [1.0, 0.0, -2.0], 1.0);
        engine.play_sfx_world(Sfx::BlockPlace(Material::Wood), [f32::NAN, 0.0, 0.0], 1.0);
        assert_eq!(engine.accepted_commands(), 1);
        engine.shutdown();
        assert!(!engine.is_available());
        engine.play_sfx(Sfx::Pickup);
        assert_eq!(engine.accepted_commands(), 1);
        assert!(engine.render_offline(64).iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn hostile_inputs_never_panic_or_emit_non_finite_samples() {
        let mut engine = AudioEngine::headless();
        engine.set_master_volume(f32::NAN);
        engine.set_sfx_volume(f32::INFINITY);
        engine.set_music_volume(-9.0);
        engine.set_listener([f32::NAN, f32::INFINITY, -0.0], f32::NAN);
        engine.tick(f32::NAN);
        engine.update_footsteps(f32::NAN, Material::Stone, true);
        engine.play_sfx_at(Sfx::Hurt, f32::INFINITY, Some(f32::NAN));
        engine.play_sfx_at(Sfx::Hurt, -1.0, Some(9.0));
        engine.play_sfx_world(Sfx::Splash, [f32::NAN, 0.0, 0.0], 1.0);
        engine.set_ambient_volume(f32::NAN);
        assert!(!engine.play_clip(ClipId::Explosion, f32::NAN, Some(f32::NAN)));
        assert!(!engine.play_clip(ClipId::RainLoop, 1.0, None));
        engine.set_loop_volume(ClipId::RainLoop, f32::INFINITY);
        engine.set_loop_volume(ClipId::RainLoop, f32::NAN);
        engine.play_clip_world(ClipId::CreeperHiss, [f32::NAN, 0.0, 0.0], 1.0);
        engine.play_cue(ClipCue::new(ClipId::ThunderDistant, f32::NAN, f32::NAN), Some(f32::NAN));
        let output = engine.render_offline(256);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
    }

    #[test]
    fn backend_disabled_error_is_documented_and_headless_remains_usable() {
        let error = AudioError::BackendDisabled;
        assert_eq!(
            error.to_string(),
            "no real-time audio backend on this platform (silent stub)"
        );
        assert!(!error.to_string().is_empty());
        let mut engine = AudioEngine::headless();
        assert!(engine.is_available());
        assert!(engine.play_clip(ClipId::LevelUp, 0.5, None));
        let output = engine.render_offline(512);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn constructing_without_a_device_is_graceful() {
        match AudioEngine::new() {
            Ok(engine) => {
                assert!(engine.is_available());
                assert!(!engine.status().description().is_empty());
                #[cfg(target_os = "linux")]
                {
                    let status = engine.status();
                    assert_eq!(status.backend, "pulse");
                    assert_eq!(status.device_name.as_deref(), Some("pulse: default"));
                    assert_eq!(status.sample_rate, SAMPLE_RATE);
                    assert_eq!(status.channels, CHANNELS);
                    assert_eq!(status.buffer_frames, 1_200);
                    assert!((status.latency_ms - 25.0).abs() < 1e-6);
                }
            }
            Err(error) => {
                assert!(!error.to_string().is_empty());
            }
        }
    }

    #[test]
    fn shutdown_and_drop_are_idempotent() {
        let mut engine = AudioEngine::headless();
        engine.play_sfx(Sfx::Pickup);
        engine.shutdown();
        engine.shutdown();
        assert!(!engine.is_available());
        assert!(!engine.set_loop_volume(ClipId::RainLoop, 1.0));
        assert!(engine.render_offline(128).iter().all(|sample| *sample == 0.0));
        drop(engine);
    }

    #[test]
    fn synthesis_and_mixing_fit_the_startup_and_cpu_budget() {
        let started = std::time::Instant::now();
        let mut engine = AudioEngine::headless();
        let synthesis_ms = started.elapsed().as_secs_f32() * 1000.0;
        // Generous CI ceilings; the measured release figures live in the audio
        // spec. This catches accidental quadratic synthesis work.
        assert!(synthesis_ms < 2_000.0, "synthesis took {synthesis_ms:.1} ms");
        engine.set_master_volume(1.0);
        for _ in 0..8 {
            engine.play_sfx(Sfx::BlockBreak(Material::Stone));
            engine.tick(0.2);
        }
        let render_started = std::time::Instant::now();
        let output = engine.render_offline(SAMPLE_RATE as usize);
        let render_ms = render_started.elapsed().as_secs_f32() * 1000.0;
        assert!(!output.is_empty());
        assert!(render_ms < 500.0, "one second of audio took {render_ms:.1} ms to mix");
        eprintln!(
            "audio measurements: synthesis {synthesis_ms:.2} ms, one stereo second {render_ms:.2} ms, clips {}",
            engine.status().clip_count
        );
    }
}

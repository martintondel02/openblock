//! Stable clip identifiers and small presentation helpers for the 1.5 cues.
//!
//! The 1.0 bank is addressed through [`crate::Sfx`]. The 1.5 procedural bank adds
//! weather, fire, explosion, projectile and mob cues. Every entry in [`ClipId`]
//! has a stable snake_case [`ClipId::name`] and a [`ClipKind`], so the client can
//! persist or log an identifier and tell looping beds from one-shots without
//! reaching into the synthesiser.
//!
//! The helpers in this module keep event mixing decisions in one place:
//! [`rain_intensity_gains`] crossfades the two rain beds, [`thunder_cue`] and
//! [`explosion_cue`] fold distance into clip choice, gain and playback rate, and
//! [`constant_power_pan`], [`world_pan`] and [`distance_attenuation`] are the
//! same spatial rules the engine applies to `Sfx`.

/// Whether a procedural clip is a one-shot or a looping ambient bed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipKind {
    /// Plays once and releases its voice when the PCM ends.
    OneShot,
    /// An ambient bed that loops until its target volume reaches zero.
    Loop,
}

impl ClipKind {
    /// Whether this is [`ClipKind::Loop`].
    pub fn is_loop(self) -> bool {
        matches!(self, ClipKind::Loop)
    }

    /// Stable name for diagnostics (`one_shot` or `loop`).
    pub fn name(self) -> &'static str {
        match self {
            ClipKind::OneShot => "one_shot",
            ClipKind::Loop => "loop",
        }
    }
}

/// Stable identifier for every clip added by the 1.5 procedural bank.
///
/// The 1.0 gameplay bank stays addressable through [`crate::Sfx`]; this enum is
/// the registry for weather, fire, explosions, projectiles and the subtle mob
/// cues. Indices are stable and exposed by [`ClipId::index`], so new variants
/// must append rather than reorder to keep logs and persisted names meaningful.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipId {
    /// Light-to-moderate rain bed, layered low/mid noise with droplet patter.
    RainLoop,
    /// Heavy rain bed, darker and denser than [`ClipId::RainLoop`].
    RainHeavyLoop,
    /// Soft snow/wind bed for cold biomes.
    SnowWindLoop,
    /// Fire crackle bed: filtered noise with deterministic pops.
    FireCrackleLoop,
    /// Long, low rolling thunder for strikes beyond 64 m.
    ThunderDistant,
    /// Close thunder with a sharp leading crack and a shorter rumble.
    ThunderNear,
    /// A direct lightning strike: very sharp transient plus a short rumble.
    LightningCrack,
    /// Explosion boom with a low sub sweep and a debris tail.
    Explosion,
    /// Creeper fuse: rising filtered hiss and a thin whistle.
    CreeperHiss,
    /// Bow release whoosh.
    BowShoot,
    /// Arrow impact on a block or entity.
    ArrowImpact,
    /// Critical-hit impact: bright transient and low thump.
    CriticalHit,
    /// Skeleton bone rattle.
    SkeletonRattle,
    /// Spider chitter.
    SpiderChitter,
    /// Experience pickup chime.
    XpPickup,
    /// Level-up fanfare.
    LevelUp,
}

/// Seconds the mixer takes to ramp an ambient loop between 0 and its target.
///
/// Starting, stopping or changing a loop is never a hard cut: the mixer advances
/// the bed's gain by `1 / (LOOP_FADE_SECONDS * sample_rate)` per sample. A
/// volume change therefore settles in at most `LOOP_FADE_SECONDS`.
pub const LOOP_FADE_SECONDS: f32 = 1.2;

impl ClipId {
    /// Every 1.5 clip, in stable order: four loops followed by twelve one-shots.
    pub const ALL: [ClipId; 16] = [
        ClipId::RainLoop,
        ClipId::RainHeavyLoop,
        ClipId::SnowWindLoop,
        ClipId::FireCrackleLoop,
        ClipId::ThunderDistant,
        ClipId::ThunderNear,
        ClipId::LightningCrack,
        ClipId::Explosion,
        ClipId::CreeperHiss,
        ClipId::BowShoot,
        ClipId::ArrowImpact,
        ClipId::CriticalHit,
        ClipId::SkeletonRattle,
        ClipId::SpiderChitter,
        ClipId::XpPickup,
        ClipId::LevelUp,
    ];

    /// The four looping ambient beds, in stable order.
    pub const LOOPS: [ClipId; 4] = [
        ClipId::RainLoop,
        ClipId::RainHeavyLoop,
        ClipId::SnowWindLoop,
        ClipId::FireCrackleLoop,
    ];

    /// The twelve non-looping cues, in stable order.
    pub const ONE_SHOTS: [ClipId; 12] = [
        ClipId::ThunderDistant,
        ClipId::ThunderNear,
        ClipId::LightningCrack,
        ClipId::Explosion,
        ClipId::CreeperHiss,
        ClipId::BowShoot,
        ClipId::ArrowImpact,
        ClipId::CriticalHit,
        ClipId::SkeletonRattle,
        ClipId::SpiderChitter,
        ClipId::XpPickup,
        ClipId::LevelUp,
    ];

    /// Stable snake_case name used by logs, diagnostics and saved settings.
    pub fn name(self) -> &'static str {
        match self {
            ClipId::RainLoop => "rain_loop",
            ClipId::RainHeavyLoop => "rain_heavy_loop",
            ClipId::SnowWindLoop => "snow_wind_loop",
            ClipId::FireCrackleLoop => "fire_crackle_loop",
            ClipId::ThunderDistant => "thunder_distant",
            ClipId::ThunderNear => "thunder_near",
            ClipId::LightningCrack => "lightning_crack",
            ClipId::Explosion => "explosion",
            ClipId::CreeperHiss => "creeper_hiss",
            ClipId::BowShoot => "bow_shoot",
            ClipId::ArrowImpact => "arrow_impact",
            ClipId::CriticalHit => "critical_hit",
            ClipId::SkeletonRattle => "skeleton_rattle",
            ClipId::SpiderChitter => "spider_chitter",
            ClipId::XpPickup => "xp_pickup",
            ClipId::LevelUp => "level_up",
        }
    }

    /// Whether this clip is a looping ambient bed or a one-shot.
    pub fn kind(self) -> ClipKind {
        if matches!(
            self,
            ClipId::RainLoop | ClipId::RainHeavyLoop | ClipId::SnowWindLoop | ClipId::FireCrackleLoop
        ) {
            ClipKind::Loop
        } else {
            ClipKind::OneShot
        }
    }

    /// Convenience predicate for [`ClipId::kind`].
    pub fn is_loop(self) -> bool {
        self.kind().is_loop()
    }

    /// Resolves a [`ClipId::name`] back to its clip.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|clip| clip.name() == name)
    }

    /// Stable index in `0..ClipId::ALL.len()`.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// A resolved one-shot: which clip, what linear gain and which playback rate.
///
/// Construct distance-aware cues with [`thunder_cue`] or [`explosion_cue`], then
/// pass the result to [`crate::AudioEngine::play_cue`]. Gains are clamped to
/// `0.0..=4.0` and rates to `0.5..=2.0`; non-finite values become the neutral
/// value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipCue {
    /// Clip to play. Looping clips are ignored by the playback methods.
    pub clip: ClipId,
    /// Linear gain before the SFX and master buses.
    pub gain: f32,
    /// Playback-rate multiplier (`1.0` is the synthesised pitch).
    pub rate: f32,
}

impl ClipCue {
    /// Builds a cue and clamps both scalars to the public API range.
    pub fn new(clip: ClipId, gain: f32, rate: f32) -> Self {
        Self {
            clip,
            gain: clamped_gain(gain),
            rate: clamped_rate(rate),
        }
    }
}

/// Target volumes for the light and heavy rain beds at `intensity` (`0..=1`).
///
/// Returns `(light, heavy)`. Both are zero at `0.0`; the light bed peaks in the
/// middle of the range and reaches zero at `1.0`, while the heavy bed grows
/// monotonically. The client crossfades between them as the weather intensity
/// ramps. Finite out-of-range values are clamped; `NaN` or infinity means
/// "no rain" and returns `(0.0, 0.0)`.
pub fn rain_intensity_gains(intensity: f32) -> (f32, f32) {
    if !intensity.is_finite() {
        return (0.0, 0.0);
    }
    let intensity = intensity.clamp(0.0, 1.0);
    (2.6 * intensity * (1.0 - intensity), 0.90 * intensity)
}

/// Folds thunder distance into clip choice, gain and pitch.
///
/// Strikes within 64 m use the sharp [`ClipId::ThunderNear`] cue; further
/// strikes use the rolling [`ClipId::ThunderDistant`] cue. Gain falls with
/// distance and distant thunder plays slightly slower, which reads as a deeper
/// rumble. Distances are clamped to `0..=512 m`; non-finite input is treated as
/// the maximum distance so hostile values stay quiet.
pub fn thunder_cue(distance_m: f32) -> ClipCue {
    let distance = clamped_distance(distance_m, 512.0);
    let clip = if distance <= 64.0 {
        ClipId::ThunderNear
    } else {
        ClipId::ThunderDistant
    };
    let falloff = (1.0 - distance / 320.0).max(0.0);
    let gain = (0.05 + 0.95 * falloff * falloff).clamp(0.0, 1.0);
    let rate = (1.0 - 0.18 * (distance / 512.0)).clamp(0.5, 2.0);
    ClipCue::new(clip, gain, rate)
}

/// Folds explosion distance into gain and pitch.
///
/// The cue uses [`ClipId::Explosion`] and a quadratic falloff over 96 m; far
/// blasts are quieter and slightly lower. Non-finite input is treated as beyond
/// the audible range, producing zero gain. Callers should skip zero-gain cues.
pub fn explosion_cue(distance_m: f32) -> ClipCue {
    let distance = clamped_distance(distance_m, 96.0);
    let falloff = 1.0 - distance / 96.0;
    let gain = (falloff * falloff).clamp(0.0, 1.0);
    let rate = (1.0 - 0.12 * (distance / 96.0)).clamp(0.5, 2.0);
    ClipCue::new(ClipId::Explosion, gain, rate)
}

/// Constant-power stereo gains for a pan in `-1.0..=1.0`.
///
/// Returns `(left, right)`; the squares sum to `1.0`, so centre is `-3 dB` on
/// each side. Non-finite pans become centre and out-of-range pans are clamped.
pub fn constant_power_pan(pan: f32) -> (f32, f32) {
    let pan = if pan.is_finite() { pan.clamp(-1.0, 1.0) } else { 0.0 };
    (((1.0 - pan) * 0.5).sqrt(), ((1.0 + pan) * 0.5).sqrt())
}

/// Yaw-relative pan for a world position, in `-1.0..=1.0`.
///
/// `yaw` is the listener's yaw in radians and the world uses the same x/z axes
/// as the rest of the game. The result is capped at `±0.8` so even a sound
/// beside the head keeps some signal in both ears. Non-finite input, or a
/// listener position at the source, returns centre.
pub fn world_pan(listener_position: [f32; 3], yaw: f32, position: [f32; 3]) -> f32 {
    if !listener_position.iter().all(|value| value.is_finite()) || !position.iter().all(|value| value.is_finite()) {
        return 0.0;
    }
    let yaw = if yaw.is_finite() { yaw } else { 0.0 };
    let dx = position[0] - listener_position[0];
    let dz = position[2] - listener_position[2];
    let distance = (dx * dx + dz * dz).sqrt().max(0.25);
    ((dx * yaw.cos() - dz * yaw.sin()) / distance * 0.8).clamp(-1.0, 1.0)
}

/// Linear distance attenuation in `0.0..=1.0`.
///
/// A source at the listener is `1.0`, one at `max_distance` is `0.0`, and one
/// beyond it stays silent. Non-finite or non-positive ranges return zero rather
/// than producing an unbounded gain.
pub fn distance_attenuation(distance: f32, max_distance: f32) -> f32 {
    if !distance.is_finite() || !max_distance.is_finite() || max_distance <= 0.0 {
        return 0.0;
    }
    (1.0 - distance.max(0.0) / max_distance).clamp(0.0, 1.0)
}

fn clamped_gain(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.0, 4.0) } else { 0.0 }
}

fn clamped_rate(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.5, 2.0) } else { 1.0 }
}

fn clamped_distance(value: f32, maximum: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, maximum)
    } else {
        maximum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_unique_stable_and_kind_checked() {
        assert_eq!(ClipId::ALL.len(), ClipId::LOOPS.len() + ClipId::ONE_SHOTS.len());
        for (index, clip) in ClipId::ALL.into_iter().enumerate() {
            assert_eq!(clip.index(), index, "the enum order is the stable index");
            assert_eq!(ClipId::from_name(clip.name()), Some(clip));
            assert!(!clip.name().is_empty());
            assert!(
                clip.name()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
                "{} is not snake_case",
                clip.name()
            );
        }
        let mut names: Vec<&str> = ClipId::ALL.iter().map(|clip| clip.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ClipId::ALL.len(), "clip names must be unique");
        assert!(ClipId::LOOPS.iter().all(|clip| clip.is_loop()));
        assert!(ClipId::ONE_SHOTS.iter().all(|clip| !clip.is_loop()));
        assert_eq!(ClipId::ONE_SHOTS.len(), 12);
        assert_eq!(ClipId::LOOPS.len(), 4);
        assert_eq!(ClipId::from_name("rain_loop"), Some(ClipId::RainLoop));
        assert_eq!(ClipId::from_name("RainLoop"), None);
        assert_eq!(ClipId::from_name(""), None);
        assert_eq!(ClipKind::Loop.name(), "loop");
        assert_eq!(ClipKind::OneShot.name(), "one_shot");
    }

    #[test]
    fn rain_gains_crossfade_and_stay_bounded() {
        assert_eq!(rain_intensity_gains(0.0), (0.0, 0.0));
        let (light, heavy) = rain_intensity_gains(1.0);
        assert_eq!(light, 0.0);
        assert!((heavy - 0.90).abs() < 1e-6);
        let (middle_light, middle_heavy) = rain_intensity_gains(0.5);
        assert!(middle_light > 0.0 && middle_heavy > 0.0);
        assert!((middle_light - 0.65).abs() < 1e-6);
        for intensity in [-2.0, 0.0, 0.25, 0.7, 1.0, 5.0, f32::NAN, f32::INFINITY] {
            let (light, heavy) = rain_intensity_gains(intensity);
            assert!(light.is_finite() && (0.0..=0.65).contains(&light));
            assert!(heavy.is_finite() && (0.0..=0.90).contains(&heavy));
        }
        assert_eq!(rain_intensity_gains(f32::NAN), (0.0, 0.0));
    }

    #[test]
    fn thunder_and_explosion_cues_are_distance_aware_and_finite() {
        let near = thunder_cue(0.0);
        assert_eq!(near.clip, ClipId::ThunderNear);
        assert!((near.gain - 1.0).abs() < 1e-6);
        let close = thunder_cue(48.0);
        let distant = thunder_cue(160.0);
        let far = thunder_cue(400.0);
        let horizon = thunder_cue(512.0);
        assert_eq!(close.clip, ClipId::ThunderNear);
        assert_eq!(distant.clip, ClipId::ThunderDistant);
        assert_eq!(far.clip, ClipId::ThunderDistant);
        assert!(near.gain > close.gain && close.gain > distant.gain && distant.gain > far.gain);
        assert!(
            far.gain >= horizon.gain,
            "gain must stay monotonic past 320 m: {} vs {}",
            far.gain,
            horizon.gain
        );
        assert!(near.rate > close.rate && close.rate > distant.rate && distant.rate > far.rate);
        for distance in [-1.0, 0.0, 10.0, 64.0, 65.0, 512.0, 1_000.0, f32::NAN, f32::INFINITY] {
            let cue = thunder_cue(distance);
            assert!(cue.gain.is_finite() && (0.0..=1.0).contains(&cue.gain));
            assert!(cue.rate.is_finite() && (0.5..=2.0).contains(&cue.rate));
        }
        assert_eq!(thunder_cue(f32::NAN).clip, ClipId::ThunderDistant);
        assert_eq!(thunder_cue(f32::NAN).gain, thunder_cue(512.0).gain);

        let blast = explosion_cue(0.0);
        assert_eq!(blast.clip, ClipId::Explosion);
        assert!((blast.gain - 1.0).abs() < 1e-6);
        assert!(explosion_cue(12.0).gain > explosion_cue(48.0).gain);
        assert_eq!(explosion_cue(100.0).gain, 0.0);
        assert_eq!(explosion_cue(f32::NAN).gain, 0.0);
        assert!(explosion_cue(200.0).rate.is_finite());
    }

    #[test]
    fn cue_constructor_clamps_hostile_values() {
        let cue = ClipCue::new(ClipId::Explosion, f32::NAN, f32::NAN);
        assert_eq!(cue.gain, 0.0);
        assert_eq!(cue.rate, 1.0);
        let cue = ClipCue::new(ClipId::Explosion, 99.0, 99.0);
        assert_eq!(cue.gain, 4.0);
        assert_eq!(cue.rate, 2.0);
        let cue = ClipCue::new(ClipId::Explosion, -1.0, -1.0);
        assert_eq!(cue.gain, 0.0);
        assert_eq!(cue.rate, 0.5);
    }

    #[test]
    fn pan_helpers_are_bounded_constant_power_and_yaw_relative() {
        let (left, right) = constant_power_pan(0.0);
        assert!((left - right).abs() < 1e-6);
        assert!((left * left + right * right - 1.0).abs() < 1e-6);
        let (left, right) = constant_power_pan(-1.0);
        assert!((left - 1.0).abs() < 1e-6 && right.abs() < 1e-6);
        let (left, right) = constant_power_pan(1.0);
        assert!(left.abs() < 1e-6 && (right - 1.0).abs() < 1e-6);
        let (left, right) = constant_power_pan(5.0);
        assert!((left * left + right * right - 1.0).abs() < 1e-6);
        assert_eq!(constant_power_pan(f32::NAN), constant_power_pan(0.0));

        let listener = [10.0, 2.0, -4.0];
        assert!(world_pan(listener, 0.0, [14.0, 2.0, -4.0]) > 0.5);
        assert!(world_pan(listener, 0.0, [6.0, 2.0, -4.0]) < -0.5);
        assert_eq!(world_pan(listener, 0.0, listener), 0.0);
        assert_eq!(world_pan([f32::NAN; 3], 0.0, listener), 0.0);
        assert!(world_pan(listener, f32::NAN, [14.0, 2.0, -4.0]).is_finite());

        assert!((distance_attenuation(0.0, 24.0) - 1.0).abs() < 1e-6);
        assert_eq!(distance_attenuation(24.0, 24.0), 0.0);
        assert_eq!(distance_attenuation(48.0, 24.0), 0.0);
        assert_eq!(distance_attenuation(f32::NAN, 24.0), 0.0);
        assert_eq!(distance_attenuation(1.0, 0.0), 0.0);
        assert_eq!(distance_attenuation(1.0, f32::INFINITY), 0.0);
    }
}

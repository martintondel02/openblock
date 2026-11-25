//! Deterministic procedural synthesis of every sound the engine can play.
//!
//! There are no external assets. Each clip is generated from integer hashes
//! and plain arithmetic, so the same seed always produces the same PCM buffer
//! on every platform. Transcendental functions (`sin`, `exp`, `powf`) are
//! deliberately avoided in the generators: their last-bit behaviour differs
//! between libm implementations, which would make the determinism tests
//! platform-dependent.

use crate::{Material, SAMPLE_RATE, Sfx};

/// Number of material groups: footsteps, block breaks and block places.
const MATERIALS: usize = 7;
/// One-shot groups: pickup, hurt, death, eat, drink, craft, craft failure,
/// tool break, splash, UI click and UI confirm.
const ONESHOTS: usize = 11;
/// Total number of SFX groups (every variant of an [`Sfx`] lives in one group).
pub(crate) const GROUP_COUNT: usize = MATERIALS * 3 + ONESHOTS;

/// Number of variants stored for a group.
pub(crate) fn variant_count(group: usize) -> usize {
    match group {
        0..=6 => 4,   // footsteps
        7..=13 => 3,  // block break
        14..=20 => 2, // block place
        _ => match group - MATERIALS * 3 {
            1 | 3 | 8 => 2, // hurt, eat, splash
            _ => 1,
        },
    }
}

/// Index of the group that backs `sfx`.
pub(crate) fn group_of(sfx: Sfx) -> usize {
    match sfx {
        Sfx::Footstep(material) => material as usize,
        Sfx::BlockBreak(material) => MATERIALS + material as usize,
        Sfx::BlockPlace(material) => MATERIALS * 2 + material as usize,
        Sfx::Pickup => MATERIALS * 3,
        Sfx::Hurt => MATERIALS * 3 + 1,
        Sfx::Death => MATERIALS * 3 + 2,
        Sfx::Eat => MATERIALS * 3 + 3,
        Sfx::Drink => MATERIALS * 3 + 4,
        Sfx::Craft => MATERIALS * 3 + 5,
        Sfx::CraftFail => MATERIALS * 3 + 6,
        Sfx::ToolBreak => MATERIALS * 3 + 7,
        Sfx::Splash => MATERIALS * 3 + 8,
        Sfx::UiClick => MATERIALS * 3 + 9,
        Sfx::UiConfirm => MATERIALS * 3 + 10,
    }
}

/// Minimum spacing between two requests for the same group, in seconds.
///
/// This is the last line of defence against a machine-gun emitter; gameplay
/// call sites also apply their own cadence (footsteps in particular).
pub(crate) fn rate_limit_seconds(group: usize) -> f32 {
    match group {
        0..=6 => 0.12,
        7..=13 => 0.035,
        14..=20 => 0.04,
        _ => match group - MATERIALS * 3 {
            0 => 0.08, // pickup
            1 => 0.20, // hurt
            2 => 1.00, // death
            3 => 0.20, // eat
            4 => 0.25, // drink
            5 => 0.12, // craft
            6 => 0.15, // craft failure
            7 => 0.30, // tool break
            8 => 0.25, // splash
            9 => 0.03, // UI click
            _ => 0.08, // UI confirm
        },
    }
}

/// Immutable PCM data for one clip.
#[derive(Debug, Clone)]
pub(crate) struct Clip {
    /// Mono samples at [`SAMPLE_RATE`].
    pub samples: Box<[f32]>,
}

impl Clip {
    fn new(samples: Vec<f32>, peak: f32) -> Self {
        let mut samples = samples;
        normalize(&mut samples, peak);
        Self {
            samples: samples.into_boxed_slice(),
        }
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

/// Contiguous range of clips in [`ClipBank::clips`] for one [`Sfx`] group.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupRange {
    /// First clip index.
    pub start: usize,
    /// Number of variants.
    pub count: usize,
}

/// Every generated clip, immutable for the lifetime of the engine.
#[derive(Debug)]
pub(crate) struct ClipBank {
    /// One range per group, indexed like [`Sfx`].
    pub groups: Vec<GroupRange>,
    /// All SFX clips.
    pub clips: Vec<Clip>,
    /// Quiet wind/forest ambience loop.
    pub ambient: Clip,
    /// Soft generative music-pad loop.
    pub music: Clip,
}

impl ClipBank {
    /// Total number of generated clips, including the two loops.
    pub fn clip_count(&self) -> usize {
        self.clips.len() + 2
    }

    /// The group range for `group`, if it exists.
    pub fn group(&self, group: usize) -> Option<GroupRange> {
        self.groups.get(group).copied()
    }

    /// Clip by index; returns `None` for an out-of-range index.
    pub fn clip(&self, index: usize) -> Option<&Clip> {
        self.clips.get(index)
    }
}

/// Synthesises every clip at 48 kHz mono.
pub(crate) fn build_bank() -> ClipBank {
    let mut groups = Vec::with_capacity(GROUP_COUNT);
    let mut clips = Vec::new();

    for material in Material::ALL {
        add_group(
            &mut groups,
            &mut clips,
            (0..variant_count(group_of(Sfx::Footstep(material)))).map(|variant| footstep(material, variant)),
        );
    }
    for material in Material::ALL {
        add_group(
            &mut groups,
            &mut clips,
            (0..variant_count(group_of(Sfx::BlockBreak(material)))).map(|variant| block_break(material, variant)),
        );
    }
    for material in Material::ALL {
        add_group(
            &mut groups,
            &mut clips,
            (0..variant_count(group_of(Sfx::BlockPlace(material)))).map(|variant| block_place(material, variant)),
        );
    }

    add_group(&mut groups, &mut clips, [pickup()]);
    add_group(&mut groups, &mut clips, (0..2).map(hurt));
    add_group(&mut groups, &mut clips, [death()]);
    add_group(&mut groups, &mut clips, (0..2).map(eat));
    add_group(&mut groups, &mut clips, [drink()]);
    add_group(&mut groups, &mut clips, [craft()]);
    add_group(&mut groups, &mut clips, [craft_fail()]);
    add_group(&mut groups, &mut clips, [tool_break()]);
    add_group(&mut groups, &mut clips, (0..2).map(splash));
    add_group(&mut groups, &mut clips, [ui_click()]);
    add_group(&mut groups, &mut clips, [ui_confirm()]);

    debug_assert_eq!(groups.len(), GROUP_COUNT);
    debug_assert_eq!(groups.iter().map(|group| group.count).sum::<usize>(), clips.len());

    let ambient = Clip::new(make_loop(generate_ambient(), 0.40), 0.42);
    let music = Clip::new(make_loop(generate_music(), 0.65), 0.30);

    ClipBank {
        groups,
        clips,
        ambient,
        music,
    }
}

fn add_group(groups: &mut Vec<GroupRange>, clips: &mut Vec<Clip>, generated: impl IntoIterator<Item = Clip>) {
    let start = clips.len();
    clips.extend(generated);
    groups.push(GroupRange {
        start,
        count: clips.len() - start,
    });
}

// --- deterministic primitives -------------------------------------------------

/// SplitMix64 finaliser. Used for every random-ish decision in synthesis.
#[inline]
pub(crate) fn hash_u64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Uniform noise in `-1.0..=1.0` for a seed and sample index.
#[inline]
fn noise(seed: u64, index: u64) -> f32 {
    let bits = hash_u64(seed ^ index.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    ((bits >> 40) as f32) * (2.0 / 16_777_216.0) - 1.0
}

/// Triangle wave for a phase in `0.0..1.0`; starts at `-1`, peaks at `1`.
#[inline]
fn triangle(phase: f32) -> f32 {
    1.0 - 4.0 * (phase - 0.5).abs()
}

/// Quadratic decay envelope over the normalised position `t`.
#[inline]
fn decay(t: f32) -> f32 {
    let remaining = (1.0 - t).clamp(0.0, 1.0);
    remaining * remaining
}

/// Smoothstep over `0.0..1.0`.
#[inline]
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Advances a phase by `step` and wraps it into `0.0..1.0`.
#[inline]
fn advance(phase: &mut f32, step: f32) -> f32 {
    *phase += step;
    if *phase >= 1.0 {
        *phase -= 1.0;
    }
    *phase
}

/// Scales a buffer so its peak equals `peak`; non-finite samples become zero.
fn normalize(samples: &mut [f32], peak: f32) {
    let mut maximum = 0.0f32;
    for sample in samples.iter_mut() {
        if !sample.is_finite() {
            *sample = 0.0;
        }
        maximum = maximum.max(sample.abs());
    }
    if maximum <= f32::EPSILON {
        samples.fill(0.0);
        return;
    }
    let scale = peak.clamp(0.0, 1.0) / maximum;
    for sample in samples.iter_mut() {
        *sample = (*sample * scale).clamp(-1.0, 1.0);
    }
}

/// Blends the tail of a buffer into its head and truncates it, producing a loop
/// whose seam is close to continuous.
fn make_loop(mut samples: Vec<f32>, crossfade_seconds: f32) -> Vec<f32> {
    let fade = ((crossfade_seconds * SAMPLE_RATE as f32) as usize).min(samples.len() / 3);
    if fade == 0 {
        return samples;
    }
    let length = samples.len() - fade;
    for index in 0..fade {
        let blend = index as f32 / fade as f32;
        let head = samples[index];
        let tail = samples[length + index];
        samples[index] = head * (1.0 - blend) + tail * blend;
    }
    samples.truncate(length);
    samples
}

/// Filter and timbre profile shared by footsteps, breaks and places.
#[derive(Clone, Copy)]
struct Profile {
    /// One-pole low-pass coefficient; higher is brighter.
    alpha: f32,
    /// Gain of the low-passed noise body.
    noise_gain: f32,
    /// Gain of the high-passed noise (the "grain").
    bright_gain: f32,
    /// Low thump frequency in Hz.
    body_freq: f32,
    /// Low thump gain.
    body_gain: f32,
    /// Short transient click gain.
    click_gain: f32,
}

fn material_profile(material: Material) -> Profile {
    match material {
        Material::Grass => Profile {
            alpha: 0.18,
            noise_gain: 0.75,
            bright_gain: 0.12,
            body_freq: 0.0,
            body_gain: 0.0,
            click_gain: 0.0,
        },
        Material::Dirt => Profile {
            alpha: 0.12,
            noise_gain: 0.82,
            bright_gain: 0.06,
            body_freq: 96.0,
            body_gain: 0.16,
            click_gain: 0.02,
        },
        Material::Stone => Profile {
            alpha: 0.45,
            noise_gain: 0.58,
            bright_gain: 0.34,
            body_freq: 168.0,
            body_gain: 0.12,
            click_gain: 0.34,
        },
        Material::Wood => Profile {
            alpha: 0.20,
            noise_gain: 0.46,
            bright_gain: 0.18,
            body_freq: 122.0,
            body_gain: 0.36,
            click_gain: 0.10,
        },
        Material::Sand => Profile {
            alpha: 0.55,
            noise_gain: 0.52,
            bright_gain: 0.44,
            body_freq: 0.0,
            body_gain: 0.0,
            click_gain: 0.02,
        },
        Material::Snow => Profile {
            alpha: 0.30,
            noise_gain: 0.68,
            bright_gain: 0.24,
            body_freq: 0.0,
            body_gain: 0.0,
            click_gain: 0.04,
        },
        Material::Gravel => Profile {
            alpha: 0.35,
            noise_gain: 0.64,
            bright_gain: 0.30,
            body_freq: 0.0,
            body_gain: 0.0,
            click_gain: 0.12,
        },
    }
}

// --- footsteps, blocks --------------------------------------------------------

fn footstep(material: Material, variant: usize) -> Clip {
    let profile = material_profile(material);
    let seed =
        0x4f6f_6f74_0000_0000 ^ (material as u64).wrapping_mul(0x9e37_79b9) ^ ((variant as u64 + 1) * 0x85eb_ca6b);
    let duration = 0.11 + 0.018 * variant as f32 + if material == Material::Snow { 0.02 } else { 0.0 };
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = ((hash_u64(seed) >> 32) as f32) / 4_294_967_296.0;
    let body_step = profile.body_freq * (1.0 + 0.035 * variant as f32) / SAMPLE_RATE as f32;
    let click_samples = (0.002 * SAMPLE_RATE as f32) as usize + 1;

    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(seed, index as u64);
        low += (white - low) * profile.alpha;
        let bright = white - low;
        let envelope = decay(t);
        let mut value = (low * profile.noise_gain + bright * profile.bright_gain) * envelope;
        if profile.body_gain > 0.0 {
            let body = triangle(advance(&mut phase, body_step));
            value += body * profile.body_gain * envelope * envelope;
        }
        if index < click_samples {
            let click = 1.0 - index as f32 / click_samples as f32;
            value += click * profile.click_gain * (0.5 + 0.5 * noise(seed ^ 0x51c1, index as u64));
        }
        *sample = value;
    }
    Clip::new(samples, 0.55)
}

fn block_break(material: Material, variant: usize) -> Clip {
    let profile = material_profile(material);
    let seed =
        0x6272_6561_6b00_0000 ^ (material as u64).wrapping_mul(0xc2b2_ae35) ^ ((variant as u64 + 3) * 0x27d4_eb2f);
    let duration = 0.22 + 0.045 * variant as f32;
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = ((hash_u64(seed) >> 32) as f32) / 4_294_967_296.0;
    let body_step = profile.body_freq * (1.0 - 0.06 * variant as f32) / SAMPLE_RATE as f32;

    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(seed, index as u64);
        low += (white - low) * profile.alpha;
        let bright = white - low;
        let envelope = decay(t);
        // A short crack at the start and a few granular impulses later make
        // stone and gravel read as hard, brittle materials.
        let mut value = (low * profile.noise_gain * 1.15 + bright * (profile.bright_gain + 0.18)) * envelope;
        if profile.click_gain > 0.0 {
            let grain = noise(seed ^ 0xa73c, (index / 37) as u64);
            if grain > 0.55 {
                let impulse = (index % 37) as f32 / 37.0;
                value += profile.click_gain * (1.0 - impulse) * envelope;
            }
        }
        if profile.body_gain > 0.0 {
            let body = triangle(advance(&mut phase, body_step));
            value += body * profile.body_gain * 1.2 * envelope * envelope;
        }
        *sample = value;
    }
    Clip::new(samples, 0.82)
}

fn block_place(material: Material, variant: usize) -> Clip {
    let profile = material_profile(material);
    let seed =
        0x706c_6163_6500_0000 ^ (material as u64).wrapping_mul(0x1656_67b1) ^ ((variant as u64 + 5) * 0x2545_f491);
    let duration = 0.10 + 0.02 * variant as f32;
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = ((hash_u64(seed) >> 32) as f32) / 4_294_967_296.0;
    let body_step = (profile.body_freq.max(70.0) * 0.8) / SAMPLE_RATE as f32;

    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(seed, index as u64);
        low += (white - low) * profile.alpha.max(0.18);
        let envelope = decay(t);
        let mut value = low * 0.85 * envelope + (white - low) * profile.bright_gain * 0.7 * envelope;
        let body = triangle(advance(&mut phase, body_step));
        value += body * (profile.body_gain.max(0.12) + 0.16) * envelope;
        *sample = value;
    }
    Clip::new(samples, 0.60)
}

// --- one-shots ----------------------------------------------------------------

fn pickup() -> Clip {
    let notes = [880.0f32, 1318.51];
    let note_seconds = 0.075;
    let total = note_seconds * notes.len() as f32 + 0.04;
    let count = (total * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    for (note_index, frequency) in notes.iter().enumerate() {
        let start = (note_index as f32 * note_seconds * SAMPLE_RATE as f32) as usize;
        let note_count = (note_seconds * SAMPLE_RATE as f32) as usize;
        let mut phase = 0.25 * note_index as f32;
        for local in 0..note_count {
            let index = start + local;
            if index >= count {
                break;
            }
            let t = local as f32 / note_count as f32;
            let envelope = (1.0 - t) * (1.0 - t);
            let step = frequency / SAMPLE_RATE as f32;
            let fundamental = triangle(advance(&mut phase, step));
            let harmonic = triangle((phase * 2.0).fract());
            samples[index] += (fundamental * 0.38 + harmonic * 0.08) * envelope;
        }
    }
    Clip::new(samples, 0.50)
}

fn hurt(variant: usize) -> Clip {
    let seed = 0x6875_7274_0000_0000 ^ ((variant as u64 + 1) * 0x9e37_79b9);
    let duration = 0.24;
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let envelope = decay(t);
        let white = noise(seed, index as u64);
        low += (white - low) * 0.35;
        let frequency = 250.0 - 130.0 * t + 12.0 * variant as f32;
        let tone = triangle(advance(&mut phase, frequency / SAMPLE_RATE as f32));
        *sample = (low * 0.55 + tone * 0.40) * envelope;
    }
    Clip::new(samples, 0.72)
}

fn death() -> Clip {
    let count = (0.95 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let envelope = decay(t) * (0.4 + 0.6 * (1.0 - t));
        let white = noise(0x6465_6174_6800_0001, index as u64);
        low += (white - low) * 0.06;
        let frequency = 190.0 - 140.0 * t;
        let tone = triangle(advance(&mut phase, frequency / SAMPLE_RATE as f32));
        let sub = triangle((phase * 0.5).fract());
        *sample = (tone * 0.38 + sub * 0.20 + low * 0.35) * envelope;
    }
    Clip::new(samples, 0.80)
}

fn eat(variant: usize) -> Clip {
    let seed = 0x6561_7400_0000_0000 ^ ((variant as u64 + 1) * 0x85eb_ca6b);
    let count = (0.22 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(seed, index as u64);
        low += (white - low) * 0.22;
        // Two crunch bursts.
        let burst = |centre: f32, width: f32| {
            let distance = (t - centre).abs();
            if distance < width {
                let x = 1.0 - distance / width;
                x * x
            } else {
                0.0
            }
        };
        let envelope = burst(0.10, 0.10).max(burst(0.55, 0.12));
        *sample = (low * 0.75 + (white - low) * 0.25) * envelope;
    }
    Clip::new(samples, 0.48)
}

fn drink() -> Clip {
    let count = (0.25 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let pulse = (t * 3.0).fract();
        let envelope = if pulse < 0.5 {
            (pulse / 0.5).min(1.0) * (1.0 - pulse / 0.5)
        } else {
            0.0
        };
        let frequency = 190.0 - 40.0 * pulse;
        let tone = triangle(advance(&mut phase, frequency / SAMPLE_RATE as f32));
        let noise = noise(0x6472_696e_6b00_0001, (index / 11) as u64);
        *sample = (tone * 0.30 + noise * 0.10) * envelope;
    }
    Clip::new(samples, 0.50)
}

fn craft() -> Clip {
    let notes = [523.25f32, 659.25, 783.99];
    let note_seconds = 0.075;
    let count = (0.30 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phase = 0.0f32;
    for (local, sample) in samples.iter_mut().enumerate() {
        let t = local as f32 / SAMPLE_RATE as f32;
        let note_index = (t / note_seconds).min(notes.len() as f32 - 1.0) as usize;
        let note_t = (t - note_index as f32 * note_seconds) / note_seconds;
        let envelope = if note_t < 1.0 {
            (1.0 - note_t) * (1.0 - note_t)
        } else {
            0.0
        };
        let tone = triangle(advance(&mut phase, notes[note_index] / SAMPLE_RATE as f32));
        *sample = tone * envelope;
    }
    Clip::new(samples, 0.50)
}

fn craft_fail() -> Clip {
    let count = (0.18 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let envelope = decay(t);
        let tone = triangle(advance(&mut phase, 155.0 / SAMPLE_RATE as f32));
        let edge = if tone >= 0.0 { 0.65 } else { -0.65 };
        *sample = (tone * 0.45 + edge * 0.10) * envelope;
    }
    Clip::new(samples, 0.42)
}

fn tool_break() -> Clip {
    let count = (0.32 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut ping_phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(0x746f_6f6c_0000_0001, index as u64);
        low += (white - low) * 0.55;
        let envelope = decay(t);
        let ping = triangle(advance(&mut ping_phase, 1180.0 / SAMPLE_RATE as f32));
        let ping_env = (1.0 - (t * 7.0).min(1.0)) * (1.0 - (t * 7.0).min(1.0));
        *sample = (low * 0.55 + white * 0.18) * envelope + ping * 0.22 * ping_env;
    }
    Clip::new(samples, 0.68)
}

fn splash(variant: usize) -> Clip {
    let seed = 0x7370_6c61_7368_0000 ^ ((variant as u64 + 1) * 0x27d4_eb2f);
    let count = (0.36 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let white = noise(seed, index as u64);
        low += (white - low) * (0.12 + 0.45 * (1.0 - t));
        let envelope = decay(t);
        let frequency = 320.0 - 210.0 * t;
        let bloop = triangle(advance(&mut phase, frequency / SAMPLE_RATE as f32));
        *sample = (low * 0.80 + bloop * 0.16) * envelope;
    }
    Clip::new(samples, 0.62)
}

fn ui_click() -> Clip {
    let count = (0.035 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let envelope = (1.0 - t) * (1.0 - t);
        let tone = triangle(advance(&mut phase, 1800.0 / SAMPLE_RATE as f32));
        *sample = tone * envelope;
    }
    Clip::new(samples, 0.22)
}

fn ui_confirm() -> Clip {
    let count = (0.12 * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phase = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / count as f32;
        let (frequency, local) = if t < 0.5 {
            (880.0, t * 2.0)
        } else {
            (1318.51, (t - 0.5) * 2.0)
        };
        let envelope = (1.0 - local) * (1.0 - local);
        let tone = triangle(advance(&mut phase, frequency / SAMPLE_RATE as f32));
        *sample = tone * envelope;
    }
    Clip::new(samples, 0.30)
}

// --- loops --------------------------------------------------------------------

fn generate_ambient() -> Vec<f32> {
    let duration = 12.0;
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut low = 0.0f32;
    let mut low2 = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / SAMPLE_RATE as f32;
        let white = noise(0xa11b_0000_0000_0001, index as u64);
        low += (white - low) * 0.020;
        low2 += (low - low2) * 0.012;
        let gust = 0.58 + 0.25 * triangle((0.071 * t).fract()) + 0.14 * triangle((0.113 * t + 0.37).fract());
        let rustle = (white - low) * 0.055 * (0.5 + 0.5 * triangle((0.19 * t).fract()));
        *sample = low2 * 0.85 * gust + rustle;
    }
    samples
}

fn generate_music() -> Vec<f32> {
    let duration = 16.0;
    let chord_seconds = 4.0;
    let chords = [
        [220.00f32, 261.63, 329.63],
        [174.61, 220.00, 261.63],
        [130.81, 164.81, 196.00],
        [196.00, 246.94, 293.66],
    ];
    let count = (duration * SAMPLE_RATE as f32) as usize;
    let mut samples = vec![0.0f32; count];
    let mut phases = [0.0f32; 3];
    let mut sub_phase = 0.0f32;
    let mut air = 0.0f32;
    for (index, sample) in samples.iter_mut().enumerate() {
        let t = index as f32 / SAMPLE_RATE as f32;
        let chord_index = ((t / chord_seconds) as usize).min(chords.len() - 1);
        let within = (t % chord_seconds) / chord_seconds;
        let attack = smoothstep((within / 0.16).min(1.0));
        let release = smoothstep(((1.0 - within) / 0.22).min(1.0));
        let envelope = attack * release;
        let chord = chords[chord_index];
        let mut value = 0.0f32;
        for (voice, frequency) in chord.iter().enumerate() {
            let tone = triangle(advance(&mut phases[voice], frequency / SAMPLE_RATE as f32));
            value += tone * 0.22;
        }
        let root = chord[0] * 0.5;
        value += triangle(advance(&mut sub_phase, root / SAMPLE_RATE as f32)) * 0.10;
        air += (noise(0x6d75_7369_6300_0001, index as u64) - air) * 0.004;
        value += air * 0.05;
        *sample = value * envelope;
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> ClipBank {
        build_bank()
    }

    fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash
    }

    fn bank_hash(bank: &ClipBank) -> u64 {
        let mut bytes = Vec::new();
        for clip in bank.clips.iter().chain([&bank.ambient, &bank.music]) {
            for sample in clip.samples.iter() {
                bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
            }
        }
        fnv1a(bytes)
    }

    #[test]
    fn every_sfx_variant_exists_and_is_short_enough() {
        let bank = bank();
        let all = Material::ALL
            .into_iter()
            .flat_map(|material| {
                [
                    Sfx::Footstep(material),
                    Sfx::BlockBreak(material),
                    Sfx::BlockPlace(material),
                ]
            })
            .chain([
                Sfx::Pickup,
                Sfx::Hurt,
                Sfx::Death,
                Sfx::Eat,
                Sfx::Drink,
                Sfx::Craft,
                Sfx::CraftFail,
                Sfx::ToolBreak,
                Sfx::Splash,
                Sfx::UiClick,
                Sfx::UiConfirm,
            ]);
        for sfx in all {
            let group = bank.group(group_of(sfx)).expect("group exists");
            assert_eq!(group.count, variant_count(group_of(sfx)), "{sfx:?}");
            assert!(group.count > 0);
            assert!(group.start + group.count <= bank.clips.len());
            for clip in &bank.clips[group.start..group.start + group.count] {
                assert!(clip.len() > 16, "{sfx:?} clip is too short");
                assert!(
                    clip.len() <= (2.5 * SAMPLE_RATE as f32) as usize,
                    "{sfx:?} clip is longer than 2.5 s"
                );
                assert!(clip.samples.iter().all(|sample| sample.is_finite()));
                assert!(clip.samples.iter().all(|sample| sample.abs() <= 1.0));
                assert!(
                    clip.samples.iter().any(|sample| sample.abs() > 1.0e-4),
                    "{sfx:?} variant is silent"
                );
            }
        }
        assert!(bank.ambient.len() > SAMPLE_RATE as usize * 8);
        assert!(bank.music.len() > SAMPLE_RATE as usize * 12);
        assert!(bank.ambient.samples.iter().any(|sample| sample.abs() > 1.0e-4));
        assert!(bank.music.samples.iter().any(|sample| sample.abs() > 1.0e-4));
    }

    #[test]
    fn synthesis_is_deterministic() {
        let first = bank_hash(&bank());
        let second = bank_hash(&bank());
        assert_eq!(first, second);
        // A second build must be byte-identical, not merely similar.
        let a = bank();
        let b = bank();
        for (left, right) in a.clips.iter().zip(&b.clips) {
            assert_eq!(left.samples.as_ref(), right.samples.as_ref());
        }
        assert_eq!(a.ambient.samples.as_ref(), b.ambient.samples.as_ref());
        assert_eq!(a.music.samples.as_ref(), b.music.samples.as_ref());
    }

    #[test]
    fn noise_and_phase_helpers_are_in_range_and_bounded() {
        let mut phase = 0.0;
        for index in 0..10_000u64 {
            let value = noise(0x1234_5678, index);
            assert!((-1.0..=1.0).contains(&value));
            let wave = triangle(advance(&mut phase, 0.137));
            assert!((-1.0..=1.0).contains(&wave));
        }
    }

    #[test]
    fn groups_cover_every_sfx_and_variant_counts_are_nonzero() {
        let bank = bank();
        assert_eq!(bank.groups.len(), GROUP_COUNT);
        for group in 0..GROUP_COUNT {
            let range = bank.group(group).expect("group");
            assert_eq!(range.count, variant_count(group), "group {group}");
            assert!(range.start + range.count <= bank.clips.len());
        }
    }
}

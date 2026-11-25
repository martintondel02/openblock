//! Real-time mixer: a fixed voice pool, a bounded command queue and the two
//! ambient loops.
//!
//! The audio writer thread owns one [`Mixer`]. The mixer never allocates, never
//! logs, never performs I/O and never blocks: commands are drained from a
//! mutex-protected ring with `try_lock`, and a contended or empty queue simply
//! means no new sounds this block. The main thread only pushes small `Copy`
//! commands and writes atomic gains; the backend's blocking write happens after
//! the mixer returns.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use crate::synth::GroupRange;
use crate::synth::{Clip, ClipBank, group_of, hash_u64, rate_limit_seconds};
use crate::{MAX_VOICES, SAMPLE_RATE, Sfx};

/// Capacity of the main-thread to audio-thread command ring.
pub(crate) const COMMAND_CAPACITY: usize = 256;
/// Maximum number of commands consumed in one mixer block.
const COMMAND_DRAIN_LIMIT: usize = 64;
/// One-pole gain smoothing per sample. At 48 kHz this is roughly a 15 ms glide.
const GAIN_SMOOTHING: f32 = 0.0015;
/// Relative level of the music bus before the music volume setting.
const MUSIC_BUS: f32 = 0.18;
/// Ambient bed level relative to the music pad.
const AMBIENT_BUS: f32 = 0.75;
/// Per-sample smoothing of the music/ambience fade gate (about 1.5 s to settle).
const MUSIC_FADE_SMOOTHING: f32 = 0.00004;

/// One sound request copied from the main thread to the audio thread.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlayCommand {
    /// Group index (see `synth::group_of`).
    pub group: u16,
    /// Seed selecting the variant and small pitch variation.
    pub seed: u32,
    /// Linear gain before category and master volumes.
    pub gain: f32,
    /// Stereo pan in `-1.0..=1.0`.
    pub pan: f32,
    /// Playback-rate multiplier for per-call pitch variation.
    pub rate: f32,
}

impl PlayCommand {
    const SILENT: Self = Self {
        group: 0,
        seed: 0,
        gain: 0.0,
        pan: 0.0,
        rate: 1.0,
    };
}

/// Shared, lock-free scalar state plus counters observable from the main
/// thread.
#[derive(Debug)]
pub(crate) struct Control {
    master: AtomicU32,
    sfx: AtomicU32,
    music: AtomicU32,
    enabled: AtomicBool,
    active_voices: AtomicUsize,
    accepted_commands: AtomicU64,
    dropped_commands: AtomicU64,
    device_errors: AtomicUsize,
}

impl Control {
    pub(crate) fn new(master: f32, sfx: f32, music: f32) -> Self {
        Self {
            master: AtomicU32::new(sanitise_gain(master).to_bits()),
            sfx: AtomicU32::new(sanitise_gain(sfx).to_bits()),
            music: AtomicU32::new(sanitise_gain(music).to_bits()),
            enabled: AtomicBool::new(true),
            active_voices: AtomicUsize::new(0),
            accepted_commands: AtomicU64::new(0),
            dropped_commands: AtomicU64::new(0),
            device_errors: AtomicUsize::new(0),
        }
    }

    pub(crate) fn set_master(&self, value: f32) {
        self.master.store(sanitise_gain(value).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_sfx(&self, value: f32) {
        self.sfx.store(sanitise_gain(value).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_music(&self, value: f32) {
        self.music.store(sanitise_gain(value).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn master(&self) -> f32 {
        volume(&self.master)
    }

    pub(crate) fn sfx(&self) -> f32 {
        volume(&self.sfx)
    }

    pub(crate) fn music(&self) -> f32 {
        volume(&self.music)
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Voices active after the most recent mixer block.
    pub(crate) fn active_voices(&self) -> usize {
        self.active_voices.load(Ordering::Relaxed)
    }

    /// Commands accepted by the queue.
    pub(crate) fn accepted_commands(&self) -> u64 {
        self.accepted_commands.load(Ordering::Relaxed)
    }

    /// Commands dropped because the ring was full.
    pub(crate) fn dropped_commands(&self) -> u64 {
        self.dropped_commands.load(Ordering::Relaxed)
    }

    /// Write failures reported by the Linux writer thread.
    pub(crate) fn device_errors(&self) -> usize {
        self.device_errors.load(Ordering::Relaxed)
    }

    /// Records one output write failure reported by the Linux writer thread.
    #[cfg(target_os = "linux")]
    pub(crate) fn record_device_error(&self) {
        self.device_errors.fetch_add(1, Ordering::Relaxed);
    }
}

fn volume(value: &AtomicU32) -> f32 {
    sanitise_gain(f32::from_bits(value.load(Ordering::Relaxed)))
}

/// Clamps an externally supplied gain to `0.0..=1.0`, mapping non-finite
/// values to zero.
pub(crate) fn sanitise_gain(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 }
}

/// Bounded single-producer / single-consumer command queue.
///
/// The consumer uses `try_lock`, so a main-thread push can never stall the
/// writer thread. A full ring drops the newest command and counts it.
pub(crate) struct CommandQueue {
    inner: Mutex<Ring>,
}

impl CommandQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Ring::new()),
        }
    }

    pub(crate) fn push(&self, command: PlayCommand, control: &Control) {
        let Ok(mut ring) = self.inner.lock() else {
            return;
        };
        if ring.len == COMMAND_CAPACITY {
            control.dropped_commands.fetch_add(1, Ordering::Relaxed);
            return;
        }
        ring.push(command);
        control.accepted_commands.fetch_add(1, Ordering::Relaxed);
    }

    /// Copies up to `out.len()` commands; never blocks.
    pub(crate) fn drain(&self, out: &mut [PlayCommand]) -> usize {
        let Ok(mut ring) = self.inner.try_lock() else {
            return 0;
        };
        let mut count = 0;
        while count < out.len() {
            let Some(command) = ring.pop() else {
                break;
            };
            out[count] = command;
            count += 1;
        }
        count
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.inner.lock().map(|ring| ring.len).unwrap_or(0)
    }
}

struct Ring {
    slots: [PlayCommand; COMMAND_CAPACITY],
    read: usize,
    write: usize,
    len: usize,
}

impl Ring {
    fn new() -> Self {
        Self {
            slots: [PlayCommand::SILENT; COMMAND_CAPACITY],
            read: 0,
            write: 0,
            len: 0,
        }
    }

    fn push(&mut self, command: PlayCommand) {
        self.slots[self.write] = command;
        self.write = (self.write + 1) % COMMAND_CAPACITY;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<PlayCommand> {
        if self.len == 0 {
            return None;
        }
        let command = self.slots[self.read];
        self.read = (self.read + 1) % COMMAND_CAPACITY;
        self.len -= 1;
        Some(command)
    }
}

/// One active sample voice.
#[derive(Clone, Copy, Debug)]
struct Voice {
    clip: usize,
    position: f64,
    step: f64,
    gain: f32,
    pan_left: f32,
    pan_right: f32,
    finished: bool,
}

/// The audio-thread mixer. Everything it needs is preallocated.
pub(crate) struct Mixer {
    bank: Arc<ClipBank>,
    control: Arc<Control>,
    queue: Arc<CommandQueue>,
    device_rate: f64,
    voices: Vec<Voice>,
    ambient_position: f64,
    music_position: f64,
    master: f32,
    sfx: f32,
    music: f32,
    music_fade: f32,
}

impl Mixer {
    pub(crate) fn new(bank: Arc<ClipBank>, control: Arc<Control>, queue: Arc<CommandQueue>, device_rate: f64) -> Self {
        let device_rate = if device_rate.is_finite() && device_rate > 1.0 {
            device_rate
        } else {
            f64::from(SAMPLE_RATE)
        };
        Self {
            bank,
            control,
            queue,
            device_rate,
            voices: Vec::with_capacity(MAX_VOICES),
            ambient_position: 0.0,
            music_position: 0.0,
            master: 0.0,
            sfx: 0.0,
            music: 0.0,
            music_fade: 0.0,
        }
    }

    /// Mixes one interleaved output buffer. No allocation or blocking occurs.
    pub(crate) fn render(&mut self, data: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        if data.is_empty() {
            return;
        }
        let frames = data.len() / channels;
        let mut commands = [PlayCommand::SILENT; COMMAND_DRAIN_LIMIT];
        let count = self.queue.drain(&mut commands);
        for command in &commands[..count] {
            self.start_voice(command);
        }

        let master_target = if self.control.enabled() {
            self.control.master()
        } else {
            0.0
        };
        let sfx_target = self.control.sfx();
        let music_target = self.control.music();
        let music_fade_target = if self.control.enabled() && music_target > 0.0 {
            1.0
        } else {
            0.0
        };
        let bank: &ClipBank = &self.bank;
        let rate_ratio = f64::from(SAMPLE_RATE) / self.device_rate;

        for frame in 0..frames {
            self.master += (master_target - self.master) * GAIN_SMOOTHING;
            self.sfx += (sfx_target - self.sfx) * GAIN_SMOOTHING;
            self.music += (music_target - self.music) * GAIN_SMOOTHING;
            self.music_fade += (music_fade_target - self.music_fade) * MUSIC_FADE_SMOOTHING;

            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for voice in &mut self.voices {
                if voice.finished {
                    continue;
                }
                let Some(clip) = bank.clip(voice.clip) else {
                    voice.finished = true;
                    continue;
                };
                let length = clip.len();
                let index = voice.position.floor() as usize;
                if index >= length {
                    voice.finished = true;
                    continue;
                }
                let sample = sample_at(clip, voice.position);
                let attack = (voice.position / (0.003 * f64::from(SAMPLE_RATE))).min(1.0) as f32;
                let release =
                    (((length as f64) - voice.position) / (0.010 * f64::from(SAMPLE_RATE))).clamp(0.0, 1.0) as f32;
                let envelope = attack.min(release);
                let value = sample * envelope * voice.gain;
                left += value * voice.pan_left;
                right += value * voice.pan_right;
                voice.position += voice.step;
            }
            self.voices.retain(|voice| !voice.finished);

            let music_scale = self.master * self.music * self.music_fade * MUSIC_BUS;
            let mut music_left = 0.0f32;
            let mut music_right = 0.0f32;
            if music_scale > 0.0 {
                let ambient = sample_loop(&bank.ambient, &mut self.ambient_position, rate_ratio);
                let pad = sample_loop(&bank.music, &mut self.music_position, rate_ratio);
                music_left = (ambient * AMBIENT_BUS + pad) * music_scale;
                music_right = music_left;
            } else {
                // Keep both loops advancing so re-enabling resumes the bed
                // rather than restarting a click.
                advance_loop(&bank.ambient, &mut self.ambient_position, rate_ratio);
                advance_loop(&bank.music, &mut self.music_position, rate_ratio);
            }

            let sfx_scale = self.master * self.sfx;
            let output_left = soft_limit(left * sfx_scale + music_left);
            let output_right = soft_limit(right * sfx_scale + music_right);
            let base = frame * channels;
            if channels == 1 {
                data[base] = (output_left + output_right) * 0.5;
            } else {
                data[base] = output_left;
                data[base + 1] = output_right;
                for channel in data.iter_mut().skip(base + 2).take(channels - 2) {
                    *channel = (output_left + output_right) * 0.5;
                }
            }
        }

        self.control.active_voices.store(self.voices.len(), Ordering::Relaxed);
    }

    fn start_voice(&mut self, command: &PlayCommand) {
        let Some(group) = self.bank.group(command.group as usize) else {
            return;
        };
        if group.count == 0 {
            return;
        }
        let variant = (hash_u64(u64::from(command.seed) ^ u64::from(command.group)) as usize) % group.count;
        let clip_index = group.start + variant;
        if self.bank.clip(clip_index).is_none() {
            return;
        }
        let gain = if command.gain.is_finite() {
            command.gain.clamp(0.0, 4.0)
        } else {
            0.0
        };
        if gain <= 0.0 {
            return;
        }
        let pan = if command.pan.is_finite() {
            command.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let pan_left = ((1.0 - pan) * 0.5).sqrt();
        let pan_right = ((1.0 + pan) * 0.5).sqrt();
        let rate = if command.rate.is_finite() {
            command.rate.clamp(0.5, 2.0)
        } else {
            1.0
        };
        let step = (f64::from(SAMPLE_RATE) / self.device_rate) * f64::from(rate);
        let voice = Voice {
            clip: clip_index,
            position: 0.0,
            step,
            gain,
            pan_left,
            pan_right,
            finished: false,
        };
        if self.voices.len() < MAX_VOICES {
            self.voices.push(voice);
            return;
        }
        // Voice stealing: replace the quietest voice (ties: oldest). A new
        // loud effect is more valuable than a nearly inaudible tail.
        let mut victim = 0;
        let mut victim_gain = f32::INFINITY;
        for (index, existing) in self.voices.iter().enumerate() {
            if existing.gain < victim_gain {
                victim = index;
                victim_gain = existing.gain;
            }
        }
        if let Some(slot) = self.voices.get_mut(victim) {
            *slot = voice;
        }
    }
}

#[inline]
fn sample_at(clip: &Clip, position: f64) -> f32 {
    let index = position.floor() as usize;
    let Some(&first) = clip.samples.get(index) else {
        return 0.0;
    };
    let next = clip.samples.get(index + 1).copied().unwrap_or(first);
    let fraction = (position - index as f64) as f32;
    first + (next - first) * fraction
}

#[inline]
fn sample_loop(clip: &Clip, position: &mut f64, rate_ratio: f64) -> f32 {
    let length = clip.len();
    if length == 0 {
        return 0.0;
    }
    let sample = sample_at(clip, *position);
    *position += rate_ratio;
    if *position >= length as f64 {
        *position -= length as f64;
    }
    sample
}

#[inline]
fn advance_loop(clip: &Clip, position: &mut f64, rate_ratio: f64) {
    let length = clip.len();
    if length == 0 {
        *position = 0.0;
        return;
    }
    *position += rate_ratio;
    if *position >= length as f64 {
        *position -= length as f64;
    }
}

/// Soft limiter used on the final stereo pair.
///
/// Linear below `0.7`, then a compressive curve approaching `1.0`; the final
/// clamp is a hard guarantee that no sample can exceed full scale.
#[inline]
pub(crate) fn soft_limit(sample: f32) -> f32 {
    if !sample.is_finite() {
        return 0.0;
    }
    let magnitude = sample.abs();
    let limited = if magnitude <= 0.7 {
        sample
    } else {
        let excess = magnitude - 0.7;
        let compressed = 0.7 + 0.3 * (excess / (excess + 0.3));
        compressed.copysign(sample)
    };
    limited.clamp(-1.0, 1.0)
}

/// Convenience for tests and the engine: the clip group and cooldown for an
/// `Sfx`, without exposing the synth module.
pub(crate) fn group_info(sfx: Sfx) -> (usize, f32) {
    let group = group_of(sfx);
    (group, rate_limit_seconds(group))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bank() -> Arc<ClipBank> {
        let clip = Clip {
            samples: vec![0.25; 4800].into_boxed_slice(),
        };
        let groups = (0..crate::synth::GROUP_COUNT)
            .map(|_| GroupRange { start: 0, count: 1 })
            .collect();
        Arc::new(ClipBank {
            groups,
            clips: vec![clip.clone()],
            ambient: clip.clone(),
            music: clip,
        })
    }

    fn mixer() -> (Mixer, Arc<Control>, Arc<CommandQueue>) {
        let control = Arc::new(Control::new(1.0, 1.0, 0.0));
        let queue = Arc::new(CommandQueue::new());
        let mixer = Mixer::new(test_bank(), Arc::clone(&control), Arc::clone(&queue), 48_000.0);
        (mixer, control, queue)
    }

    fn command(gain: f32) -> PlayCommand {
        PlayCommand {
            group: 0,
            seed: 7,
            gain,
            pan: 0.0,
            rate: 1.0,
        }
    }

    #[test]
    fn gains_are_clamped_and_non_finite_becomes_zero() {
        let control = Control::new(0.8, 1.0, 0.55);
        assert!((control.master() - 0.8).abs() < 1e-6);
        control.set_master(2.0);
        assert!((control.master() - 1.0).abs() < 1e-6);
        control.set_sfx(-1.0);
        assert_eq!(control.sfx(), 0.0);
        control.set_music(f32::NAN);
        assert_eq!(control.music(), 0.0);
        control.set_enabled(false);
        assert!(!control.enabled());
    }

    #[test]
    fn command_ring_is_bounded_and_drains_in_order() {
        let control = Control::new(0.8, 1.0, 0.55);
        let queue = CommandQueue::new();
        for index in 0..(COMMAND_CAPACITY + 10) {
            let mut command = command(1.0);
            command.seed = index as u32;
            queue.push(command, &control);
        }
        assert_eq!(queue.queued(), COMMAND_CAPACITY);
        assert_eq!(control.accepted_commands(), COMMAND_CAPACITY as u64);
        assert_eq!(control.dropped_commands(), 10);
        let mut out = [PlayCommand::SILENT; 4];
        assert_eq!(queue.drain(&mut out), 4);
        assert_eq!(out[0].seed, 0);
        assert_eq!(out[3].seed, 3);
    }

    #[test]
    fn limiter_is_bounded_and_continuous_at_zero() {
        assert_eq!(soft_limit(0.0), 0.0);
        assert!((soft_limit(0.4) - 0.4).abs() < 1e-6);
        for value in [-100.0, -1.0, 0.7, 1.0, 9.0, 100.0] {
            let limited = soft_limit(value);
            assert!(limited.is_finite());
            assert!(limited.abs() <= 1.0);
            assert_eq!(limited.signum(), value.signum());
        }
        assert_eq!(soft_limit(f32::NAN), 0.0);
        assert_eq!(soft_limit(f32::INFINITY), 0.0);
    }

    #[test]
    fn voice_cap_and_clipping_are_enforced() {
        let (mut mixer, control, queue) = mixer();
        for _ in 0..40 {
            queue.push(command(8.0), &control);
        }
        let mut buffer = vec![0.0f32; 512 * 2];
        mixer.render(&mut buffer, 2);
        assert!(control.active_voices() <= MAX_VOICES);
        assert!(control.active_voices() > 0);
        for sample in buffer {
            assert!(sample.is_finite());
            assert!(sample.abs() <= 1.0);
        }
    }

    #[test]
    fn disabled_control_renders_silence() {
        let (mut mixer, control, queue) = mixer();
        control.set_enabled(false);
        queue.push(command(1.0), &control);
        let mut buffer = vec![0.0f32; 480];
        mixer.render(&mut buffer, 2);
        assert!(buffer.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn mono_downmix_and_multichannel_output_are_finite() {
        let (mut mixer, control, queue) = mixer();
        queue.push(command(1.0), &control);
        let mut mono = vec![0.0f32; 256];
        mixer.render(&mut mono, 1);
        assert!(mono.iter().all(|sample| sample.is_finite()));

        queue.push(command(1.0), &control);
        let mut four = vec![0.0f32; 256 * 4];
        mixer.render(&mut four, 4);
        assert!(four.iter().all(|sample| sample.is_finite()));
        // Every channel is a duplicate or a downmix of the same stereo pair.
        for frame in four.as_chunks::<4>().0 {
            assert!(frame[0].is_finite() && frame[1].is_finite());
            assert_eq!(frame[2], frame[3]);
        }
    }

    #[test]
    fn voices_fade_in_and_are_released_after_the_clip() {
        let (mut mixer, control, queue) = mixer();
        control.set_master(1.0);
        queue.push(command(1.0), &control);
        let mut buffer = vec![0.0f32; 2_048 * 2];
        mixer.render(&mut buffer, 2);
        assert_eq!(buffer[0], 0.0, "master fades in from silence");
        assert_eq!(buffer[1], 0.0);
        assert!(
            buffer.iter().skip(2_000).any(|sample| sample.abs() > 1.0e-4),
            "the voice must become audible during the clip"
        );
        // The test clip is 4,800 frames; render past it and verify the voice
        // was released without an allocation-heavy leak.
        let mut rest = vec![0.0f32; 4_096 * 2];
        mixer.render(&mut rest, 2);
        assert_eq!(control.active_voices(), 0);
    }

    #[test]
    fn output_is_stereo_panned() {
        let (mut mixer, control, queue) = mixer();
        let mut hard_left = command(1.0);
        hard_left.pan = -1.0;
        queue.push(hard_left, &control);
        let mut buffer = vec![0.0f32; 512 * 2];
        mixer.render(&mut buffer, 2);
        let left_energy: f32 = buffer.as_chunks::<2>().0.iter().map(|frame| frame[0].abs()).sum();
        let right_energy: f32 = buffer.as_chunks::<2>().0.iter().map(|frame| frame[1].abs()).sum();
        assert!(
            left_energy > right_energy,
            "hard-left voice must favour the left channel"
        );
    }
}

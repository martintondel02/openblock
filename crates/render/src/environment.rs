//! Time-of-day environment model.
//!
//! [`Environment`] is the single value the client advances (or derives from
//! the simulation tick count) to drive the sky, the sun and moon, the stars and
//! the light applied to world geometry. It deliberately owns only the clock;
//! the visual state is a pure function of that clock and can therefore be
//! tested on the CPU without a GPU adapter.
//!
//! # Time mapping
//!
//! `time_of_day` is normalised to `0.0..1.0`:
//!
//! | value | phase |
//! |-------|-------|
//! | 0.00 | midnight |
//! | 0.25 | sunrise |
//! | 0.50 | noon |
//! | 0.75 | sunset |
//!
//! A full day is [`DAY_LENGTH_TICKS`] simulation ticks: 24,000 ticks at 20 Hz,
//! i.e. 20 real minutes. [`Environment::from_ticks`] converts a tick counter
//! into that clock so the client can keep the environment in sync with the
//! simulation without a separate wall clock.
//!
//! # Palette
//!
//! Colours are authored as sRGB key frames and converted to linear RGB, which
//! is what the shader and the sRGB render targets expect. At noon the palette
//! is exactly the v0.0.4 sky (`sRGB (0.68, 0.80, 0.93)` horizon,
//! `sRGB (0.30, 0.52, 0.86)` zenith), so the default look is unchanged. Dawn
//! and dusk blend a warm orange horizon under a dusty blue zenith, and night is
//! a deep blue with a cool, dim light so the world stays readable.
//!
//! The sun travels in a great circle through the east (`+X`) at sunrise, the
//! zenith at noon and the west (`-X`) at sunset. The moon is its antipode and
//! the star field rotates once per day around the vertical axis.

use std::f32::consts::{FRAC_PI_2, TAU};

/// Number of simulation ticks in one full day.
///
/// 24,000 ticks at the simulation's 20 Hz tick rate is 20 real minutes.
pub const DAY_LENGTH_TICKS: u64 = 24_000;

// Authored sRGB key frames.
const DAY_HORIZON_SRGB: [f32; 3] = [0.68, 0.80, 0.93];
const DAY_ZENITH_SRGB: [f32; 3] = [0.30, 0.52, 0.86];
const TWILIGHT_HORIZON_SRGB: [f32; 3] = [1.00, 0.52, 0.26];
const TWILIGHT_ZENITH_SRGB: [f32; 3] = [0.18, 0.25, 0.52];
const NIGHT_HORIZON_SRGB: [f32; 3] = [0.050, 0.075, 0.150];
const NIGHT_ZENITH_SRGB: [f32; 3] = [0.012, 0.020, 0.060];
const NOON_SUN_SRGB: [f32; 3] = [1.00, 0.97, 0.90];
const SUNSET_SUN_SRGB: [f32; 3] = [1.00, 0.45, 0.15];

// Subtle world-light tints. The daytime tint is neutral so world geometry is
// bit-identical to the previous renderer at the default environment.
const DAY_TINT: [f32; 3] = [1.0, 1.0, 1.0];
const TWILIGHT_TINT: [f32; 3] = [1.0, 0.90, 0.78];
const NIGHT_TINT: [f32; 3] = [0.70, 0.82, 1.0];

/// Fraction of daytime block light available at midnight.
const MOONLIGHT_DAYLIGHT: f32 = 0.22;
/// Fraction of daytime ambient light available at midnight.
const MOONLIGHT_AMBIENT: f32 = 0.18;

/// Converts one sRGB channel to linear RGB.
pub(crate) fn srgb_to_linear_channel(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Converts an sRGB RGB triple to linear RGB.
pub(crate) fn srgb_to_linear_rgb(color: [f32; 3]) -> [f32; 3] {
    [
        srgb_to_linear_channel(color[0]),
        srgb_to_linear_channel(color[1]),
        srgb_to_linear_channel(color[2]),
    ]
}

/// Converts an sRGB RGBA colour to linear RGB, leaving alpha untouched.
pub(crate) fn srgb_to_linear_rgba(color: [f32; 4]) -> [f32; 4] {
    [
        srgb_to_linear_channel(color[0]),
        srgb_to_linear_channel(color[1]),
        srgb_to_linear_channel(color[2]),
        color[3],
    ]
}

/// Smooth Hermite interpolation between two edges.
///
/// `edge0` may be greater than `edge1`, which reverses the ramp.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn blend(a: [f32; 3], b: [f32; 3], c: [f32; 3], weights: [f32; 3]) -> [f32; 3] {
    [
        a[0] * weights[0] + b[0] * weights[1] + c[0] * weights[2],
        a[1] * weights[0] + b[1] * weights[1] + c[1] * weights[2],
        a[2] * weights[0] + b[2] * weights[1] + c[2] * weights[2],
    ]
}

/// Relative luminance of a linear RGB triple.
fn luminance(color: [f32; 3]) -> f32 {
    0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2]
}

/// The time-of-day clock.
///
/// The default is noon (`time_of_day == 0.5`), which reproduces the v0.0.4
/// daylight palette and lighting exactly. Values outside `0.0..1.0` are wrapped
/// by [`Environment::visual`]; non-finite values fall back to noon.
///
/// ```
/// use ob_render::{Environment, DAY_LENGTH_TICKS};
///
/// // 06:00 at 20 Hz: one quarter into a 24,000-tick day.
/// let dawn = Environment::from_ticks(DAY_LENGTH_TICKS / 4, DAY_LENGTH_TICKS);
/// assert!((dawn.time_of_day - 0.25).abs() < 1e-6);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Environment {
    /// Normalised time of day: `0.0` midnight, `0.25` sunrise, `0.5` noon,
    /// `0.75` sunset.
    pub time_of_day: f32,
}

impl Default for Environment {
    fn default() -> Self {
        Self { time_of_day: 0.5 }
    }
}

impl Environment {
    /// Creates an environment at the given time of day.
    pub const fn new(time_of_day: f32) -> Self {
        Self { time_of_day }
    }

    /// Derives the environment from a simulation tick counter.
    ///
    /// `ticks` is reduced modulo `day_length_ticks`, so very large counters are
    /// harmless. A zero `day_length_ticks` returns
    /// `Environment::default()` instead of dividing by zero.
    pub fn from_ticks(ticks: u64, day_length_ticks: u64) -> Self {
        if day_length_ticks == 0 {
            return Self::default();
        }
        let phase = ticks % day_length_ticks;
        Self {
            // The divisor is bounded by `u64::MAX`, but the quotient is always
            // below 1, so the f32 conversion is exact enough for a clock.
            time_of_day: phase as f32 / day_length_ticks as f32,
        }
    }

    /// Returns the environment with `time_of_day` wrapped into `0.0..1.0`.
    ///
    /// Non-finite clocks fall back to noon so a corrupt tick value never
    /// produces NaN colours or geometry.
    pub fn normalized(self) -> Self {
        if !self.time_of_day.is_finite() {
            return Self::default();
        }
        Self {
            time_of_day: self.time_of_day.rem_euclid(1.0),
        }
    }

    /// Computes the complete visual state for this clock.
    ///
    /// This is a pure, GPU-free function. Colours are linear RGB and vectors are
    /// normalised.
    pub fn visual(self) -> EnvironmentVisual {
        let time_of_day = self.normalized().time_of_day;

        // Sun angle measured from the horizon: 0 at sunrise, pi/2 at noon,
        // pi at sunset and 3pi/2 at midnight.
        let sun_angle = time_of_day * TAU - FRAC_PI_2;
        let (sun_elevation, sun_forward) = sun_angle.sin_cos();
        let sun_direction = [sun_forward, sun_elevation, 0.0];

        // Day/twilight/night weights always sum to one: at most one of the day
        // and night ramps is active at a time and the remainder is twilight.
        let day_weight = smoothstep(0.02, 0.35, sun_elevation);
        let night_weight = smoothstep(0.02, 0.35, -sun_elevation);
        let twilight_weight = (1.0 - day_weight - night_weight).max(0.0);

        let zenith = blend(
            srgb_to_linear_rgb(DAY_ZENITH_SRGB),
            srgb_to_linear_rgb(TWILIGHT_ZENITH_SRGB),
            srgb_to_linear_rgb(NIGHT_ZENITH_SRGB),
            [day_weight, twilight_weight, night_weight],
        );
        let horizon = blend(
            srgb_to_linear_rgb(DAY_HORIZON_SRGB),
            srgb_to_linear_rgb(TWILIGHT_HORIZON_SRGB),
            srgb_to_linear_rgb(NIGHT_HORIZON_SRGB),
            [day_weight, twilight_weight, night_weight],
        );

        // Block light fades between the 22% moonlight floor and full daylight.
        let day_cycle = smoothstep(-0.10, 0.30, sun_elevation);
        let daylight = MOONLIGHT_DAYLIGHT + (1.0 - MOONLIGHT_DAYLIGHT) * day_cycle;
        let ambient_light = MOONLIGHT_AMBIENT + (1.0 - MOONLIGHT_AMBIENT) * day_cycle;

        // Stars and the moon only appear once the sun is below the horizon.
        let star_visibility = smoothstep(-0.02, -0.22, sun_elevation);
        let moon_visibility = smoothstep(-0.02, -0.15, sun_elevation);

        // A low sun is orange; a high sun is close to white. The disc is hidden
        // once it has set, and brighter while it is high in the sky.
        let sun_height = smoothstep(-0.06, 0.30, sun_elevation);
        let sun_color = mix(
            srgb_to_linear_rgb(SUNSET_SUN_SRGB),
            srgb_to_linear_rgb(NOON_SUN_SRGB),
            sun_height,
        );
        let sun_intensity = smoothstep(-0.05, 0.04, sun_elevation) * (0.9 + 0.7 * day_cycle);

        let light_tint = blend(
            DAY_TINT,
            TWILIGHT_TINT,
            NIGHT_TINT,
            [day_weight, twilight_weight, night_weight],
        );

        debug_assert!(luminance(zenith).is_finite());
        debug_assert!(luminance(horizon).is_finite());
        debug_assert!(sun_elevation.is_finite());

        EnvironmentVisual {
            time_of_day,
            sun_direction,
            sun_elevation,
            zenith,
            horizon,
            sun_color,
            sun_intensity,
            ambient_light,
            daylight,
            star_visibility,
            moon_visibility,
            light_tint,
            star_rotation: time_of_day * TAU,
        }
    }
}

/// CPU-computed sky and lighting state for one [`Environment`].
///
/// All colours are linear RGB in `0.0..=1.0` unless noted. The shader consumes
/// the same values through the camera uniform, so tests over this struct cover
/// the GPU palette without needing an adapter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvironmentVisual {
    /// Normalised time of day this state was evaluated for.
    pub time_of_day: f32,
    /// Unit vector from the world towards the sun (above the horizon by day).
    pub sun_direction: [f32; 3],
    /// Sine of the sun's elevation: `-1.0` at midnight, `1.0` at noon.
    pub sun_elevation: f32,
    /// Linear zenith colour.
    pub zenith: [f32; 3],
    /// Linear horizon colour; the fog colour follows it exactly.
    pub horizon: [f32; 3],
    /// Linear colour of the sun disc.
    pub sun_color: [f32; 3],
    /// Sun-disc intensity; zero when the sun has set.
    pub sun_intensity: f32,
    /// Ambient light multiplier: `0.18` at night, `1.0` at noon.
    pub ambient_light: f32,
    /// Multiplier for baked block-face light: `0.22` at night, `1.0` at noon.
    pub daylight: f32,
    /// Star visibility: `0.0` by day, `1.0` at night.
    pub star_visibility: f32,
    /// Moon visibility: `0.0` by day, `1.0` at night.
    pub moon_visibility: f32,
    /// Subtle linear tint multiplied into world lighting.
    pub light_tint: [f32; 3],
    /// Rotation of the star field around the vertical axis, in radians.
    pub star_rotation: f32,
}

impl EnvironmentVisual {
    /// Linear RGB fog colour. Kept as a named accessor to make the "fog follows
    /// the horizon" invariant explicit at call sites and in tests.
    pub fn fog_color(&self) -> [f32; 3] {
        self.horizon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(time_of_day: f32) -> EnvironmentVisual {
        Environment { time_of_day }.visual()
    }

    fn max_channel(color: [f32; 3]) -> f32 {
        color[0].max(color[1]).max(color[2])
    }

    #[test]
    fn default_matches_the_v004_daylight_palette() {
        let visual = Environment::default().visual();
        assert_eq!(visual.time_of_day, 0.5);
        // The former shader constants: sRGB (0.68, 0.80, 0.93) and
        // sRGB (0.30, 0.52, 0.86) converted to linear.
        let expected_horizon = [0.4196, 0.6038, 0.8481];
        let expected_zenith = [0.0732, 0.2330, 0.7106];
        for (actual, expected) in visual.horizon.iter().zip(expected_horizon) {
            assert!((actual - expected).abs() < 1e-3, "horizon {actual} vs {expected}");
        }
        for (actual, expected) in visual.zenith.iter().zip(expected_zenith) {
            assert!((actual - expected).abs() < 1e-3, "zenith {actual} vs {expected}");
        }
        // Neutral lighting so existing scenes render exactly as before.
        assert!((visual.daylight - 1.0).abs() < 1e-6);
        assert!((visual.ambient_light - 1.0).abs() < 1e-6);
        assert_eq!(visual.light_tint, [1.0, 1.0, 1.0]);
        assert_eq!(visual.star_visibility, 0.0);
        assert_eq!(visual.moon_visibility, 0.0);
        assert!(visual.sun_intensity > 1.0);
    }

    #[test]
    fn night_is_darker_than_day_and_stars_come_out() {
        let day = env(0.5);
        let night = env(0.0);
        assert!(luminance(night.zenith) < luminance(day.zenith) * 0.05);
        assert!(luminance(night.horizon) < luminance(day.horizon) * 0.05);
        assert!(max_channel(night.horizon) < max_channel(day.horizon));
        assert!(night.daylight >= 0.18 && night.daylight <= 0.25);
        assert!(night.ambient_light >= 0.15 && night.ambient_light <= 0.25);
        assert!(night.star_visibility > 0.99);
        assert!(night.moon_visibility > 0.99);
        assert_eq!(day.star_visibility, 0.0);
        assert_eq!(day.moon_visibility, 0.0);
        // The night tint is cool: blue is stronger than red.
        assert!(night.light_tint[2] > night.light_tint[0]);
        assert!((night.sun_intensity - 0.0).abs() < 1e-6);
    }

    #[test]
    fn dawn_and_dusk_horizons_are_warmer_than_noon() {
        let noon = env(0.5).horizon;
        for time in [0.25, 0.75] {
            let horizon = env(time).horizon;
            assert!(
                horizon[0] - horizon[2] > noon[0] - noon[2] + 0.35,
                "time {time}: horizon {horizon:?} is not redder than noon {noon:?}"
            );
            assert!(
                horizon[0] / horizon[1] > noon[0] / noon[1] + 0.5,
                "time {time}: horizon {horizon:?} is not warmer than noon {noon:?}"
            );
            assert!(
                luminance(horizon) > luminance(env(0.0).horizon),
                "time {time}: the twilight horizon should still be brighter than night"
            );
        }
    }

    #[test]
    fn from_ticks_maps_and_wraps_a_24000_tick_day() {
        assert_eq!(Environment::from_ticks(0, DAY_LENGTH_TICKS).time_of_day, 0.0);
        assert_eq!(Environment::from_ticks(6_000, DAY_LENGTH_TICKS).time_of_day, 0.25);
        assert_eq!(Environment::from_ticks(12_000, DAY_LENGTH_TICKS).time_of_day, 0.5);
        assert_eq!(Environment::from_ticks(18_000, DAY_LENGTH_TICKS).time_of_day, 0.75);
        // One full day wraps back to midnight, two days plus dawn too.
        assert_eq!(Environment::from_ticks(24_000, DAY_LENGTH_TICKS).time_of_day, 0.0);
        assert_eq!(Environment::from_ticks(30_000, DAY_LENGTH_TICKS).time_of_day, 0.25);
        assert_eq!(
            Environment::from_ticks(u64::MAX, DAY_LENGTH_TICKS).time_of_day,
            (u64::MAX % DAY_LENGTH_TICKS) as f32 / DAY_LENGTH_TICKS as f32
        );
        // A different day length still normalises correctly.
        assert_eq!(Environment::from_ticks(12_000, 48_000).time_of_day, 0.25);
        // A zero day length must not divide by zero.
        assert_eq!(Environment::from_ticks(123, 0), Environment::default());
    }

    #[test]
    fn out_of_range_and_non_finite_clocks_are_safe() {
        assert_eq!(Environment::new(1.0).normalized().time_of_day, 0.0);
        assert_eq!(Environment::new(-0.25).normalized().time_of_day, 0.75);
        assert_eq!(Environment::new(2.75).normalized().time_of_day, 0.75);
        assert_eq!(Environment::new(f32::NAN).normalized(), Environment::default());
        assert_eq!(Environment::new(f32::INFINITY).normalized(), Environment::default());
        // NaN must never leak into the uniform.
        let visual = Environment::new(f32::NAN).visual();
        for value in visual
            .zenith
            .iter()
            .chain(visual.horizon.iter())
            .chain(visual.sun_direction.iter())
            .chain([&visual.sun_intensity, &visual.daylight, &visual.star_visibility])
        {
            assert!(value.is_finite(), "non-finite visual value: {value}");
        }
    }

    #[test]
    fn the_cycle_is_smooth_and_continuous() {
        let steps = 2_000;
        let mut previous = env(0.0);
        for step in 1..=steps {
            let visual = env(step as f32 / steps as f32);
            assert!(visual.daylight >= MOONLIGHT_DAYLIGHT && visual.daylight <= 1.0);
            assert!(visual.ambient_light >= MOONLIGHT_AMBIENT && visual.ambient_light <= 1.0);
            assert!((0.0..=1.0).contains(&visual.star_visibility));
            assert!((0.0..=1.0).contains(&visual.moon_visibility));
            assert!((0.0..=1.0).contains(&visual.time_of_day));
            let length = visual.sun_direction.iter().map(|c| c * c).sum::<f32>().sqrt();
            assert!((length - 1.0).abs() < 1e-5, "sun direction is not unit: {length}");
            for (previous, current) in [
                (previous.zenith, visual.zenith),
                (previous.horizon, visual.horizon),
                (previous.light_tint, visual.light_tint),
            ] {
                for channel in 0..3 {
                    assert!(
                        (current[channel] - previous[channel]).abs() < 0.05,
                        "abrupt palette change at step {step}: {previous:?} -> {current:?}"
                    );
                }
            }
            assert!((visual.daylight - previous.daylight).abs() < 0.05);
            previous = visual;
        }
        // Noon is the brightest point and midnight the darkest.
        let noon = env(0.5);
        let midnight = env(0.0);
        assert!(luminance(noon.zenith) > luminance(midnight.zenith));
        for step in 0..100 {
            let visual = env(step as f32 / 100.0);
            assert!(
                luminance(visual.zenith) <= luminance(noon.zenith) + 1e-4,
                "zenith brighter than noon at {step}"
            );
        }
    }

    #[test]
    fn fog_colour_is_the_horizon_colour() {
        for time in [0.0, 0.25, 0.5, 0.75] {
            let visual = env(time);
            assert_eq!(visual.fog_color(), visual.horizon);
        }
    }
}

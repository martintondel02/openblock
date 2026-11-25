//! Graphics quality settings and their mapping to renderer resources.
//!
//! [`GraphicsSettings`] is the GPU-free description of the 1.1 visual
//! feature set: sun shadows, procedural clouds, transparent water, HDR
//! bloom and FXAA. A [`GraphicsPreset`] selects a coherent set of defaults;
//! individual qualities can then be adjusted without touching the renderer's
//! shader modules (every pipeline is compiled once at start-up and the
//! features are switched by uniforms and target allocation).
//!
//! The windowed client owns the persisted values in `ob_core::config::Settings`
//! and maps them through [`GraphicsSettings`] on start-up and whenever a
//! settings row changes. The offscreen renderer and the benchmarks use the
//! same type, so a saved preset renders identically in the window, in
//! screenshots and in the headless harness.
//!
//! FSR is deliberately an **independent** quality row: presets never enable it
//! and changing it does not mark a preset custom, because it only trades
//! internal resolution for frame time and does not alter the scene's visual
//! feature set.

use crate::fsr::FsrQuality;

/// Built-in visual preset.
///
/// [`GraphicsPreset::Custom`] is stored when an individual quality no longer
/// matches a preset; it behaves like [`GraphicsPreset::High`] for fields that
/// are not explicitly set.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsPreset {
    /// No shadows or clouds, cheap water, no bloom or FXAA.
    Low,
    /// Low shadows and clouds, cheap water, bloom on, FXAA on.
    Medium,
    /// The default: two 2048² cascades, full clouds, transparent water,
    /// bloom and FXAA.
    #[default]
    High,
    /// High plus 5×5 PCF shadow filtering.
    Ultra,
    /// Individual overrides that do not match a preset.
    Custom,
}

impl GraphicsPreset {
    /// Selectable presets in menu order, excluding [`Self::Custom`].
    pub const ALL: [GraphicsPreset; 4] = [
        GraphicsPreset::Low,
        GraphicsPreset::Medium,
        GraphicsPreset::High,
        GraphicsPreset::Ultra,
    ];

    /// Stable lowercase settings/CLI name.
    pub fn name(self) -> &'static str {
        match self {
            GraphicsPreset::Low => "low",
            GraphicsPreset::Medium => "medium",
            GraphicsPreset::High => "high",
            GraphicsPreset::Ultra => "ultra",
            GraphicsPreset::Custom => "custom",
        }
    }

    /// Display label used by settings rows and F3.
    pub fn label(self) -> &'static str {
        match self {
            GraphicsPreset::Low => "Low",
            GraphicsPreset::Medium => "Medium",
            GraphicsPreset::High => "High",
            GraphicsPreset::Ultra => "Ultra",
            GraphicsPreset::Custom => "Custom",
        }
    }

    /// Parses a preset name (CLI and tests).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain([Self::Custom])
            .find(|preset| preset.name().eq_ignore_ascii_case(name))
    }

    /// Next preset in the menu cycle (Low → Medium → High → Ultra → Low).
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|preset| *preset == self).unwrap_or(2);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous preset in the menu cycle.
    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|preset| *preset == self).unwrap_or(2);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Resolution, cascade count and PCF width of a [`ShadowQuality`].
///
/// `Off` carries a 1×1 map and no cascades so the shader can early out while
/// group 0 still has a valid depth texture to bind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowConfig {
    /// Selected quality.
    pub quality: ShadowQuality,
    /// Shadow-map edge length in texels per cascade.
    pub resolution: u32,
    /// Number of cascades (0 when shadows are off).
    pub cascades: u32,
    /// Half-width of the PCF kernel: 1 = 3×3, 2 = 5×5.
    pub pcf_radius: u32,
    /// Far distance of each cascade in metres. Unused entries are zero.
    pub splits: [f32; 3],
}

impl ShadowConfig {
    /// Far distance of the last active cascade, or zero when shadows are off.
    pub fn max_distance(&self) -> f32 {
        if self.cascades == 0 {
            0.0
        } else {
            self.splits[self.cascades as usize - 1]
        }
    }
}

/// Sun-shadow quality.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ShadowQuality {
    /// No shadow map; the world is lit exactly as in 1.0.
    Off,
    /// Two 1024² cascades with 3×3 PCF.
    Low,
    /// Three 2048² cascades with 3×3 PCF.
    #[default]
    High,
    /// Three 2048² cascades with 5×5 PCF.
    Ultra,
}

impl ShadowQuality {
    /// All selectable qualities in menu order.
    pub const ALL: [ShadowQuality; 4] = [
        ShadowQuality::Off,
        ShadowQuality::Low,
        ShadowQuality::High,
        ShadowQuality::Ultra,
    ];

    /// Stable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            ShadowQuality::Off => "off",
            ShadowQuality::Low => "low",
            ShadowQuality::High => "high",
            ShadowQuality::Ultra => "ultra",
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            ShadowQuality::Off => "Off",
            ShadowQuality::Low => "Low",
            ShadowQuality::High => "High",
            ShadowQuality::Ultra => "Ultra",
        }
    }

    /// Parses a quality name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|quality| quality.name().eq_ignore_ascii_case(name))
    }

    /// Renderer configuration for this quality.
    pub fn config(self) -> ShadowConfig {
        match self {
            ShadowQuality::Off => ShadowConfig {
                quality: self,
                resolution: 1,
                cascades: 0,
                pcf_radius: 0,
                splits: [0.0; 3],
            },
            ShadowQuality::Low => ShadowConfig {
                quality: self,
                resolution: 1024,
                cascades: 2,
                pcf_radius: 1,
                splits: [48.0, 160.0, 0.0],
            },
            ShadowQuality::High => ShadowConfig {
                quality: self,
                resolution: 2048,
                cascades: 2,
                pcf_radius: 1,
                splits: [32.0, 160.0, 0.0],
            },
            ShadowQuality::Ultra => ShadowConfig {
                quality: self,
                resolution: 2048,
                cascades: 2,
                pcf_radius: 2,
                splits: [32.0, 160.0, 0.0],
            },
        }
    }
}

/// Procedural cloud quality.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CloudQuality {
    /// No cloud layer.
    Off,
    /// Two-octave cloud noise, cheaper for integrated GPUs.
    Low,
    /// Four-octave noise with sun-lit edges and horizon fade.
    #[default]
    High,
}

impl CloudQuality {
    /// All choices in menu order.
    pub const ALL: [CloudQuality; 3] = [CloudQuality::Off, CloudQuality::Low, CloudQuality::High];

    /// Stable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            CloudQuality::Off => "off",
            CloudQuality::Low => "low",
            CloudQuality::High => "high",
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            CloudQuality::Off => "Off",
            CloudQuality::Low => "Low",
            CloudQuality::High => "High",
        }
    }

    /// Parses a quality name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|quality| quality.name().eq_ignore_ascii_case(name))
    }

    /// Shader quality factor in `0.0..=1.0`.
    pub fn factor(self) -> f32 {
        match self {
            CloudQuality::Off => 0.0,
            CloudQuality::Low => 0.45,
            CloudQuality::High => 1.0,
        }
    }
}

/// Transparent-water quality.
///
/// `Off` keeps the water pass (so the surface never disappears) but draws it
/// fully opaque with no uv animation or specular sun highlight.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WaterQuality {
    /// Opaque, static water.
    Off,
    /// Translucent water with gentle waves.
    Low,
    /// Translucent water, stronger animated normals and sun glints.
    #[default]
    High,
}

impl WaterQuality {
    /// All choices in menu order.
    pub const ALL: [WaterQuality; 3] = [WaterQuality::Off, WaterQuality::Low, WaterQuality::High];

    /// Stable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            WaterQuality::Off => "off",
            WaterQuality::Low => "low",
            WaterQuality::High => "high",
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            WaterQuality::Off => "Off",
            WaterQuality::Low => "Low",
            WaterQuality::High => "High",
        }
    }

    /// Parses a quality name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|quality| quality.name().eq_ignore_ascii_case(name))
    }

    /// Shader parameters: `(wave strength, surface alpha, sun specular)`.
    pub fn shader_params(self) -> (f32, f32, f32) {
        match self {
            WaterQuality::Off => (0.0, 1.0, 0.0),
            WaterQuality::Low => (0.45, 0.80, 0.55),
            WaterQuality::High => (1.0, 0.72, 1.0),
        }
    }
}

/// Complete visual quality configuration for one renderer.
///
/// The default is [`GraphicsPreset::High`], which is the documented 1.1
/// default. Constructing from a preset keeps every field coherent; individual
/// fields can be overridden afterwards (the caller then sets
/// `preset = GraphicsPreset::Custom` when it wants the UI to report a custom
/// configuration).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphicsSettings {
    /// Preset this configuration was derived from, or `Custom`.
    pub preset: GraphicsPreset,
    /// Shadow quality.
    pub shadow_quality: ShadowQuality,
    /// Cloud quality.
    pub clouds: CloudQuality,
    /// Water quality.
    pub water: WaterQuality,
    /// Whether HDR bloom is enabled.
    pub bloom: bool,
    /// Whether the FXAA-lite edge filter runs in the post pass.
    pub fxaa: bool,
    /// Cloud coverage in `0.0..=1.0` (0 = clear, 1 = overcast).
    pub cloud_coverage: f32,
    /// Exposure multiplier applied before tonemapping. The default 0.8 keeps
    /// the pre-1.1 sky and terrain midtones close under the ACES fit.
    pub exposure: f32,
    /// FidelityFX Super Resolution 1.0 quality, or [`FsrQuality::Off`] for the
    /// native path. Presets always leave this off; it is an independent row.
    pub fsr_quality: FsrQuality,
}

impl Default for GraphicsSettings {
    fn default() -> Self {
        Self::from_preset(GraphicsPreset::High)
    }
}

impl GraphicsSettings {
    /// Builds the coherent defaults for a preset.
    pub fn from_preset(preset: GraphicsPreset) -> Self {
        let (shadow_quality, clouds, water, bloom, fxaa) = match preset {
            GraphicsPreset::Low => (ShadowQuality::Off, CloudQuality::Off, WaterQuality::Low, false, false),
            GraphicsPreset::Medium => (ShadowQuality::Low, CloudQuality::Low, WaterQuality::Low, true, true),
            GraphicsPreset::High | GraphicsPreset::Custom => {
                (ShadowQuality::High, CloudQuality::High, WaterQuality::High, true, true)
            }
            GraphicsPreset::Ultra => (ShadowQuality::Ultra, CloudQuality::High, WaterQuality::High, true, true),
        };
        Self {
            preset,
            shadow_quality,
            clouds,
            water,
            bloom,
            fxaa,
            cloud_coverage: 0.45,
            exposure: DEFAULT_EXPOSURE,
            fsr_quality: FsrQuality::Off,
        }
    }

    /// Clamps free-form values into their supported ranges.
    pub fn normalised(mut self) -> Self {
        if !self.cloud_coverage.is_finite() {
            self.cloud_coverage = Self::default().cloud_coverage;
        }
        self.cloud_coverage = self.cloud_coverage.clamp(0.0, 1.0);
        if !self.exposure.is_finite() {
            self.exposure = DEFAULT_EXPOSURE;
        }
        self.exposure = self.exposure.clamp(MIN_EXPOSURE, MAX_EXPOSURE);
        self
    }

    /// Shadow-map configuration for the selected quality.
    pub fn shadow_config(&self) -> ShadowConfig {
        self.shadow_quality.config()
    }

    /// Shader cloud factor in `0.0..=1.0` (zero disables the cloud layer).
    pub fn cloud_factor(&self) -> f32 {
        self.clouds.factor()
    }

    /// Blender strength passed to the post shader.
    pub fn bloom_strength(&self) -> f32 {
        if self.bloom { BLOOM_STRENGTH } else { 0.0 }
    }

    /// Whether the depth-reconstructed SSAO pass runs.
    ///
    /// The 1.4 slice spends the extra fill-rate only on the two high tiers:
    /// Low and Medium (`ShadowQuality::Off`/`Low`) keep their 1.1–1.3 cheap
    /// path. `Custom` stays coherent because it is keyed off the shadow row,
    /// not off the preset name: a player who turned shadows down to Low also
    /// gets the cheaper ambient term instead of a full-price AO pass.
    pub fn ssao_enabled(&self) -> bool {
        matches!(self.shadow_quality, ShadowQuality::High | ShadowQuality::Ultra)
    }

    /// Strength of the SSAO multiplier in the post pass, zero when disabled.
    pub fn ssao_strength(&self) -> f32 {
        if self.ssao_enabled() { SSAO_STRENGTH } else { 0.0 }
    }

    /// Whether the terrain/water shaders project the cloud layer onto the
    /// ground as moving cloud shadows.
    ///
    /// This is an **Ultra-only** effect after measurement: on the reference
    /// RADV integrated GPU it costs ~1.1 ms at 1920×1080 (a full-screen
    /// cloud-density fetch plus the projection), which would take High below
    /// the 60 FPS target. High keeps the same cloud layer in the sky and the
    /// height fog, so only the ground modulation is deferred to Ultra.
    pub fn cloud_shadows_enabled(&self) -> bool {
        self.shadow_quality == ShadowQuality::Ultra && self.clouds == CloudQuality::High
    }

    /// Resolution divisor of the SSAO target: Ultra uses half resolution,
    /// High a quarter. Ambient occlusion is low-frequency, and the measured
    /// 0.3 ms saved by the quarter target is what keeps High at 60 FPS.
    pub fn ssao_resolution_divisor(&self) -> u32 {
        match self.shadow_quality {
            ShadowQuality::Ultra => 2,
            _ => 4,
        }
    }

    /// Strength of the cloud-shadow modulation on terrain, water and far
    /// terrain (zero on Low/Medium and when clouds are off).
    pub fn cloud_shadow_strength(&self) -> f32 {
        if self.cloud_shadows_enabled() {
            CLOUD_SHADOW_STRENGTH
        } else {
            0.0
        }
    }

    /// Whether the cheap height-fog / aerial-perspective term is applied.
    ///
    /// This is a few ALU operations in the terrain/water/far shaders, so both
    /// High and Ultra keep it; Low and Medium stay at their 1.3 look.
    pub fn height_fog_enabled(&self) -> bool {
        self.ssao_enabled()
    }

    /// Multiplier for the sun glints on water (High keeps the full specular
    /// response, Low is dimmer, Off has none).
    pub fn water_glints(&self) -> f32 {
        self.water.shader_params().2
    }

    /// Short one-line description for F3 and the benchmark report.
    pub fn summary(&self) -> String {
        let shadows = match self.shadow_quality {
            ShadowQuality::Off => "shadows off".to_string(),
            quality => format!(
                "shadows {} {}² ×{} (PCF near)",
                quality.name(),
                self.shadow_config().resolution,
                self.shadow_config().cascades,
            ),
        };
        format!(
            "{} preset | {} | clouds {} | water {} | bloom {} | FXAA {} | FSR {}",
            self.preset.label(),
            shadows,
            self.clouds.name(),
            self.water.name(),
            if self.bloom { "on" } else { "off" },
            if self.fxaa { "on" } else { "off" },
            self.fsr_quality.key(),
        )
    }
}

/// Default exposure multiplier before the ACES fit.
pub const DEFAULT_EXPOSURE: f32 = 0.80;
/// Lowest accepted exposure multiplier.
pub const MIN_EXPOSURE: f32 = 0.30;
/// Highest accepted exposure multiplier.
pub const MAX_EXPOSURE: f32 = 2.0;
/// Bloom contribution added by the post pass at full white.
pub const BLOOM_STRENGTH: f32 = 0.07;
/// Luminance above which the bloom threshold pass emits light.
pub const BLOOM_THRESHOLD: f32 = 1.00;
/// Strength of the half-resolution SSAO multiplier on High/Ultra.
pub const SSAO_STRENGTH: f32 = 0.85;
/// Maximum darkening applied to ambient light by a cloud shadow.
pub const CLOUD_SHADOW_STRENGTH: f32 = 0.30;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_coherent_and_ordered_by_cost() {
        let low = GraphicsSettings::from_preset(GraphicsPreset::Low);
        let medium = GraphicsSettings::from_preset(GraphicsPreset::Medium);
        let high = GraphicsSettings::from_preset(GraphicsPreset::High);
        let ultra = GraphicsSettings::from_preset(GraphicsPreset::Ultra);

        assert_eq!(low.shadow_quality, ShadowQuality::Off);
        assert_eq!(low.clouds, CloudQuality::Off);
        assert!(!low.bloom && !low.fxaa);
        assert_eq!(medium.shadow_quality, ShadowQuality::Low);
        assert!(medium.bloom && medium.fxaa);
        assert_eq!(high.shadow_quality, ShadowQuality::High);
        assert_eq!(high.clouds, CloudQuality::High);
        assert_eq!(high.water, WaterQuality::High);
        assert_eq!(ultra.shadow_quality, ShadowQuality::Ultra);
        assert!(ultra.shadow_config().pcf_radius > high.shadow_config().pcf_radius);
        assert_eq!(GraphicsSettings::default(), high);
        // FSR is an independent row: every preset starts native.
        for settings in [low, medium, high, ultra] {
            assert_eq!(settings.fsr_quality, FsrQuality::Off);
        }
    }

    #[test]
    fn shadow_configs_match_the_documented_resolutions_and_cascades() {
        let off = ShadowQuality::Off.config();
        assert_eq!((off.resolution, off.cascades), (1, 0));
        assert_eq!(off.max_distance(), 0.0);

        let low = ShadowQuality::Low.config();
        assert_eq!((low.resolution, low.cascades, low.pcf_radius), (1024, 2, 1));
        assert_eq!(low.splits[..2], [48.0, 160.0]);
        assert_eq!(low.max_distance(), 160.0);

        let high = ShadowQuality::High.config();
        assert_eq!((high.resolution, high.cascades, high.pcf_radius), (2048, 2, 1));
        assert_eq!(high.splits[..2], [32.0, 160.0]);

        let ultra = ShadowQuality::Ultra.config();
        assert_eq!((ultra.resolution, ultra.cascades, ultra.pcf_radius), (2048, 2, 2));
    }

    #[test]
    fn high_tier_effects_are_gated_by_the_existing_quality_fields() {
        let low = GraphicsSettings::from_preset(GraphicsPreset::Low);
        let medium = GraphicsSettings::from_preset(GraphicsPreset::Medium);
        let high = GraphicsSettings::from_preset(GraphicsPreset::High);
        let ultra = GraphicsSettings::from_preset(GraphicsPreset::Ultra);
        for cheap in [low, medium] {
            assert!(!cheap.ssao_enabled(), "{} must not run SSAO", cheap.preset.name());
            assert_eq!(cheap.ssao_strength(), 0.0);
            assert_eq!(cheap.cloud_shadow_strength(), 0.0);
            assert!(!cheap.height_fog_enabled());
        }
        for expensive in [high, ultra] {
            assert!(expensive.ssao_enabled(), "{} should run SSAO", expensive.preset.name());
            assert!(expensive.ssao_strength() > 0.5);
            assert!(expensive.height_fog_enabled());
        }
        // Cloud shadows are Ultra-only after measurement; High keeps the sky
        // clouds and height fog.
        assert!(!high.cloud_shadows_enabled());
        assert_eq!(high.cloud_shadow_strength(), 0.0);
        assert!(ultra.cloud_shadows_enabled());
        assert!(ultra.cloud_shadow_strength() > 0.0);
        assert_eq!(high.ssao_resolution_divisor(), 4);
        assert_eq!(ultra.ssao_resolution_divisor(), 2);
        // A disabled cloud layer removes the Ultra effect too.
        let no_clouds = GraphicsSettings {
            clouds: CloudQuality::Off,
            ..ultra
        };
        assert!(!no_clouds.cloud_shadows_enabled());
        assert_eq!(no_clouds.cloud_shadow_strength(), 0.0);
        // A custom shadow row drives the gate, not the preset name.
        let custom_cheap = GraphicsSettings {
            preset: GraphicsPreset::Custom,
            shadow_quality: ShadowQuality::Low,
            ..high
        };
        assert!(!custom_cheap.ssao_enabled());
    }

    #[test]
    fn free_form_settings_clamp_and_recover_from_non_finite_values() {
        let settings = GraphicsSettings {
            cloud_coverage: f32::NAN,
            exposure: f32::INFINITY,
            ..GraphicsSettings::default()
        }
        .normalised();
        assert_eq!(settings.cloud_coverage, 0.45);
        assert_eq!(settings.exposure, DEFAULT_EXPOSURE);

        let settings = GraphicsSettings {
            cloud_coverage: -4.0,
            exposure: 20.0,
            ..GraphicsSettings::default()
        }
        .normalised();
        assert_eq!(settings.cloud_coverage, 0.0);
        assert_eq!(settings.exposure, MAX_EXPOSURE);
    }

    #[test]
    fn names_round_trip_and_cycles_stay_in_menu_order() {
        for preset in GraphicsPreset::ALL {
            assert_eq!(GraphicsPreset::from_name(preset.name()), Some(preset));
        }
        for quality in ShadowQuality::ALL {
            assert_eq!(ShadowQuality::from_name(quality.name()), Some(quality));
        }
        for quality in CloudQuality::ALL {
            assert_eq!(CloudQuality::from_name(quality.name()), Some(quality));
        }
        for quality in WaterQuality::ALL {
            assert_eq!(WaterQuality::from_name(quality.name()), Some(quality));
        }
        assert_eq!(GraphicsPreset::Low.next(), GraphicsPreset::Medium);
        assert_eq!(GraphicsPreset::Ultra.next(), GraphicsPreset::Low);
        assert_eq!(GraphicsPreset::Low.previous(), GraphicsPreset::Ultra);
        assert_eq!(GraphicsPreset::Custom.name(), "custom");
    }

    #[test]
    fn summary_mentions_every_feature_and_shadow_details() {
        let summary = GraphicsSettings::default().summary();
        for needle in [
            "High preset",
            "shadows high",
            "2048²",
            "clouds high",
            "water high",
            "bloom on",
            "FXAA on",
            "FSR off",
        ] {
            assert!(summary.contains(needle), "missing {needle}: {summary}");
        }
        let summary = GraphicsSettings::from_preset(GraphicsPreset::Low).summary();
        assert!(summary.contains("shadows off"));
        assert!(summary.contains("bloom off"));
    }
}

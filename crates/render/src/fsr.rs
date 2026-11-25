//! Self-contained AMD FidelityFX Super Resolution 1.0 (FSR 1) spatial
//! upscaler: EASU followed by RCAS, implemented in WGSL.
//!
//! This module is deliberately independent of the private scene render graph
//! in `frame.rs`; `SceneRenderer` owns one [`FsrUpscaler`] per output format and
//! creates it when the FSR row is first enabled. The module owns both pipelines,
//! a clamp/linear sampler, two small uniform buffers and the output-resolution
//! EASU intermediate texture. No shader is compiled or pipeline created after
//! [`FsrUpscaler::new`]; a size change only reallocates the intermediate.
//!
//! # Integration contract
//!
//! The input is the **tonemapped, LDR scene at render resolution** (normally an
//! `Rgba8UnormSrgb` post target). The output is the **final sRGB surface or
//! offscreen colour texture at display resolution**. Both views must use their
//! texture's base format (no sRGB/linear view reinterpretation). The UI is
//! drawn *after* FSR, at native resolution, and must never pass through it.
//!
//! # Passes and bindings
//!
//! [`FsrUpscaler::upscale`] records two full-screen-triangle passes:
//!
//! 1. **EASU** (group 0: binding 0 EASU uniform, binding 1 input texture,
//!    binding 2 linear/clamp sampler) writes an internally owned
//!    `Rgba8Unorm` intermediate at the output size. The texture is reallocated
//!    only when the output size changes; sampling is clamp-to-edge, which is
//!    what makes the 4×4 gather edge-safe.
//! 2. **RCAS** (group 0: binding 0 RCAS uniform, binding 1 intermediate
//!    texture) writes the final output. RCAS uses `textureLoad`, so it needs no
//!    sampler; this also matches the reference's texel-exact 3×3 neighborhood.
//!
//! # Wiring in `SceneRenderer` (implemented in v1.3)
//!
//! 1. The quality row chooses a render size with [`FsrQuality::render_size`].
//!    `SceneRenderer` keeps the display size and the render size separate:
//!    its HDR/depth/bloom targets and `resize` use the render size, while the
//!    camera aspect, shadow fit, surface/offscreen texture, FSR output and UI
//!    use the display size. FSR maps the full render target to the full output,
//!    so the camera aspect stays the output aspect or the image would stretch.
//!    For [`FsrQuality::Off`] the existing native path is unchanged; the
//!    upscaler is never created or called.
//! 2. One internal LDR colour target is allocated at render size with the
//!    display target's format (so the existing post pipelines target it
//!    unchanged) and `RENDER_ATTACHMENT | TEXTURE_BINDING | COPY_SRC`. It is
//!    reallocated from the scene resize and FSR-quality paths.
//! 3. In `crates/render/src/frame.rs` the final post pass targets that LDR view
//!    instead of the output view; bloom stays at render resolution. The post
//!    pipelines were compiled for the display format at `SceneRenderer::new`,
//!    and the LDR target shares it, so they are reused unchanged. The post
//!    uniform's texel sizes are the render sizes.
//! 4. After the post pass and before the caller draws the UI,
//!    `upscaler.upscale(&mut encoder, &ldr_view, output_view, sharpness)` is
//!    recorded. The windowed `Renderer` and `OffscreenRenderer` both call
//!    `scene.record`, then draw the UI at native display resolution.
//! 5. The upscaler is created once per device/output format when the row is
//!    first enabled, kept across enabled quality changes, and dropped when the
//!    row goes Off. It is never rebuilt per frame.
//!
//! Sizes are validated by [`validate_sizes`]: both dimensions must be
//! non-zero, the output must be at least as large as the input, and the linear
//! upscale factor must be at most ~2x (FSR 1 supports 1x–4x area scaling).
//!
//! # Colour space
//!
//! FSR 1 is designed for a perceptual (sRGB/gamma-encoded) image. If the input
//! view is an sRGB format the hardware returns linear values when gathering;
//! the EASU shader re-encodes them with the exact sRGB transfer function before
//! filtering, and the RCAS shader decodes the sharpened value back to linear
//! before storing to an sRGB destination. A non-sRGB `*Unorm` input is assumed
//! to already contain perceptual values (the existing post pipeline writes
//! linear values, so use an sRGB-format internal LDR target, as recommended
//! above). A non-sRGB output receives perceptual values.
//!
//! # Cost and benchmark note
//!
//! EASU gathers a 4×4 texel footprint (four gather positions, one gather per
//! channel each) and evaluates the 12-tap edge-adaptive kernel; RCAS then loads
//! a 3×3 neighborhood from the output-resolution intermediate. Estimated
//! *combined* cost at 1920×1080 is
//! ~0.15–0.35 ms on a mid-range discrete GPU, roughly the 0.28–0.35 ms
//! `taa`+`upscale` line in `docs/spec/25-performance-budgets.md` (that budget
//! was written for TAA + upscale, not for FSR). On the integrated RADV
//! reference machine the measured pass alone costs **3.5 ms at 1280×720 output
//! and 7.8 ms at 1920×1080 with an sRGB input** (4.6 ms at 1080p with a
//! non-sRGB input), which is why FSR stays off by default; see
//! `docs/plan/1.3-fsr.md`. Measure both passes with `wgpu` timestamp queries
//! before changing that default. The intermediate costs
//! `output_width * output_height * 4` bytes (≈8.3 MB at 1080p). Both pipelines
//! are created in [`FsrUpscaler::new`]; there is no per-frame compilation.
//!
//! # Licensing
//!
//! EASU, RCAS and their constant setup are a WGSL port of AMD FidelityFX
//! Super Resolution 1.0 (`ffx-fsr/ffx_fsr1.h`, v1.20210629), Copyright (c) 2021
//! Advanced Micro Devices, Inc., distributed under the MIT license. The same
//! notice is kept in `crates/render/assets/fsr/LICENSE-FSR.txt` and the WGSL
//! files carry SPDX headers. No GPL code is used.
//!
//! ```text
//! Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in
//! all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
//! THE SOFTWARE.
//! ```

use std::fmt;

use bytemuck::{Pod, Zeroable};

/// EASU intermediate and RCAS source format.
///
/// It is deliberately **not** sRGB: the EASU pass stores perceptual values and
/// RCAS reads them back without a hardware transfer function in between. The
/// final sRGB conversion happens once, in the RCAS shader, for an sRGB output.
pub const INTERMEDIATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Size of the EASU uniform block (four `vec4` constants plus the flag row).
pub const EASU_UNIFORM_SIZE: wgpu::BufferAddress = 80;
/// Size of the RCAS uniform block (one `vec4`).
pub const RCAS_UNIFORM_SIZE: wgpu::BufferAddress = 16;
/// Alignment the two uniform structs are declared with in WGSL and Rust.
pub const FSR_UNIFORM_ALIGN: usize = 16;

/// Smallest render-size dimension returned by [`FsrQuality::render_size`].
///
/// Tiny windows and test targets are clamped to this before the even-dimension
/// adjustment, but never above the requested output size.
pub const MIN_RENDER_DIMENSION: u32 = 16;

/// Default RCAS sharpness in the FSR 1 "stops" convention.
///
/// `0.0` is maximum sharpening and larger values are milder; `exp2(-0.2)` is
/// the reference sample's default. Values are clamped to `0.0..=2.0` by
/// [`FsrUpscaler::upscale`].
pub const DEFAULT_RCAS_SHARPNESS: f32 = 0.2;

/// Maximum accepted sharpness (stops) before clamping.
pub const MAX_RCAS_SHARPNESS: f32 = 2.0;

/// Maximum linear upscale factor accepted by [`validate_sizes`].
///
/// FSR 1 is documented for 1x to 4x *area* scaling, i.e. 2x per axis. The small
/// tolerance absorbs integer rounding and the even-dimension adjustment in
/// [`FsrQuality::render_size`].
pub const MAX_LINEAR_SCALE: f32 = 2.02;

/// FSR 1 render-scale presets.
///
/// The enum is serde-free on purpose: the client's settings layer can map its
/// serialized string to [`FsrQuality::parse`] and back with [`FsrQuality::key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FsrQuality {
    /// Native resolution, no FSR pass. Callers skip the upscaler entirely.
    #[default]
    Off,
    /// 0.77 linear scale (about 1.69x area).
    UltraQuality,
    /// 0.67 linear scale (about 2.23x area).
    Quality,
    /// 0.59 linear scale (about 2.87x area).
    Balanced,
    /// 0.50 linear scale (4x area), the FSR 1 upper limit.
    Performance,
}

impl FsrQuality {
    /// All presets in decreasing quality order, starting with [`Self::Off`].
    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::UltraQuality,
        Self::Quality,
        Self::Balanced,
        Self::Performance,
    ];

    /// Linear resolution scale relative to the display resolution.
    ///
    /// [`Self::Off`] returns `1.0`.
    pub const fn scale_factor(self) -> f32 {
        match self {
            Self::Off => 1.0,
            Self::UltraQuality => 0.77,
            Self::Quality => 0.67,
            Self::Balanced => 0.59,
            Self::Performance => 0.50,
        }
    }

    /// Human-readable name for settings UI and logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::UltraQuality => "Ultra Quality",
            Self::Quality => "Quality",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    /// Stable lowercase key for persistence and CLI parsing.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::UltraQuality => "ultra_quality",
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    /// Whether the upscaler should run (`false` for [`Self::Off`]).
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Parses a setting value.
    ///
    /// Matching is case-insensitive and ignores spaces, hyphens and
    /// underscores, so `"Ultra Quality"`, `"ultra-quality"` and
    /// `"ultra_quality"` all map to [`Self::UltraQuality`]. The scale factors
    /// (`"1.0"`, `"0.77"`, `"0.67"`, `"0.59"`, `"0.50"`) and the labels accepted
    /// by the settings UI are also understood.
    pub fn parse(value: &str) -> Option<Self> {
        let normalised: String = value
            .chars()
            .filter(|character| !character.is_ascii_whitespace() && *character != '-' && *character != '_')
            .flat_map(char::to_lowercase)
            .collect();
        match normalised.as_str() {
            "off" | "none" | "disabled" | "1.0" | "1" => Some(Self::Off),
            "ultraquality" | "ultra" | "0.77" => Some(Self::UltraQuality),
            "quality" | "0.67" => Some(Self::Quality),
            "balanced" | "0.59" => Some(Self::Balanced),
            "performance" | "perf" | "0.50" | "0.5" => Some(Self::Performance),
            _ => None,
        }
    }

    /// Alias for [`Self::parse`], named for the settings integration.
    pub fn from_setting(value: &str) -> Option<Self> {
        Self::parse(value)
    }

    /// Internal render resolution for a display resolution.
    ///
    /// The result is rounded to the nearest pixel, clamped to at least
    /// [`MIN_RENDER_DIMENSION`] (but never above the requested output size),
    /// and snapped up to an even dimension when that stays within the output.
    /// Aspect ratio is preserved as closely as integer rounding allows.
    /// [`Self::Off`] returns the output size unchanged (after a minimum of one
    /// pixel).
    ///
    /// ```
    /// use ob_render::fsr::FsrQuality;
    ///
    /// assert_eq!(FsrQuality::Performance.render_size(1920, 1080), (960, 540));
    /// assert_eq!(FsrQuality::Quality.render_size(1920, 1080), (1286, 724));
    /// assert_eq!(FsrQuality::Off.render_size(1921, 1081), (1921, 1081));
    /// ```
    pub fn render_size(self, output_width: u32, output_height: u32) -> (u32, u32) {
        let output_width = output_width.max(1);
        let output_height = output_height.max(1);
        if !self.is_enabled() {
            return (output_width, output_height);
        }
        let scale = self.scale_factor();
        (
            scaled_dimension(output_width, scale),
            scaled_dimension(output_height, scale),
        )
    }
}

/// Rounds one output dimension to an even FSR render dimension.
///
/// Odd candidates snap *up* rather than down: for very small outputs a
/// downward snap could push the linear upscale past FSR 1's 2x-per-axis range,
/// while snapping up only makes the render target marginally larger.
fn scaled_dimension(output: u32, scale: f32) -> u32 {
    let candidate = (output as f32 * scale).round() as u32;
    // max-then-min avoids `Ord::clamp` panicking when `output < MIN`.
    let clamped = candidate.max(MIN_RENDER_DIMENSION).min(output);
    if clamped % 2 == 1 && clamped < output {
        clamped + 1
    } else {
        clamped
    }
}

/// Errors returned while creating or recording the FSR upscaler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsrError {
    /// The EASU input view uses a format the module does not sample.
    UnsupportedInputFormat(wgpu::TextureFormat),
    /// The destination format is not an 8-bit unorm format.
    UnsupportedOutputFormat(wgpu::TextureFormat),
    /// The destination view does not use the format passed to [`FsrUpscaler::new`].
    OutputFormatMismatch {
        /// Format the upscaler's RCAS pipeline was compiled for.
        expected: wgpu::TextureFormat,
        /// Format of the provided destination view.
        actual: wgpu::TextureFormat,
    },
    /// One of the dimensions was zero.
    InvalidSize {
        /// Input width.
        input_width: u32,
        /// Input height.
        input_height: u32,
        /// Output width.
        output_width: u32,
        /// Output height.
        output_height: u32,
    },
    /// The requested output is smaller than the render resolution.
    OutputSmallerThanInput {
        /// Input width.
        input_width: u32,
        /// Input height.
        input_height: u32,
        /// Output width.
        output_width: u32,
        /// Output height.
        output_height: u32,
    },
    /// The requested scale exceeds FSR 1's 4x-area (2x per axis) range.
    ScaleTooLarge {
        /// Input width.
        input_width: u32,
        /// Input height.
        input_height: u32,
        /// Output width.
        output_width: u32,
        /// Output height.
        output_height: u32,
    },
}

impl fmt::Display for FsrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedInputFormat(format) => {
                write!(
                    formatter,
                    "unsupported FSR input format {format:?}: expected Rgba8/Bgra8 Unorm or UnormSrgb"
                )
            }
            Self::UnsupportedOutputFormat(format) => {
                write!(
                    formatter,
                    "unsupported FSR output format {format:?}: expected Rgba8/Bgra8 Unorm or UnormSrgb"
                )
            }
            Self::OutputFormatMismatch { expected, actual } => write!(
                formatter,
                "FSR output format mismatch: pipeline expects {expected:?}, view is {actual:?}"
            ),
            Self::InvalidSize {
                input_width,
                input_height,
                output_width,
                output_height,
            } => write!(
                formatter,
                "invalid FSR size {input_width}x{input_height} -> {output_width}x{output_height}: dimensions must be non-zero"
            ),
            Self::OutputSmallerThanInput {
                input_width,
                input_height,
                output_width,
                output_height,
            } => write!(
                formatter,
                "FSR cannot downscale {input_width}x{input_height} -> {output_width}x{output_height}"
            ),
            Self::ScaleTooLarge {
                input_width,
                input_height,
                output_width,
                output_height,
            } => write!(
                formatter,
                "FSR scale {input_width}x{input_height} -> {output_width}x{output_height} exceeds the 2x-per-axis limit"
            ),
        }
    }
}

impl std::error::Error for FsrError {}

/// Whether a view in this format can be passed to [`FsrUpscaler::upscale`].
///
/// Only 8-bit `Unorm`/`UnormSrgb` formats are supported, matching FSR 1's
/// 32-bpp perceptual-input requirement and the renderer's LDR targets.
pub fn is_supported_format(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Rgba8Unorm
            | wgpu::TextureFormat::Rgba8UnormSrgb
            | wgpu::TextureFormat::Bgra8Unorm
            | wgpu::TextureFormat::Bgra8UnormSrgb
    )
}

/// Validates FSR input/output dimensions before any GPU work is recorded.
///
/// Both dimensions must be non-zero, the output must be at least as large as
/// the input in both axes, and the linear scale must not exceed
/// [`MAX_LINEAR_SCALE`]. Zero-sized textures cannot be created by wgpu, but
/// this function is also used by tests and tools that compute sizes on the CPU.
pub fn validate_sizes(
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
) -> Result<(), FsrError> {
    if input_width == 0 || input_height == 0 || output_width == 0 || output_height == 0 {
        return Err(FsrError::InvalidSize {
            input_width,
            input_height,
            output_width,
            output_height,
        });
    }
    if output_width < input_width || output_height < input_height {
        return Err(FsrError::OutputSmallerThanInput {
            input_width,
            input_height,
            output_width,
            output_height,
        });
    }
    if output_width as f32 > input_width as f32 * MAX_LINEAR_SCALE
        || output_height as f32 > input_height as f32 * MAX_LINEAR_SCALE
    {
        return Err(FsrError::ScaleTooLarge {
            input_width,
            input_height,
            output_width,
            output_height,
        });
    }
    Ok(())
}

/// RCAS lobe scale for a sharpness value in FSR 1 "stops".
///
/// `0.0` maps to `1.0` (maximum sharpening), larger values halve the lobe per
/// stop, and non-finite input falls back to [`DEFAULT_RCAS_SHARPNESS`]. The
/// result is clamped to the range produced by `0.0..=2.0`.
pub fn rcas_lobe_scale(sharpness: f32) -> f32 {
    let stops = if sharpness.is_finite() {
        sharpness.clamp(0.0, MAX_RCAS_SHARPNESS)
    } else {
        DEFAULT_RCAS_SHARPNESS
    };
    (-stops).exp2()
}

/// EASU constants and the input-transfer-function flag (group 0, binding 0).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct FsrEasuUniform {
    con0: [f32; 4],
    con1: [f32; 4],
    con2: [f32; 4],
    con3: [f32; 4],
    params: [f32; 4],
}

// Keep the Rust layout in sync with `fsr_easu.wgsl`.
const _: () = assert!(std::mem::size_of::<FsrEasuUniform>() == EASU_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<FsrEasuUniform>() == FSR_UNIFORM_ALIGN);
const _: () = assert!(std::mem::offset_of!(FsrEasuUniform, con0) == 0);
const _: () = assert!(std::mem::offset_of!(FsrEasuUniform, con1) == 16);
const _: () = assert!(std::mem::offset_of!(FsrEasuUniform, con2) == 32);
const _: () = assert!(std::mem::offset_of!(FsrEasuUniform, con3) == 48);
const _: () = assert!(std::mem::offset_of!(FsrEasuUniform, params) == 64);

impl FsrEasuUniform {
    /// FSR 1's `FsrEasuCon`: output pixel index to input position mapping.
    ///
    /// The input viewport and input resource are the same size here, so the
    /// dynamic-resolution offset variant is not needed.
    fn new(input_width: u32, input_height: u32, output_width: u32, output_height: u32, input_srgb: bool) -> Self {
        let input_width = input_width as f32;
        let input_height = input_height as f32;
        let output_width = output_width as f32;
        let output_height = output_height as f32;
        let ratio_x = input_width / output_width;
        let ratio_y = input_height / output_height;
        let inv_x = 1.0 / input_width;
        let inv_y = 1.0 / input_height;
        Self {
            // Output pixel position to viewport pixel position, then the
            // half-texel convention of the reference.
            con0: [ratio_x, ratio_y, 0.5 * ratio_x - 0.5, 0.5 * ratio_y - 0.5],
            // Viewport pixel position to normalized image space; upper-left
            // of the 'F' tap is one texel right and one texel up.
            con1: [inv_x, inv_y, inv_x, -inv_y],
            con2: [-inv_x, 2.0 * inv_y, inv_x, 2.0 * inv_y],
            con3: [0.0, 4.0 * inv_y, 0.0, 0.0],
            params: [if input_srgb { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
        }
    }
}

/// RCAS parameters (group 0, binding 0).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct FsrRcasUniform {
    params: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<FsrRcasUniform>() == RCAS_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<FsrRcasUniform>() == FSR_UNIFORM_ALIGN);
const _: () = assert!(std::mem::offset_of!(FsrRcasUniform, params) == 0);

impl FsrRcasUniform {
    fn new(sharpness: f32, output_srgb: bool) -> Self {
        Self {
            params: [
                rcas_lobe_scale(sharpness),
                if output_srgb { 1.0 } else { 0.0 },
                0.0,
                0.0,
            ],
        }
    }
}

/// Output-resolution EASU intermediate owned by [`FsrUpscaler`].
struct FsrIntermediate {
    width: u32,
    height: u32,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Precompiled EASU + RCAS upscaler for one output format.
///
/// Create once per device and destination format. [`Self::upscale`] records two
/// full-screen passes into the caller's encoder and writes the upscaled LDR
/// image into `output`; it reallocates only its internal intermediate when the
/// output size changes. Call it at most once per command submission (the
/// uniform buffers are persistent and a second call before submission would
/// overwrite the first call's constants).
pub struct FsrUpscaler {
    device: wgpu::Device,
    queue: wgpu::Queue,
    output_format: wgpu::TextureFormat,
    easu_layout: wgpu::BindGroupLayout,
    rcas_layout: wgpu::BindGroupLayout,
    easu_pipeline: wgpu::RenderPipeline,
    rcas_pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    easu_uniform: wgpu::Buffer,
    rcas_uniform: wgpu::Buffer,
    intermediate: Option<FsrIntermediate>,
    last_easu: Option<FsrEasuUniform>,
    last_rcas: Option<FsrRcasUniform>,
}

impl fmt::Debug for FsrUpscaler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsrUpscaler")
            .field("output_format", &self.output_format)
            .field(
                "intermediate",
                &self.intermediate.as_ref().map(|target| (target.width, target.height)),
            )
            .finish_non_exhaustive()
    }
}

impl FsrUpscaler {
    /// Creates the two pipelines, layouts, sampler and uniform buffers.
    ///
    /// `output_format` must be one of the formats accepted by
    /// [`is_supported_format`]; it is the format of every view later passed as
    /// `output` to [`Self::upscale`]. The *input* format is detected per call,
    /// so the same upscaler can consume an sRGB post target or a non-sRGB
    /// capture target; an input-format switch only changes a uniform flag. The
    /// output format is fixed for the upscaler's lifetime because the RCAS
    /// pipeline is compiled for it.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output_format: wgpu::TextureFormat,
    ) -> Result<Self, FsrError> {
        if !is_supported_format(output_format) {
            return Err(FsrError::UnsupportedOutputFormat(output_format));
        }

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fsr-linear-clamp-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let easu_layout = create_easu_layout(device);
        let rcas_layout = create_rcas_layout(device);
        let easu_shader = create_shader_module(device, "fsr-easu-shader", include_str!("../assets/fsr/fsr_easu.wgsl"));
        let rcas_shader = create_shader_module(device, "fsr-rcas-shader", include_str!("../assets/fsr/fsr_rcas.wgsl"));
        let easu_pipeline = create_fsr_pipeline(
            device,
            &easu_shader,
            &easu_layout,
            "fsr-easu-pipeline",
            "fs_easu",
            INTERMEDIATE_FORMAT,
        );
        let rcas_pipeline = create_fsr_pipeline(
            device,
            &rcas_shader,
            &rcas_layout,
            "fsr-rcas-pipeline",
            "fs_rcas",
            output_format,
        );

        let easu_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fsr-easu-uniform"),
            size: EASU_UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let rcas_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fsr-rcas-uniform"),
            size: RCAS_UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            output_format,
            easu_layout,
            rcas_layout,
            easu_pipeline,
            rcas_pipeline,
            sampler,
            easu_uniform,
            rcas_uniform,
            intermediate: None,
            last_easu: None,
            last_rcas: None,
        })
    }

    /// Destination format this upscaler was compiled for.
    pub const fn output_format(&self) -> wgpu::TextureFormat {
        self.output_format
    }

    /// Current intermediate size, or `None` before the first [`Self::upscale`].
    pub fn intermediate_size(&self) -> Option<(u32, u32)> {
        self.intermediate.as_ref().map(|target| (target.width, target.height))
    }

    /// Records EASU + RCAS, writing the upscaled LDR image into `output`.
    ///
    /// `input` is the tonemapped scene at render resolution, `output` the final
    /// sRGB (or perceptual non-sRGB) destination at display resolution.
    /// `sharpness` follows FSR 1's stops convention, see
    /// [`DEFAULT_RCAS_SHARPNESS`]. Invalid sizes, unsupported formats and a
    /// destination format that differs from [`Self::new`]'s argument are
    /// rejected before any pass is recorded.
    ///
    /// The caller must draw the UI after this call and must skip FSR entirely
    /// when the quality is [`FsrQuality::Off`]. Both views must cover their full
    /// mip-0 texture, use the texture's base format (no sRGB view
    /// reinterpretation), and `input` must have `TEXTURE_BINDING`; `output` must
    /// have `RENDER_ATTACHMENT`.
    pub fn upscale(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        input: &wgpu::TextureView,
        output: &wgpu::TextureView,
        sharpness: f32,
    ) -> Result<(), FsrError> {
        let input_format = input.texture().format();
        if !is_supported_format(input_format) {
            return Err(FsrError::UnsupportedInputFormat(input_format));
        }
        let output_format = output.texture().format();
        if output_format != self.output_format {
            return Err(FsrError::OutputFormatMismatch {
                expected: self.output_format,
                actual: output_format,
            });
        }
        let input_width = input.texture().width();
        let input_height = input.texture().height();
        let output_width = output.texture().width();
        let output_height = output.texture().height();
        validate_sizes(input_width, input_height, output_width, output_height)?;

        // The intermediate is the only resource that follows a resize.
        let needs_intermediate = self
            .intermediate
            .as_ref()
            .is_none_or(|target| target.width != output_width || target.height != output_height);
        if needs_intermediate {
            self.intermediate = Some(self.create_intermediate(output_width, output_height));
        }
        let intermediate_view = self
            .intermediate
            .as_ref()
            .expect("intermediate was just created")
            .view
            .clone();

        // Upload uniforms only when their contents change. This keeps repeated
        // same-size, same-sharpness calls free and avoids redundant queue work.
        let easu_uniform = FsrEasuUniform::new(
            input_width,
            input_height,
            output_width,
            output_height,
            input_format.is_srgb(),
        );
        if self.last_easu != Some(easu_uniform) {
            self.queue
                .write_buffer(&self.easu_uniform, 0, bytemuck::bytes_of(&easu_uniform));
            self.last_easu = Some(easu_uniform);
        }
        let rcas_uniform = FsrRcasUniform::new(sharpness, output_format.is_srgb());
        if self.last_rcas != Some(rcas_uniform) {
            self.queue
                .write_buffer(&self.rcas_uniform, 0, bytemuck::bytes_of(&rcas_uniform));
            self.last_rcas = Some(rcas_uniform);
        }

        let easu_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fsr-easu-bind-group"),
            layout: &self.easu_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.easu_uniform,
                        offset: 0,
                        size: wgpu::BufferSize::new(EASU_UNIFORM_SIZE),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(input),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let rcas_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fsr-rcas-bind-group"),
            layout: &self.rcas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.rcas_uniform,
                        offset: 0,
                        size: wgpu::BufferSize::new(RCAS_UNIFORM_SIZE),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&intermediate_view),
                },
            ],
        });

        // EASU: render-resolution linear/perceptual LDR -> output-resolution
        // perceptual intermediate.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fsr-easu"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.easu_pipeline);
            pass.set_bind_group(0, &easu_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        // RCAS: output-resolution perceptual intermediate -> final LDR.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fsr-rcas"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.rcas_pipeline);
            pass.set_bind_group(0, &rcas_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        Ok(())
    }

    fn create_intermediate(&self, width: u32, height: u32) -> FsrIntermediate {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fsr-easu-intermediate"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: INTERMEDIATE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("fsr-easu-intermediate-view"),
            ..Default::default()
        });
        FsrIntermediate {
            width,
            height,
            _texture: texture,
            view,
        }
    }
}

fn create_shader_module(device: &wgpu::Device, label: &str, source: &'static str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    })
}

fn create_easu_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("fsr-easu-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(EASU_UNIFORM_SIZE),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn create_rcas_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("fsr-rcas-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(RCAS_UNIFORM_SIZE),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    })
}

fn create_fsr_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    bind_group_layout: &wgpu::BindGroupLayout,
    label: &str,
    fragment_entry: &str,
    target_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_fsr"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // CPU tests: presets, sizes, layouts and validation.
    // ------------------------------------------------------------------

    #[test]
    fn quality_presets_scale_labels_and_parsing() {
        let expected = [
            (FsrQuality::Off, 1.0, "Off", "off"),
            (FsrQuality::UltraQuality, 0.77, "Ultra Quality", "ultra_quality"),
            (FsrQuality::Quality, 0.67, "Quality", "quality"),
            (FsrQuality::Balanced, 0.59, "Balanced", "balanced"),
            (FsrQuality::Performance, 0.50, "Performance", "performance"),
        ];
        assert_eq!(FsrQuality::ALL.len(), expected.len());
        for (quality, scale, label, key) in expected {
            assert!((quality.scale_factor() - scale).abs() < 1e-6, "{quality:?}");
            assert_eq!(quality.label(), label);
            assert_eq!(quality.key(), key);
            assert_eq!(FsrQuality::parse(key), Some(quality));
            assert_eq!(FsrQuality::parse(label), Some(quality));
            assert_eq!(FsrQuality::parse(&label.to_ascii_uppercase()), Some(quality));
            assert_eq!(FsrQuality::from_setting(key), Some(quality));
            assert_eq!(FsrQuality::parse(&scale.to_string()), Some(quality));
        }
        assert_eq!(FsrQuality::parse("ultra-quality"), Some(FsrQuality::UltraQuality));
        assert_eq!(FsrQuality::parse(" ultra quality "), Some(FsrQuality::UltraQuality));
        assert_eq!(FsrQuality::parse("balanced"), Some(FsrQuality::Balanced));
        assert_eq!(FsrQuality::parse("fast"), None);
        assert_eq!(FsrQuality::parse(""), None);
        assert!(!FsrQuality::Off.is_enabled());
        assert!(FsrQuality::Performance.is_enabled());
        assert_eq!(FsrQuality::default(), FsrQuality::Off);
    }

    #[test]
    fn render_sizes_are_scaled_even_and_clamped() {
        assert_eq!(FsrQuality::Off.render_size(1920, 1080), (1920, 1080));
        assert_eq!(FsrQuality::Off.render_size(1921, 1081), (1921, 1081));
        assert_eq!(FsrQuality::Off.render_size(0, 0), (1, 1));

        assert_eq!(FsrQuality::UltraQuality.render_size(1920, 1080), (1478, 832));
        assert_eq!(FsrQuality::Quality.render_size(1920, 1080), (1286, 724));
        assert_eq!(FsrQuality::Balanced.render_size(1920, 1080), (1134, 638));
        assert_eq!(FsrQuality::Performance.render_size(1920, 1080), (960, 540));

        // Odd output sizes round and then snap to even; Off stays native.
        assert_eq!(FsrQuality::Quality.render_size(1001, 777), (672, 522));
        assert_eq!(FsrQuality::Performance.render_size(1001, 777), (502, 390));

        // Minimum render size, never above the display size.
        assert_eq!(FsrQuality::Quality.render_size(20, 20), (16, 16));
        assert_eq!(FsrQuality::Performance.render_size(9, 9), (9, 9));
        assert_eq!(FsrQuality::Performance.render_size(0, 0), (1, 1));

        for quality in FsrQuality::ALL {
            for (width, height) in [(320, 200), (1920, 1080), (1366, 768), (101, 77)] {
                let (render_width, render_height) = quality.render_size(width, height);
                assert!(render_width >= 1 && render_height >= 1);
                assert!(render_width <= width.max(1));
                assert!(render_height <= height.max(1));
                if quality.is_enabled() {
                    assert!(render_width >= MIN_RENDER_DIMENSION.min(width.max(1)));
                    assert!(render_height >= MIN_RENDER_DIMENSION.min(height.max(1)));
                    // Even output unless the display itself is smaller than
                    // the minimum or odd at a tiny size.
                    if width >= MIN_RENDER_DIMENSION {
                        assert_eq!(render_width % 2, 0, "{quality:?} {width}x{height}");
                    }
                    if height >= MIN_RENDER_DIMENSION {
                        assert_eq!(render_height % 2, 0, "{quality:?} {width}x{height}");
                    }
                    validate_sizes(render_width, render_height, width.max(1), height.max(1))
                        .expect("preset render size must be valid");
                }
            }
        }
    }

    #[test]
    fn shader_sources_contain_entry_points_and_attribution() {
        const EASU: &str = include_str!("../assets/fsr/fsr_easu.wgsl");
        const RCAS: &str = include_str!("../assets/fsr/fsr_rcas.wgsl");
        for (name, source) in [("fsr_easu.wgsl", EASU), ("fsr_rcas.wgsl", RCAS)] {
            assert!(source.contains("fn vs_fsr"), "{name} has no vertex entry point");
            assert!(source.contains("@vertex"), "{name} has no @vertex attribute");
            assert!(source.contains("@fragment"), "{name} has no @fragment attribute");
            assert!(
                source.contains("SPDX-License-Identifier: MIT"),
                "{name} lacks the SPDX header"
            );
            assert!(source.contains("FidelityFX"), "{name} lacks AMD attribution");
        }
        assert!(EASU.contains("fn fs_easu"));
        assert!(RCAS.contains("fn fs_rcas"));
        assert!(EASU.contains("textureGather"));
        assert!(RCAS.contains("textureLoad"));

        let license_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fsr/LICENSE-FSR.txt");
        let license = std::fs::read_to_string(&license_path).expect("FSR license asset");
        assert!(license.contains("FidelityFX Super Resolution"));
        assert!(license.contains("Permission is hereby granted"));
        assert!(license.contains("THE SOFTWARE IS PROVIDED"));
    }

    #[test]
    fn uniform_layouts_match_the_shaders() {
        assert_eq!(EASU_UNIFORM_SIZE, 4 * 4 * 5);
        assert_eq!(RCAS_UNIFORM_SIZE, 4 * 4);
        assert_eq!(std::mem::size_of::<FsrEasuUniform>(), EASU_UNIFORM_SIZE as usize);
        assert_eq!(std::mem::size_of::<FsrRcasUniform>(), RCAS_UNIFORM_SIZE as usize);
        assert_eq!(std::mem::align_of::<FsrEasuUniform>(), FSR_UNIFORM_ALIGN);
        assert_eq!(std::mem::align_of::<FsrRcasUniform>(), FSR_UNIFORM_ALIGN);
        assert_eq!(std::mem::offset_of!(FsrEasuUniform, params), 64);
    }

    #[test]
    fn size_validation_rejects_zero_downscale_and_overscale() {
        assert!(validate_sizes(960, 540, 1920, 1080).is_ok());
        assert!(validate_sizes(960, 540, 960, 540).is_ok());
        assert!(matches!(
            validate_sizes(0, 540, 1920, 1080),
            Err(FsrError::InvalidSize { .. })
        ));
        assert!(matches!(
            validate_sizes(1920, 0, 1920, 1080),
            Err(FsrError::InvalidSize { .. })
        ));
        assert!(matches!(
            validate_sizes(320, 180, 160, 90),
            Err(FsrError::OutputSmallerThanInput { .. })
        ));
        assert!(matches!(
            validate_sizes(320, 180, 320, 90),
            Err(FsrError::OutputSmallerThanInput { .. })
        ));
        assert!(matches!(
            validate_sizes(100, 100, 400, 400),
            Err(FsrError::ScaleTooLarge { .. })
        ));
    }

    #[test]
    fn rcas_lobe_scale_is_bounded_and_monotonic() {
        assert_eq!(rcas_lobe_scale(0.0), 1.0);
        assert!((rcas_lobe_scale(DEFAULT_RCAS_SHARPNESS) - 0.870_550_8).abs() < 1e-5);
        assert_eq!(rcas_lobe_scale(MAX_RCAS_SHARPNESS), 0.25);
        assert_eq!(rcas_lobe_scale(100.0), 0.25);
        assert_eq!(rcas_lobe_scale(-5.0), 1.0);
        assert_eq!(rcas_lobe_scale(f32::NAN), rcas_lobe_scale(DEFAULT_RCAS_SHARPNESS));
        let mut previous = 1.0;
        for step in 0..=20 {
            let scale = rcas_lobe_scale(step as f32 * 0.1);
            assert!(scale <= previous + 1e-6);
            previous = scale;
        }
    }

    // ------------------------------------------------------------------
    // Offscreen GPU tests. They skip cleanly when no adapter is available.
    // ------------------------------------------------------------------

    struct Gpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
    }

    impl Gpu {
        fn new(test: &str) -> Option<Self> {
            if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
                eprintln!("skipping {test}: OPENBLOCK_NO_GPU_TESTS is set");
                return None;
            }
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let request = |power_preference: wgpu::PowerPreference| wgpu::RequestAdapterOptions {
                power_preference,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            };
            let adapter = pollster::block_on(instance.request_adapter(&request(wgpu::PowerPreference::LowPower)))
                .or_else(|_| pollster::block_on(instance.request_adapter(&request(wgpu::PowerPreference::default()))))
                .map_err(|error| eprintln!("skipping {test}: no GPU adapter ({error})"))
                .ok()?;
            let info = adapter.get_info();
            eprintln!("{test} GPU: {} ({}, {})", info.name, info.backend, info.driver);
            let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("openblock-fsr-tests"),
                ..Default::default()
            }))
            .map_err(|error| eprintln!("skipping {test}: device request failed ({error})"))
            .ok()?;
            Some(Self { device, queue })
        }
    }

    fn create_texture(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        usage: wgpu::TextureUsages,
        label: &str,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some(label),
            ..Default::default()
        });
        (texture, view)
    }

    fn upload_input(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        pixels: &[u8],
    ) -> (wgpu::Texture, wgpu::TextureView) {
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let (texture, view) = create_texture(
            gpu,
            width,
            height,
            format,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            "fsr-test-input",
        );
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        (texture, view)
    }

    fn create_output(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        create_texture(
            gpu,
            width,
            height,
            format,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            "fsr-test-output",
        )
    }

    fn read_texture(gpu: &Gpu, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
        let row_bytes = width * 4;
        let bytes_per_row = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fsr-test-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fsr-test-readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll for FSR readback");
        receiver
            .recv()
            .expect("map callback")
            .expect("map FSR output for readback");
        let mut pixels = Vec::with_capacity((row_bytes * height) as usize);
        {
            let data = slice.get_mapped_range().expect("mapped FSR output");
            for row in 0..height as usize {
                let start = row * bytes_per_row as usize;
                pixels.extend_from_slice(&data[start..start + row_bytes as usize]);
            }
        }
        buffer.unmap();
        pixels
    }

    /// A synthetic sRGB LDR image: a hard vertical edge between a dark cool
    /// half and a bright warm half, with a gentle vertical shade so the FSR
    /// output has more than one colour to sanity-check.
    fn synthetic_image(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let edge = width / 2;
        for y in 0..height {
            let shade = (y * 59 / height.max(1)) as u8;
            for x in 0..width {
                let (red, green, blue) = if x < edge {
                    (24 + shade / 3, 52 + shade / 2, 116 + shade)
                } else {
                    (196 + shade / 2, 172 + shade / 3, 56 + shade / 4)
                };
                let index = ((y * width + x) * 4) as usize;
                pixels[index] = red;
                pixels[index + 1] = green;
                pixels[index + 2] = blue;
                pixels[index + 3] = 255;
            }
        }
        pixels
    }

    fn uniform_image(width: u32, height: u32, colour: [u8; 3]) -> Vec<u8> {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[colour[0], colour[1], colour[2], 255]);
        }
        pixels
    }

    fn luma(red: u8, green: u8, blue: u8) -> f64 {
        0.2126 * f64::from(red) + 0.7152 * f64::from(green) + 0.0722 * f64::from(blue)
    }

    fn pixel_luma(pixels: &[u8], width: u32, x: u32, y: u32) -> f64 {
        let index = ((y * width + x) * 4) as usize;
        luma(pixels[index], pixels[index + 1], pixels[index + 2])
    }

    /// Sharpness metrics in a band around a vertical edge.
    ///
    /// `mean_squared_gradient` weights the central jump more than a monotonic
    /// blur does; `max_gradient` measures how abrupt the transition remains.
    fn edge_metrics(pixels: &[u8], width: u32, height: u32, edge_x: u32, band: u32) -> (f64, f64) {
        let mut max_gradient = 0.0f64;
        let mut sum_squared = 0.0f64;
        let mut count = 0u64;
        for y in (height / 4)..(3 * height / 4) {
            for x in edge_x.saturating_sub(band)..(edge_x + band).min(width - 1) {
                let gradient = (pixel_luma(pixels, width, x + 1, y) - pixel_luma(pixels, width, x, y)).abs();
                max_gradient = max_gradient.max(gradient);
                sum_squared += gradient * gradient;
                count += 1;
            }
        }
        (max_gradient, sum_squared / count.max(1) as f64)
    }

    /// Naive bilinear upscale of the perceptual sRGB bytes, used as the
    /// "without FSR" reference for the sharpness comparison.
    fn bilinear_reference(input: &[u8], in_width: u32, in_height: u32, out_width: u32, out_height: u32) -> Vec<u8> {
        let mut output = vec![0u8; (out_width * out_height * 4) as usize];
        for y in 0..out_height {
            let source_y = (f64::from(y) + 0.5) * f64::from(in_height) / f64::from(out_height) - 0.5;
            let y0 = source_y.floor().clamp(0.0, f64::from(in_height - 1)) as u32;
            let y1 = (y0 + 1).min(in_height - 1);
            let fy = (source_y - source_y.floor()).clamp(0.0, 1.0);
            for x in 0..out_width {
                let source_x = (f64::from(x) + 0.5) * f64::from(in_width) / f64::from(out_width) - 0.5;
                let x0 = source_x.floor().clamp(0.0, f64::from(in_width - 1)) as u32;
                let x1 = (x0 + 1).min(in_width - 1);
                let fx = (source_x - source_x.floor()).clamp(0.0, 1.0);
                let mut channels = [0u8; 4];
                for (channel, value) in channels.iter_mut().enumerate() {
                    let sample = |x: u32, y: u32| f64::from(input[((y * in_width + x) * 4) as usize + channel]);
                    let top = sample(x0, y0) * (1.0 - fx) + sample(x1, y0) * fx;
                    let bottom = sample(x0, y1) * (1.0 - fx) + sample(x1, y1) * fx;
                    *value = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
                }
                let index = ((y * out_width + x) * 4) as usize;
                output[index..index + 4].copy_from_slice(&channels);
            }
        }
        output
    }

    fn run_upscale(
        gpu: &Gpu,
        upscaler: &mut FsrUpscaler,
        input_pixels: &[u8],
        in_width: u32,
        in_height: u32,
        out_width: u32,
        out_height: u32,
    ) -> Vec<u8> {
        let (input_texture, input_view) = upload_input(
            gpu,
            in_width,
            in_height,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            input_pixels,
        );
        let (output_texture, output_view) =
            create_output(gpu, out_width, out_height, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fsr-test-frame"),
        });
        upscaler
            .upscale(&mut encoder, &input_view, &output_view, DEFAULT_RCAS_SHARPNESS)
            .expect("FSR upscale");
        gpu.queue.submit([encoder.finish()]);
        let pixels = read_texture(gpu, &output_texture, out_width, out_height);
        drop(input_texture);
        drop(output_texture);
        drop(input_view);
        drop(output_view);
        pixels
    }

    #[test]
    fn gpu_easu_rcas_validate_and_keep_edges_sharp() {
        let Some(gpu) = Gpu::new("gpu_easu_rcas_validate_and_keep_edges_sharp") else {
            return;
        };
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut upscaler = FsrUpscaler::new(&gpu.device, &gpu.queue, wgpu::TextureFormat::Rgba8UnormSrgb)
            .expect("create FSR upscaler");
        let creation_error = pollster::block_on(scope.pop());
        assert!(
            creation_error.is_none(),
            "FSR pipeline creation validation error: {creation_error:?}"
        );
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        assert_eq!(upscaler.output_format(), wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(upscaler.intermediate_size(), None);

        let out_width = 320;
        let out_height = 180;
        let mut summaries = Vec::new();
        for quality in [FsrQuality::Quality, FsrQuality::Performance] {
            let (in_width, in_height) = quality.render_size(out_width, out_height);
            let input = synthetic_image(in_width, in_height);
            let fsr = run_upscale(&gpu, &mut upscaler, &input, in_width, in_height, out_width, out_height);
            assert_eq!(fsr.len(), (out_width * out_height * 4) as usize);
            assert_eq!(upscaler.intermediate_size(), Some((out_width, out_height)));

            let non_black = fsr
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2]) > 24)
                .count();
            assert!(
                non_black > (out_width * out_height / 4) as usize,
                "{quality:?} output is mostly black ({non_black} lit pixels)"
            );
            let mean = |channel: usize| {
                fsr.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|pixel| f64::from(pixel[channel]))
                    .sum::<f64>()
                    / f64::from(out_width * out_height)
            };
            let means = [mean(0), mean(1), mean(2)];
            assert!(
                means.iter().all(|value| *value > 20.0),
                "{quality:?} is too dark: {means:?}"
            );
            // The synthetic edge is at half width; left half is dark and cool,
            // right half bright and warm, so the channels must differ.
            let left_index = ((out_height / 2 * out_width + out_width / 8) * 4) as usize;
            let right_index = ((out_height / 2 * out_width + 7 * out_width / 8) * 4) as usize;
            let left_luma = luma(fsr[left_index], fsr[left_index + 1], fsr[left_index + 2]);
            let right_luma = luma(fsr[right_index], fsr[right_index + 1], fsr[right_index + 2]);
            assert!(right_luma > left_luma + 80.0, "{quality:?} lost the edge contrast");

            let reference = bilinear_reference(&input, in_width, in_height, out_width, out_height);
            let edge_x = out_width / 2;
            let (fsr_max, fsr_squared) = edge_metrics(&fsr, out_width, out_height, edge_x, 8);
            let (reference_max, reference_squared) = edge_metrics(&reference, out_width, out_height, edge_x, 8);
            eprintln!(
                "{quality:?}: {in_width}x{in_height} -> {out_width}x{out_height}; \
                 FSR max/mean^2 gradient = {fsr_max:.2}/{fsr_squared:.2}, \
                 bilinear = {reference_max:.2}/{reference_squared:.2}"
            );

            assert!(
                fsr_max >= reference_max * 1.1,
                "{quality:?} edge is not sharper than bilinear: FSR {fsr_max:.2} vs bilinear {reference_max:.2}"
            );
            assert!(
                fsr_squared >= reference_squared * 1.05,
                "{quality:?} edge energy is not above bilinear: FSR {fsr_squared:.2} vs bilinear {reference_squared:.2}"
            );
            summaries.push((quality.label(), fsr_max, reference_max));
        }
        assert_eq!(summaries.len(), 2);

        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "wgpu validation error: {error:?}");
    }

    #[test]
    fn gpu_resize_formats_and_errors_are_handled() {
        let Some(gpu) = Gpu::new("gpu_resize_formats_and_errors_are_handled") else {
            return;
        };
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut upscaler =
            FsrUpscaler::new(&gpu.device, &gpu.queue, wgpu::TextureFormat::Rgba8UnormSrgb).expect("FSR upscaler");
        let creation_error = pollster::block_on(scope.pop());
        assert!(
            creation_error.is_none(),
            "FSR pipeline creation validation error: {creation_error:?}"
        );
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);

        // Resize the output twice; the intermediate follows.
        let first = synthetic_image(160, 90);
        let pixels = run_upscale(&gpu, &mut upscaler, &first, 160, 90, 320, 180);
        assert_eq!(pixels.len(), (320 * 180 * 4) as usize);
        assert_eq!(upscaler.intermediate_size(), Some((320, 180)));

        let second = synthetic_image(250, 140);
        let pixels = run_upscale(&gpu, &mut upscaler, &second, 250, 140, 500, 280);
        assert_eq!(pixels.len(), (500 * 280 * 4) as usize);
        assert_eq!(upscaler.intermediate_size(), Some((500, 280)));
        assert!(pixels.iter().any(|value| *value > 0));

        // A uniform perceptual colour must round-trip through EASU and RCAS
        // with both sRGB transfer functions applied exactly once. This catches
        // a double encode/decode and a broken filter weight normalisation.
        let uniform = uniform_image(96, 54, [128, 64, 32]);
        let pixels = run_upscale(&gpu, &mut upscaler, &uniform, 96, 54, 192, 108);
        for pixel in pixels.as_chunks::<4>().0 {
            for (channel, expected) in pixel[..3].iter().zip([128i16, 64, 32]) {
                assert!(
                    (i16::from(*channel) - expected).abs() <= 3,
                    "uniform colour drifted: {pixel:?}, expected {expected}"
                );
            }
            assert_eq!(pixel[3], 255);
        }

        // A non-sRGB input is assumed to already be perceptual; the same
        // upscaler must accept it and switch its EASU transfer-function flag.
        let perceptual = synthetic_image(160, 90);
        let (non_srgb_texture, non_srgb_view) =
            upload_input(&gpu, 160, 90, wgpu::TextureFormat::Rgba8Unorm, &perceptual);
        let (output_texture, output_view) = create_output(&gpu, 320, 180, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fsr-format-frame"),
        });
        upscaler
            .upscale(&mut encoder, &non_srgb_view, &output_view, DEFAULT_RCAS_SHARPNESS)
            .expect("non-sRGB input");
        gpu.queue.submit([encoder.finish()]);
        let pixels = read_texture(&gpu, &output_texture, 320, 180);
        assert!(pixels.iter().any(|value| *value > 0));

        // A BGRA8 sRGB destination is a supported format variant. The shader
        // still writes RGBA channels; the API applies the memory swizzle, so
        // the readback of the blue-ish left half must have byte 0 (blue) above
        // byte 2 (red).
        let mut bgra_upscaler =
            FsrUpscaler::new(&gpu.device, &gpu.queue, wgpu::TextureFormat::Bgra8UnormSrgb).expect("BGRA FSR upscaler");
        let (bgra_texture, bgra_view) = create_output(&gpu, 320, 180, wgpu::TextureFormat::Bgra8UnormSrgb);
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fsr-bgra-frame"),
        });
        bgra_upscaler
            .upscale(&mut encoder, &non_srgb_view, &bgra_view, DEFAULT_RCAS_SHARPNESS)
            .expect("BGRA output");
        gpu.queue.submit([encoder.finish()]);
        let pixels = read_texture(&gpu, &bgra_texture, 320, 180);
        let index = ((90 * 320 + 40) * 4) as usize;
        assert!(
            pixels[index] > pixels[index + 2],
            "BGRA output lost the channel swap: {:?}",
            &pixels[index..index + 4]
        );

        // Invalid sizes are rejected before any pass is recorded.
        let (large_input_texture, large_input_view) = upload_input(
            &gpu,
            320,
            180,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            &synthetic_image(320, 180),
        );
        let (small_output_texture, small_output_view) =
            create_output(&gpu, 160, 90, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fsr-error-frame"),
        });
        let error = upscaler
            .upscale(
                &mut encoder,
                &large_input_view,
                &small_output_view,
                DEFAULT_RCAS_SHARPNESS,
            )
            .expect_err("downscale must be rejected");
        assert!(matches!(error, FsrError::OutputSmallerThanInput { .. }), "{error:?}");
        assert!(matches!(
            validate_sizes(160, 90, 0, 0),
            Err(FsrError::InvalidSize { .. })
        ));
        assert!(matches!(
            validate_sizes(150, 90, 320, 180),
            Err(FsrError::ScaleTooLarge { .. })
        ));

        // Format mismatch is rejected before recording.
        let (wrong_format_texture, wrong_format_view) = create_output(&gpu, 320, 180, wgpu::TextureFormat::Rgba8Unorm);
        let error = upscaler
            .upscale(&mut encoder, &non_srgb_view, &wrong_format_view, DEFAULT_RCAS_SHARPNESS)
            .expect_err("format mismatch must be rejected");
        assert!(matches!(error, FsrError::OutputFormatMismatch { .. }), "{error:?}");

        // Unsupported input and output formats are rejected.
        let (float_input_texture, float_input_view) = create_texture(
            &gpu,
            160,
            90,
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureUsages::TEXTURE_BINDING,
            "fsr-float-input",
        );
        let error = upscaler
            .upscale(&mut encoder, &float_input_view, &output_view, DEFAULT_RCAS_SHARPNESS)
            .expect_err("float input must be rejected");
        assert!(matches!(error, FsrError::UnsupportedInputFormat(_)), "{error:?}");
        assert!(matches!(
            FsrUpscaler::new(&gpu.device, &gpu.queue, wgpu::TextureFormat::Rgba16Float),
            Err(FsrError::UnsupportedOutputFormat(_))
        ));
        assert!(is_supported_format(wgpu::TextureFormat::Bgra8UnormSrgb));
        assert!(!is_supported_format(wgpu::TextureFormat::Rgba16Float));

        drop((
            non_srgb_texture,
            non_srgb_view,
            output_texture,
            small_output_texture,
            bgra_texture,
            bgra_view,
            large_input_texture,
            large_input_view,
            wrong_format_texture,
            float_input_texture,
            float_input_view,
            output_view,
            small_output_view,
            wrong_format_view,
        ));
        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "wgpu validation error: {error:?}");
    }

    /// Procedural panel used by the optional comparison image: sky gradient,
    /// checkerboard ground, and a bright diagonal stripe crossing a hard edge.
    fn detailed_pixel(nx: f64, ny: f64) -> [u8; 4] {
        let horizon = 0.48 + 0.05 * (nx * std::f64::consts::TAU * 1.5).sin();
        let mut colour = if ny < horizon {
            let t = (ny / horizon).clamp(0.0, 1.0);
            [
                (60.0 + 60.0 * t) as u8,
                (120.0 + 80.0 * t) as u8,
                (200.0 + 40.0 * t) as u8,
            ]
        } else {
            let cell = ((nx * 40.0).floor() as i32 + (ny * 22.0).floor() as i32).rem_euclid(2);
            if cell == 0 { [74, 148, 68] } else { [40, 96, 44] }
        };
        let stripe = (nx * 6.0 - ny * 10.0).rem_euclid(1.0);
        if (0.22..0.28).contains(&stripe) {
            colour = [246, 196, 64];
        }
        [colour[0], colour[1], colour[2], 255]
    }

    fn detailed_image(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let nx = (f64::from(x) + 0.5) / f64::from(width);
                let ny = (f64::from(y) + 0.5) / f64::from(height);
                let index = ((y * width + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&detailed_pixel(nx, ny));
            }
        }
        pixels
    }

    #[test]
    fn gpu_optional_comparison_image() {
        if std::env::var_os("OPENBLOCK_FSR_COMPARE").is_none() {
            eprintln!("skipping comparison image: set OPENBLOCK_FSR_COMPARE=1 to write /tmp/openblock-fsr-compare.png");
            return;
        }
        let Some(gpu) = Gpu::new("gpu_optional_comparison_image") else {
            return;
        };
        let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out_width = 640;
        let out_height = 360;
        let mut upscaler =
            FsrUpscaler::new(&gpu.device, &gpu.queue, wgpu::TextureFormat::Rgba8UnormSrgb).expect("FSR upscaler");

        let (quality_width, quality_height) = FsrQuality::Quality.render_size(out_width, out_height);
        let quality_input = detailed_image(quality_width, quality_height);
        let quality_bilinear = bilinear_reference(&quality_input, quality_width, quality_height, out_width, out_height);
        let quality_fsr = run_upscale(
            &gpu,
            &mut upscaler,
            &quality_input,
            quality_width,
            quality_height,
            out_width,
            out_height,
        );

        let (performance_width, performance_height) = FsrQuality::Performance.render_size(out_width, out_height);
        let performance_input = detailed_image(performance_width, performance_height);
        let performance_fsr = run_upscale(
            &gpu,
            &mut upscaler,
            &performance_input,
            performance_width,
            performance_height,
            out_width,
            out_height,
        );

        let gap = 4u32;
        let panel_width = out_width;
        let total_width = panel_width * 3 + gap * 2;
        let mut image = vec![255u8; (total_width * out_height * 4) as usize];
        for (panel, pixels) in [
            quality_bilinear.as_slice(),
            quality_fsr.as_slice(),
            performance_fsr.as_slice(),
        ]
        .into_iter()
        .enumerate()
        {
            let x_offset = panel as u32 * (panel_width + gap);
            for y in 0..out_height {
                let source_start = (y * out_width * 4) as usize;
                let destination_start = ((y * total_width + x_offset) * 4) as usize;
                image[destination_start..destination_start + (out_width * 4) as usize]
                    .copy_from_slice(&pixels[source_start..source_start + (out_width * 4) as usize]);
            }
        }

        let path = std::path::Path::new("/tmp/openblock-fsr-compare.png");
        let file = std::fs::File::create(path).expect("create FSR comparison image");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), total_width, out_height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header");
        writer.write_image_data(&image).expect("PNG data");
        eprintln!(
            "wrote {} (left: bilinear {out_width}x{out_height}; middle: FSR Quality from {quality_width}x{quality_height}; right: FSR Performance from {performance_width}x{performance_height})",
            path.display()
        );

        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "wgpu validation error: {error:?}");
    }
}

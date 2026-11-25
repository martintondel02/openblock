//! The GPU renderer built on wgpu: voxel meshing, mesh upload and shading.
//!
//! v1.1 scope: an HDR forward scene with sun-shadow cascades, animated
//! procedural clouds, a transparent water pass with Fresnel and sun glints,
//! HDR bloom, ACES-fit tonemapping and FXAA-lite. The windowed renderer and the
//! windowless [`OffscreenRenderer`] share one private scene renderer, so a
//! graphics preset looks identical in both, and every GPU
//! pipeline is created once at start-up: quality changes only reallocate the
//! shadow map and update uniforms.
//!
//! The public model is still simple: a [`Camera`], an [`Environment`] clock and
//! a [`SceneParams`] bundle. Dropped items are cubes, sections are uploaded as
//! opaque and water vertex buffers, and far terrain is merged into LOD batches.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

mod cloud_shadow;
pub mod entities;
pub mod environment;
pub mod far_terrain;
mod frame;
pub mod frustum;
pub mod fsr;
pub mod graphics;
pub mod lod;
pub mod mesh;
pub mod offscreen;
pub mod outline;
mod pipeline;
pub mod resource_pack;
pub mod shadow;
mod ssao;
pub mod textures;
pub mod tonemap;
pub mod ui;
pub mod ui_font;

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use ob_math::{Mat4, SectionPos, WorldVec};
use winit::window::Window;

pub use entities::{EntityInstance, entity_layer, entity_layers};
pub use environment::{DAY_LENGTH_TICKS, Environment, EnvironmentVisual};
pub use far_terrain::{
    FAR_RECENTER_HYSTERESIS, FAR_TIER_BOUNDARIES, FAR_TIER_CELLS, FAR_TIER_OVERLAP_CELLS, FarBiome, FarSample,
    FarTerrainBuild, FarTerrainConfig, FarTerrainMesh, FarTerrainStats, FarTerrainVertex, FarTier, FarTierStats,
    build_far_terrain,
};

pub use frustum::{FRUSTUM_PLANE_COUNT, aabb_visible, aabb_within_distance, section_bounds, section_visible};
pub use graphics::{
    BLOOM_STRENGTH, BLOOM_THRESHOLD, CloudQuality, DEFAULT_EXPOSURE, GraphicsPreset, GraphicsSettings, MAX_EXPOSURE,
    MIN_EXPOSURE, ShadowConfig, ShadowQuality, WaterQuality,
};
pub use lod::{
    DEFAULT_FULL_DETAIL_RADIUS_CHUNKS, DEFAULT_LOD_GROUP_CHUNKS, DEFAULT_SHELL_DEPTH_BLOCKS, LodBatchKey, LodBatchMesh,
    LodCacheStats, LodConfig, LodTier, mesh_lod_batch, shell_section_range, skipped_sections_for_column,
    tier_for_group,
};
pub use mesh::{MesherStats, SNAPSHOT_SIZE, SectionSnapshot, mesh_section, mesh_section_with_stats};
pub use offscreen::OffscreenRenderer;
pub use outline::{BlockOutline, DEFAULT_EDGE_COLOR, DEFAULT_INFLATION, OUTLINE_VERTEX_COUNT, OutlineVertex};
pub use resource_pack::{ResourcePackLayers, TextureLoadError, TextureSource, load_resource_pack};
pub use shadow::{MAX_SHADOW_CASCADES, ShadowCascade, ShadowCascades};
pub use textures::{TexturePack, TexturePackStyle};
pub use ui::{GLYPH_SIZE, LINE_HEIGHT, UiFrame, UiRenderer, UiStats, UiTexture, UiVertex, glyph_advance, line_height};
pub use ui_font::{
    UI_GLYPH_SIZE, UI_LINE_HEIGHT, UiFontAtlas, UiFontError, default_ui_font, default_ui_font_name, rasterize_ui_font,
};

/// Camera used to render the world.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Eye position in world space.
    pub position: WorldVec,
    /// Yaw in radians (0 faces -Z).
    pub yaw: f32,
    /// Pitch in radians (positive looks up).
    pub pitch: f32,
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Near plane distance in metres.
    pub near: f32,
    /// Far plane distance in metres.
    pub far: f32,
}

impl Camera {
    /// Creates a camera.
    pub fn new(position: WorldVec, yaw: f32, pitch: f32) -> Self {
        Self {
            position,
            yaw,
            pitch,
            fov_y: 70f32.to_radians(),
            near: 0.05,
            far: 512.0,
        }
    }

    /// Unit forward vector.
    pub fn forward(&self) -> [f32; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        [-sy * cp, sp, -cy * cp]
    }

    /// Unit right vector, independent of pitch.
    ///
    /// This is the camera's `+X` axis in world space and matches the
    /// right-handed view matrix used by [`Camera::view_projection`].
    pub fn right(&self) -> [f32; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        [cy, 0.0, -sy]
    }

    /// Unit up vector (`cross(right, forward)`).
    pub fn up(&self) -> [f32; 3] {
        let forward = self.forward();
        let right = self.right();
        [
            right[1] * forward[2] - right[2] * forward[1],
            right[2] * forward[0] - right[0] * forward[2],
            right[0] * forward[1] - right[1] * forward[0],
        ]
    }

    /// Combined view-projection matrix for an aspect ratio.
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        let forward = ob_math::forward_from_angles(self.yaw, self.pitch);
        let eye = self.position.as_vec3();
        let view = ob_math::look_to_rh(eye, forward, ob_math::RenderVec::Y);
        let proj = ob_math::perspective_rh_zo(self.fov_y, aspect.max(0.01), self.near, self.far);
        proj * view
    }

    /// The six normalised world-space frustum planes for an aspect ratio.
    ///
    /// The order is left, right, bottom, top, near, far, and the inside
    /// convention is `a·x + b·y + c·z + d >= 0`.
    pub fn frustum_planes(&self, aspect: f32) -> [[f32; 4]; FRUSTUM_PLANE_COUNT] {
        frustum::planes_from_view_projection(&self.view_projection(aspect))
    }

    /// Distance-fog `(start, end)` bounds derived from the far plane.
    ///
    /// Small/near-far-plane configurations keep the historical 55%-of-far
    /// start so the last streamed ring dissolves into the sky. Medium far
    /// planes (600..=1,000 blocks) fade from 62% to 95%. Once the far plane
    /// grows beyond 1,000 blocks — the visual far-terrain case — the haze
    /// starts at 12% and ends at 95%, so a 512-chunk (8,320-block) horizon
    /// starts at ~998 blocks and reaches the sky colour at 7,904 blocks. That
    /// avoids both the degenerate one-metre band a `far - 160` formula would
    /// produce and an unnaturally crisp eight-kilometre low-poly horizon. At
    /// the 128-chunk default the haze starts around 260 blocks, still far
    /// enough beyond the near world that the seam is only lightly veiled. Near
    /// and far geometry share this one range, so their seam can never disagree
    /// about the amount of haze. The end is clamped to at least `start + 1` so
    /// a tiny far plane cannot invert the range.
    pub fn fog_range(&self) -> (f32, f32) {
        if self.far > 1000.0 {
            // Long visual horizons need an early aerial-perspective band,
            // otherwise eight kilometres of low-poly terrain stay unnaturally
            // crisp and the far LOD reads as a flat cut-out. At the 512-chunk
            // maximum this starts near 1 km, well beyond the 512-block
            // simulated radius; at 128 chunks it starts around 260 blocks, still
            // far enough that the seam itself is only lightly veiled. The same
            // range feeds the near-world fog, so both sides of the seam agree.
            let start = self.far * 0.12;
            let end = (self.far * 0.95).max(start + 1.0);
            return (start, end);
        }
        if self.far > 600.0 {
            let start = self.far * 0.62;
            let end = (self.far * 0.95).max(start + 1.0);
            return (start, end);
        }
        let start = (self.far - 160.0).max(self.far * 0.55);
        let end = (self.far * 0.95).max(start + 1.0);
        (start, end)
    }
}

/// Far plane used for a horizontal streaming radius in chunks.
///
/// The margin keeps the last streamed ring inside the frustum even when the
/// camera is close to a column boundary; 512 blocks (32 chunks) therefore maps
/// to a 560-block far plane. The client and the offscreen benchmark share this
/// helper so their fog and culling agree. This remains the function for the
/// simulation/full-detail radius; visual far terrain uses
/// [`far_plane_for_view_distance`].
pub fn far_plane_for_render_distance(chunks: i32) -> f32 {
    (chunks.max(1) as f32 * 16.0 + 48.0).max(128.0)
}

/// Far plane used for a visual far-terrain radius in chunks.
///
/// The 128-block margin covers the far mesh's recenter hysteresis and leaves
/// room for its last, fully fogged cell. At 512 chunks this is an 8,320-block
/// far plane. The client, screenshots and the benchmark share this helper so
/// culling and fog agree everywhere.
pub fn far_plane_for_view_distance(chunks: i32) -> f32 {
    (chunks.max(1) as f32 * 16.0 + 128.0).max(192.0)
}

/// Everything a frame needs besides the camera: the time of day, an optional
/// immediate-mode UI frame, an optional targeted-block outline and the scene
/// animation clock.
///
/// The default environment is noon, so `SceneParams::default()` and
/// [`Environment::default()`] keep the pre-v0.0.5 daylight look. Existing
/// renderer methods delegate here with the default environment and no outline.
#[derive(Debug, Clone, Copy, Default)]
pub struct SceneParams<'a> {
    /// Time-of-day environment driving the sky and world light.
    pub environment: Environment,
    /// Optional HUD/menu frame drawn after post-processing.
    pub ui: Option<&'a UiFrame>,
    /// Optional targeted-block outline drawn after the water pass.
    pub outline: Option<BlockOutline>,
    /// Monotonic scene time in seconds, used for cloud and water animation.
    ///
    /// Zero is deterministic and matches the 1.0 still-frame behaviour; the
    /// windowed client passes its elapsed time, screenshots and benchmarks pass
    /// a fixed value so captures are reproducible.
    pub time_seconds: f32,
}

impl<'a> SceneParams<'a> {
    /// Creates scene parameters for an environment with no UI and no outline.
    pub fn new(environment: Environment) -> Self {
        Self {
            environment,
            ui: None,
            outline: None,
            time_seconds: 0.0,
        }
    }

    /// Attaches an immediate-mode UI frame.
    pub fn with_ui(mut self, ui: &'a UiFrame) -> Self {
        self.ui = Some(ui);
        self
    }

    /// Attaches a targeted-block outline.
    pub fn with_outline(mut self, outline: BlockOutline) -> Self {
        self.outline = Some(outline);
        self
    }

    /// Sets the animation clock.
    pub fn with_time_seconds(mut self, seconds: f32) -> Self {
        self.time_seconds = seconds;
        self
    }
}

/// One renderable vertex: world position, tile UV, texture-array layer and
/// packed sky/block light.
///
/// The layout is 32 bytes: `position` 12, `uv` 8, `layer` 4, packed light 4,
/// reserved 4. Packed light stores sky in the low byte and block light (the
/// brightest RGB channel) in the next byte, both scaled to `0..=255` after face
/// brightness, per-corner ambient occlusion and the per-quad tint are folded
/// in. Normals are reconstructed from screen-space derivatives in the fragment
/// shader. The reserved word keeps the stride 4-byte aligned at 32 bytes and is
/// available for a future second light attribute without changing the stride.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Tile-space UV covering the merged rectangle in block units.
    pub uv: [f32; 2],
    /// Index into the block texture array.
    pub layer: u32,
    /// Packed light: `sky | block << 8`, each `0..=255`.
    pub light: u32,
    /// Reserved (zero) so the vertex stride stays 32 bytes.
    pub reserved: u32,
}

impl Vertex {
    /// Packs byte-scaled sky and block-light values.
    pub const fn pack_light(sky: u8, block: u8) -> u32 {
        sky as u32 | ((block as u32) << 8)
    }

    /// Sky-light byte of this vertex (`0..=255`).
    pub const fn sky_byte(self) -> u8 {
        (self.light & 0xFF) as u8
    }

    /// Block-light byte of this vertex (`0..=255`).
    pub const fn block_byte(self) -> u8 {
        ((self.light >> 8) & 0xFF) as u8
    }

    /// Normalised sky light used by tests and shader parity checks.
    pub fn sky(&self) -> f32 {
        f32::from(self.sky_byte()) / 255.0
    }

    /// Normalised block light used by tests and shader parity checks.
    pub fn block(&self) -> f32 {
        f32::from(self.block_byte()) / 255.0
    }
}

// Keep the vertex layout and shader attribute offsets in sync.
const _: () = assert!(std::mem::size_of::<Vertex>() == 32);

/// A CPU-side mesh for one section.
///
/// Opaque geometry lives in `vertices`, fluid/water geometry in
/// `water_vertices`. The renderer uploads both into separate buffers: the
/// opaque pass draws only `vertices`, the translucent water pass only
/// `water_vertices`, and the shadow pass only opaque geometry.
#[derive(Debug, Default, Clone)]
pub struct MeshData {
    /// Opaque triangle vertices (six per merged quad).
    pub vertices: Vec<Vertex>,
    /// Water/fluid triangle vertices (six per merged quad).
    pub water_vertices: Vec<Vertex>,
}

impl MeshData {
    /// Number of triangles in both lists.
    pub fn triangle_count(&self) -> usize {
        self.vertex_count() / 3
    }

    /// Number of opaque triangles.
    pub fn opaque_triangle_count(&self) -> usize {
        self.vertices.len() / 3
    }

    /// Number of water triangles.
    pub fn water_triangle_count(&self) -> usize {
        self.water_vertices.len() / 3
    }

    /// Total number of vertices in both lists.
    pub fn vertex_count(&self) -> usize {
        self.vertices.len() + self.water_vertices.len()
    }

    /// Number of water vertices.
    pub fn water_vertex_count(&self) -> usize {
        self.water_vertices.len()
    }

    /// Whether the mesh has no geometry in either list.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty() && self.water_vertices.is_empty()
    }
}

/// Per-frame renderer counters.
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderStats {
    /// Draw calls issued for opaque chunk geometry (sections, far batches and
    /// the visual-only far heightfield).
    pub draw_calls: u32,
    /// Opaque vertices drawn from all world geometry.
    pub vertices: u64,
    /// Opaque draw calls issued for the visual-only far terrain (0 or 1).
    pub far_draw_calls: u32,
    /// Opaque far-terrain vertices drawn.
    pub far_vertices: u64,
    /// Opaque far-terrain triangles drawn.
    pub far_triangles: u64,
    /// Draw calls issued for the far-terrain water surface (0 or 1).
    pub far_water_draw_calls: u32,
    /// Far-terrain water vertices drawn.
    pub far_water_vertices: u64,
    /// Full-detail section meshes drawn after frustum and distance culling.
    pub sections: u32,
    /// Merged far-LOD batches drawn.
    pub lod_batches: u32,
    /// Vertices drawn from far-LOD batches (opaque only).
    pub lod_vertices: u64,
    /// Section meshes rejected by the frustum or distance checks.
    pub culled_sections: u32,
    /// Far-LOD batches rejected by the frustum or distance checks.
    pub culled_lod_batches: u32,
    /// Draw calls issued in the transparent water pass (near and far).
    pub water_draw_calls: u32,
    /// Vertices drawn in the transparent water pass.
    pub water_vertices: u64,
    /// Draw calls issued across all sun-shadow cascades.
    pub shadow_draw_calls: u32,
    /// Active shadow cascades this frame.
    pub shadow_cascades: u32,
    /// Shadow-map edge length in texels (0 when shadows are off).
    pub shadow_resolution: u32,
    /// Render passes recorded this frame (shadow, sky, world, water, bloom,
    /// post).
    pub passes: u32,
    /// Bloom threshold/downsample passes recorded this frame.
    pub bloom_passes: u32,
    /// FSR EASU+RCAS passes recorded this frame (0 on the native path).
    pub fsr_passes: u32,
    /// SSAO raw and blur passes recorded this frame (0 on Low/Medium).
    pub ssao_passes: u32,
}

/// Errors from renderer initialisation or presentation.
#[derive(Debug)]
pub enum RenderError {
    /// No suitable GPU adapter exists.
    NoAdapter(String),
    /// Device creation failed.
    Device(String),
    /// Surface creation or configuration failed.
    Surface(String),
    /// The window surface is out of date and must be reconfigured.
    SurfaceOutOfDate,
    /// The surface timed out or was lost.
    SurfaceLost,
    /// A block texture source could not be loaded.
    Texture(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::NoAdapter(reason) => write!(f, "no suitable GPU adapter: {reason}"),
            RenderError::Device(reason) => write!(f, "GPU device creation failed: {reason}"),
            RenderError::Surface(reason) => write!(f, "window surface error: {reason}"),
            RenderError::SurfaceOutOfDate => write!(f, "window surface is out of date"),
            RenderError::SurfaceLost => write!(f, "window surface was lost"),
            RenderError::Texture(reason) => write!(f, "block texture loading failed: {reason}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<TextureLoadError> for RenderError {
    fn from(error: TextureLoadError) -> Self {
        RenderError::Texture(error.to_string())
    }
}

/// The wgpu renderer for a window surface.
pub struct Renderer {
    /// wgpu instance.
    ///
    /// Kept alive for the lifetime of the surface and for the GPU-driven
    /// submission work planned for MS-04.
    #[allow(dead_code)]
    pub(crate) instance: wgpu::Instance,
    /// Window surface.
    pub(crate) surface: wgpu::Surface<'static>,
    /// GPU device.
    pub(crate) device: wgpu::Device,
    /// Submission queue.
    pub(crate) queue: wgpu::Queue,
    /// Surface configuration.
    pub(crate) config: wgpu::SurfaceConfiguration,
    /// Shared scene renderer: pipelines, targets, meshes and entities.
    scene: frame::SceneRenderer,
    /// Immediate-mode UI renderer (HUD, F3 overlay, menus).
    pub(crate) ui: ui::UiRenderer,
    /// Adapter description.
    pub(crate) adapter_summary: String,
    /// Window handle keeping the surface alive.
    ///
    /// The surface owns a clone of the `Arc` as well, so this field is
    /// redundant for safety but part of the declared v0.0.1 renderer state.
    #[allow(dead_code)]
    pub(crate) window: Arc<Window>,
}

impl Renderer {
    /// Creates a renderer for a window: instance, surface, adapter, device and pipeline.
    pub fn new(window: Arc<Window>) -> Result<Self, RenderError> {
        Self::new_with_style(window, TexturePackStyle::default())
    }

    /// Creates a renderer using the requested procedural texture-pack style.
    pub fn new_with_style(window: Arc<Window>, style: TexturePackStyle) -> Result<Self, RenderError> {
        Self::new_with_source(window, TextureSource::Style(style))
    }

    /// Creates a renderer with an arbitrary block texture source.
    ///
    /// `source` may be a procedural style or a user-local Minecraft resource
    /// pack. Resource-pack load failures become [`RenderError::Texture`] and
    /// leave no partially initialised renderer behind.
    pub fn new_with_source(window: Arc<Window>, source: TextureSource) -> Result<Self, RenderError> {
        Self::new_with_source_and_graphics(window, source, GraphicsSettings::default())
    }

    /// Creates a renderer with an explicit graphics preset.
    ///
    /// Used by the client so the persisted graphics settings are applied before
    /// the first frame (and therefore before any shadow map is allocated).
    pub fn new_with_source_and_graphics(
        window: Arc<Window>,
        source: TextureSource,
        graphics: GraphicsSettings,
    ) -> Result<Self, RenderError> {
        // wgpu 30's `InstanceDescriptor` has no `Default` impl; this constructor
        // is its `default()` equivalent and needs no display handle. On native
        // platforms `Arc<Window>` satisfies raw-window-handle's blanket
        // `HasWindowHandle`/`HasDisplayHandle` impls, so the surface can own a
        // clone of the window and stay `'static`.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|err| RenderError::Surface(err.to_string()))?;

        let adapter = request_adapter(&instance, &surface)?;
        let info = adapter.get_info();
        let adapter_summary = format!("{} ({}, {})", info.name, info.backend, info.driver);
        log::info!("GPU adapter: {adapter_summary}");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("openblock"),
            ..Default::default()
        }))
        .map_err(|err| RenderError::Device(err.to_string()))?;

        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb
                )
            })
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| RenderError::Surface("surface supports no texture formats".to_owned()))?;
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else if caps.present_modes.contains(&wgpu::PresentMode::AutoVsync) {
            wgpu::PresentMode::AutoVsync
        } else {
            caps.present_modes.first().copied().unwrap_or_default()
        };
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes.first().copied().unwrap_or_default()
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode,
            alpha_mode,
            view_formats: Vec::new(),
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let scene = frame::SceneRenderer::new(&device, &queue, width, height, format, &source, graphics)?;
        let ui = ui::UiRenderer::new(&device, &queue, &scene.textures, format);
        log::info!("graphics: {}", scene.graphics.summary());

        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            scene,
            ui,
            adapter_summary,
            window,
        })
    }

    /// Current drawable size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Current internal scene render size (smaller than [`Self::size`] under FSR).
    pub fn render_size(&self) -> (u32, u32) {
        self.scene.size()
    }

    /// Whether the FSR EASU+RCAS pass is active.
    pub fn fsr_enabled(&self) -> bool {
        self.scene.fsr_enabled()
    }

    /// Internal render resolution when FSR is active, else `None`.
    pub fn fsr_internal_size(&self) -> Option<(u32, u32)> {
        self.scene.fsr_internal_size()
    }

    /// One-line FSR preset/internal-resolution description for F3 and logs.
    pub fn fsr_summary(&self) -> String {
        self.scene.fsr_summary()
    }

    /// Reconfigures the surface after a resize and reallocates the HDR targets.
    /// Zero-sized sizes are ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.scene.resize(&self.device, width, height);
    }

    /// Applies a new visual quality without recompiling any scene shader.
    ///
    /// Changing the independent FSR row may compile its two pipelines once and
    /// reallocates the scene targets at the new internal resolution.
    pub fn set_graphics(&mut self, graphics: GraphicsSettings) {
        self.scene.set_graphics(&self.device, &self.queue, graphics);
        log::info!("graphics: {}", self.scene.graphics.summary());
        log::info!("fsr: {}", self.scene.fsr_summary());
    }

    /// Current visual quality.
    pub fn graphics(&self) -> GraphicsSettings {
        self.scene.graphics
    }

    /// Short shadow-mode description for F3 (`off`, `high 2048² ×3 3×3 PCF`).
    pub fn shadow_summary(&self) -> String {
        self.scene.shadow_summary()
    }

    /// Rebuilds the block texture array and every bind group that references it.
    ///
    /// Cheap enough for a settings change: procedural textures are 16x16 or 32x32
    /// per layer and the world pipeline itself does not change. A resource-pack
    /// failure returns [`RenderError::Texture`] and leaves the current textures,
    /// bind group and UI untouched.
    pub fn set_texture_source(&mut self, source: TextureSource) -> Result<(), RenderError> {
        if self.scene.textures.source() == &source {
            return Ok(());
        }
        self.scene
            .set_texture_source(&self.device, &self.queue, source.clone())?;
        // The UI renderer owns its font atlases and pipeline; only its block
        // bind group refers to the replaced textures, so keep the active UI
        // font instead of rebuilding the whole renderer.
        self.ui.set_block_textures(&self.scene.textures);
        log::info!("texture source set to {}", source.label());
        Ok(())
    }

    /// Rebuilds the block textures for a procedural style.
    ///
    /// This is the infallible compatibility wrapper around
    /// [`Self::set_texture_source`]; procedural styles always load.
    pub fn set_texture_style(&mut self, style: TexturePackStyle) {
        if let Err(error) = self.set_texture_source(TextureSource::Style(style)) {
            log::error!("failed to set texture style {}: {error}", style.name());
        }
    }

    /// Currently active procedural texture-pack style.
    ///
    /// For a resource-pack source this is the style used for missing layers
    /// (Classic by default); use [`Self::texture_source`] for the exact source.
    pub fn texture_style(&self) -> TexturePackStyle {
        self.scene.textures.style()
    }

    /// Base-level texture edge length in texels (procedural or resource pack).
    pub fn texture_resolution(&self) -> u32 {
        self.scene.textures.resolution()
    }

    /// Currently active block texture source.
    pub fn texture_source(&self) -> &TextureSource {
        self.scene.textures.source()
    }

    /// Uploads (or replaces) the mesh for a section.
    pub fn upload_mesh(&mut self, pos: SectionPos, mesh: MeshData) {
        self.scene.upload_mesh(&self.device, &self.queue, pos, mesh);
    }

    /// Removes a section mesh (streaming unload).
    pub fn remove_mesh(&mut self, pos: SectionPos) {
        self.scene.remove_mesh(pos);
    }

    /// Removes all section meshes.
    pub fn clear_meshes(&mut self) {
        self.scene.clear_meshes();
    }

    /// Uploads (or replaces) one merged far-LOD batch.
    ///
    /// `min`/`max` are the world-space bounds of the emitted vertices; the
    /// renderer uses them for frustum and distance culling. An empty mesh
    /// removes the batch instead of allocating a zero-length buffer.
    pub fn upload_lod_batch(&mut self, key: LodBatchKey, min: [f32; 3], max: [f32; 3], mesh: MeshData) {
        self.scene
            .upload_lod_batch(&self.device, &self.queue, key, min, max, mesh);
    }

    /// Removes one merged far-LOD batch.
    pub fn remove_lod_batch(&mut self, key: LodBatchKey) {
        self.scene.remove_lod_batch(key);
    }

    /// Removes every merged far-LOD batch.
    pub fn clear_lod_batches(&mut self) {
        self.scene.clear_lod_batches();
    }

    /// Number of uploaded far-LOD batches.
    pub fn lod_batch_count(&self) -> usize {
        self.scene.lod_batch_count()
    }

    /// Total vertices held by uploaded far-LOD batches.
    pub fn lod_vertex_count(&self) -> u64 {
        self.scene.lod_vertex_count()
    }

    /// Uploads (or clears, for an empty mesh) the visual-only far-terrain mesh.
    ///
    /// The geometry is fixed world-space heightfield data built by the caller;
    /// the renderer does not know about worldgen, biomes or gameplay. It is
    /// drawn depth-tested behind the near full-detail world, casts no shadows
    /// and never feeds collision, streaming or simulation. Passing a mesh with
    /// no opaque and no water triangles clears the previous mesh.
    pub fn set_far_terrain(&mut self, mesh: FarTerrainMesh) {
        self.scene.set_far_terrain(&self.device, &self.queue, mesh);
    }

    /// Removes the far-terrain mesh.
    pub fn clear_far_terrain(&mut self) {
        self.scene.clear_far_terrain();
    }

    /// World-space bounds of the uploaded far mesh, if any.
    pub fn far_terrain_bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        self.scene.far_terrain_bounds()
    }

    /// Replaces the dropped-item entities drawn with the world.
    ///
    /// The slice is copied into the renderer and kept until the next call; an
    /// empty slice disables the entity draw (no draw call is issued). Entities
    /// are drawn after the opaque world geometry and before the outline and UI
    /// passes, and they cast into the sun-shadow cascades.
    pub fn set_entities(&mut self, entities: &[EntityInstance]) {
        self.scene.set_entities(&self.device, &self.queue, entities);
    }

    /// Renders one frame with a day/night environment, an optional UI overlay
    /// and an optional targeted-block outline.
    ///
    /// Shadow cascades are rendered first. The sky (gradient, clouds, sun/moon,
    /// stars) is then drawn into the HDR target, followed by the opaque world,
    /// dropped items and the translucent water pass. The outline is drawn after
    /// water with `LessEqual` depth testing and no depth writes. Bloom and the
    /// ACES-fit post pass write the surface, and the UI is drawn last directly
    /// onto the LDR surface so text stays crisp.
    ///
    /// The statistics returned by [`Self::stats`] count shadow, opaque, water
    /// and post passes separately.
    pub fn render_scene(&mut self, camera: &Camera, scene: &SceneParams<'_>) -> Result<(), RenderError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Err(RenderError::SurfaceOutOfDate);
            }
            wgpu::CurrentSurfaceTexture::Timeout => return Err(RenderError::SurfaceLost),
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Validation => return Err(RenderError::SurfaceLost),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        self.scene
            .record(&self.device, &self.queue, &mut encoder, &view, camera, scene);
        if let Some(ui_frame) = scene.ui {
            self.ui.render(&mut encoder, &view, ui_frame);
        }
        self.queue.submit([encoder.finish()]);
        drop(view);
        self.queue.present(frame);
        Ok(())
    }

    /// Renders one frame of the world with an optional UI overlay at the default
    /// (noon) environment and without a block outline.
    pub fn render_with_ui(&mut self, camera: &Camera, ui: Option<&ui::UiFrame>) -> Result<(), RenderError> {
        self.render_scene(
            camera,
            &SceneParams {
                environment: Environment::default(),
                ui,
                outline: None,
                time_seconds: 0.0,
            },
        )
    }

    /// Renders one frame of the world without a UI overlay.
    pub fn render(&mut self, camera: &Camera) -> Result<(), RenderError> {
        self.render_with_ui(camera, None)
    }

    /// Replaces the Minecraft-style UI font used by [`UiFrame::text`].
    ///
    /// `font` is the raw contents of a TrueType/OpenType file; `None` restores
    /// the embedded Monocraft font. On error the previous font is kept and the
    /// error is returned. Titles drawn with [`UiFrame::title_text`] keep the
    /// built-in 8×8 bitmap font regardless.
    pub fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError> {
        self.ui.set_ui_font(font)
    }

    /// Display name of the active UI font: `"Monocraft"` or `"custom"`.
    pub fn ui_font_name(&self) -> &str {
        self.ui.ui_font_name()
    }

    /// Counters from the last UI frame, if any.
    pub fn ui_stats(&self) -> ui::UiStats {
        self.ui.stats()
    }

    /// Human-readable adapter description for logs and `--version`.
    pub fn adapter_summary(&self) -> &str {
        &self.adapter_summary
    }

    /// Statistics from the last rendered frame.
    pub fn stats(&self) -> RenderStats {
        self.scene.stats()
    }
}

/// Requests a compatible adapter, preferring low power.
///
/// The first request keeps the surface compatibility constraint; if it fails,
/// a second request falls back to the default power preference and only then
/// reports [`RenderError::NoAdapter`] with both messages.
fn request_adapter(instance: &wgpu::Instance, surface: &wgpu::Surface<'_>) -> Result<wgpu::Adapter, RenderError> {
    let request = |power_preference: wgpu::PowerPreference| wgpu::RequestAdapterOptions {
        power_preference,
        force_fallback_adapter: false,
        compatible_surface: Some(surface),
        apply_limit_buckets: false,
    };
    match pollster::block_on(instance.request_adapter(&request(wgpu::PowerPreference::LowPower))) {
        Ok(adapter) => Ok(adapter),
        Err(low_power_error) => {
            let default_request = request(wgpu::PowerPreference::default());
            pollster::block_on(instance.request_adapter(&default_request)).map_err(|default_error| {
                RenderError::NoAdapter(format!(
                    "low-power request failed ({low_power_error}); \
                         default-preference request failed ({default_error})"
                ))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_layout_matches_the_shader() {
        assert_eq!(std::mem::size_of::<Vertex>(), 32);
        assert_eq!(std::mem::align_of::<Vertex>(), 4);
        assert_eq!(std::mem::offset_of!(Vertex, position), 0);
        assert_eq!(std::mem::offset_of!(Vertex, uv), 12);
        assert_eq!(std::mem::offset_of!(Vertex, layer), 20);
        assert_eq!(std::mem::offset_of!(Vertex, light), 24);
        assert_eq!(std::mem::offset_of!(Vertex, reserved), 28);
    }

    #[test]
    fn mesh_data_separates_water_without_losing_totals() {
        let vertex = Vertex {
            position: [0.0; 3],
            uv: [0.0; 2],
            layer: 0,
            light: Vertex::pack_light(255, 0),
            reserved: 0,
        };
        let mesh = MeshData {
            vertices: vec![vertex; 36],
            water_vertices: vec![vertex; 6],
        };
        assert_eq!(mesh.vertex_count(), 42);
        assert_eq!(mesh.water_vertex_count(), 6);
        assert_eq!(mesh.triangle_count(), 14);
        assert_eq!(mesh.opaque_triangle_count(), 12);
        assert_eq!(mesh.water_triangle_count(), 2);
        assert!(!mesh.is_empty());
        assert!(MeshData::default().is_empty());
    }

    #[test]
    fn scene_params_default_to_noon_and_are_composable() {
        let scene = SceneParams::default();
        assert_eq!(scene.environment, Environment::default());
        assert_eq!(scene.environment.time_of_day, 0.5);
        assert!(scene.ui.is_none());
        assert!(scene.outline.is_none());
        assert_eq!(scene.time_seconds, 0.0);

        let outline = BlockOutline::for_block([1, 2, 3]);
        let scene = SceneParams::new(Environment::new(0.0))
            .with_outline(outline)
            .with_time_seconds(12.5);
        assert_eq!(scene.environment, Environment::new(0.0));
        assert_eq!(scene.outline, Some(outline));
        assert_eq!(scene.time_seconds, 12.5);
        assert!(scene.ui.is_none());
    }

    #[test]
    fn render_stats_defaults_are_zero_and_named() {
        let stats = RenderStats::default();
        assert_eq!(stats.draw_calls, 0);
        assert_eq!(stats.water_draw_calls, 0);
        assert_eq!(stats.shadow_draw_calls, 0);
        assert_eq!(stats.passes, 0);
        assert_eq!(stats.shadow_resolution, 0);
    }

    #[test]
    fn fog_range_tracks_the_far_plane() {
        let mut camera = Camera::new(WorldVec::new(0.0, 70.0, 0.0), 0.0, 0.0);
        camera.far = 200.0;
        let (start, end) = camera.fog_range();
        assert!((start - 110.0).abs() < 1e-4, "fog start is {start}");
        assert!((end - 190.0).abs() < 1e-4, "fog end is {end}");

        // At the 512-block radius the fog starts well beyond 300 blocks, so
        // the horizon stays readable instead of dissolving into grey.
        camera.far = far_plane_for_render_distance(32);
        let (start, end) = camera.fog_range();
        assert!((camera.far - 560.0).abs() < 1e-4);
        assert!((399.0..440.0).contains(&start), "fog start is {start}");
        assert!(end >= 530.0, "fog end is {end}");

        // A very short far plane keeps a usable, non-inverted range.
        camera.far = 1.0;
        let (start, end) = camera.fog_range();
        assert!(start >= 0.0);
        assert!(end >= start + 1.0, "fog range is {start}..{end}");

        // The 512-chunk visual horizon uses the long-range formula instead of
        // a degenerate one-metre fog band.
        camera.far = far_plane_for_view_distance(512);
        assert!((camera.far - 8320.0).abs() < 1e-4, "far plane is {}", camera.far);
        let (start, end) = camera.fog_range();
        assert!((start - 998.4).abs() < 0.1, "long-range fog start is {start}");
        assert!((end - 7904.0).abs() < 0.1, "long-range fog end is {end}");
        assert!(end > 6000.0 && start < end);

        // The 128-chunk default starts far enough out to keep the 512-block
        // near world crisp and well ahead of the seam.
        camera.far = far_plane_for_view_distance(128);
        let (start, end) = camera.fog_range();
        assert!(start > 200.0 && end > 2000.0);
    }

    #[test]
    fn graphics_is_exported_from_the_crate_root() {
        assert_eq!(GraphicsSettings::default().preset, GraphicsPreset::High);
        assert!(GraphicsSettings::default().shadow_config().cascades as usize <= MAX_SHADOW_CASCADES);
        let exposure = GraphicsSettings::default().normalised().exposure;
        assert!((MIN_EXPOSURE..=MAX_EXPOSURE).contains(&exposure));
    }
}

//! The GPU renderer built on wgpu: voxel meshing, mesh upload and shading.
//!
//! v0.0.1 scope: a screen-space sky gradient pass followed by one forward pass,
//! textured greedy-meshed chunk geometry with baked face shading and ambient
//! occlusion, a 16×16 block texture array, distance fog that fades chunk edges
//! into the sky horizon, a depth buffer and section frustum culling. The render
//! graph, GPU-driven submission and post chain in `docs/spec/03-rendering.md`
//! arrive with the rest of MS-04.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

pub mod entities;
pub mod environment;
pub mod frustum;
pub mod lod;
pub mod mesh;
pub mod offscreen;
pub mod outline;
mod pipeline;
pub mod resource_pack;
pub mod textures;
pub mod ui;
pub mod ui_font;

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use ob_math::{Mat4, SectionPos, WorldVec};
use winit::window::Window;

pub use entities::{EntityInstance, entity_layer, entity_layers};
pub use environment::{DAY_LENGTH_TICKS, Environment, EnvironmentVisual};
pub use frustum::{FRUSTUM_PLANE_COUNT, aabb_visible, aabb_within_distance, section_bounds, section_visible};
pub use lod::{
    DEFAULT_FULL_DETAIL_RADIUS_CHUNKS, DEFAULT_LOD_GROUP_CHUNKS, DEFAULT_SHELL_DEPTH_BLOCKS, LodBatchKey, LodBatchMesh,
    LodCacheStats, LodConfig, LodTier, mesh_lod_batch, shell_section_range, skipped_sections_for_column,
    tier_for_group,
};
pub use mesh::{MesherStats, SNAPSHOT_SIZE, SectionSnapshot, mesh_section, mesh_section_with_stats};
pub use offscreen::OffscreenRenderer;
pub use outline::{BlockOutline, DEFAULT_EDGE_COLOR, DEFAULT_INFLATION, OUTLINE_VERTEX_COUNT, OutlineVertex};
pub use resource_pack::{ResourcePackLayers, TextureLoadError, TextureSource, load_resource_pack};
pub use textures::{TexturePack, TexturePackStyle};
pub use ui::{GLYPH_SIZE, LINE_HEIGHT, UiFrame, UiRenderer, UiStats, UiTexture, UiVertex, glyph_advance, line_height};
pub use ui_font::{
    UI_GLYPH_SIZE, UI_LINE_HEIGHT, UiFontAtlas, UiFontError, default_ui_font, default_ui_font_name, rasterize_ui_font,
};

use pipeline::CameraUniform;
use textures::BlockTextureArray;

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
    /// Small render distances keep the historical 55%-of-far start so the
    /// last streamed ring always dissolves into the sky. At long distances the
    /// fog instead starts 160 blocks before the far plane (never earlier than
    /// 55%), so a 512-block world has a crisp horizon while 200 blocks away is
    /// still clear. The end is clamped to at least `start + 1` so a tiny far
    /// plane cannot invert the range. Both the windowed and offscreen renderers
    /// use this method, which keeps their fog identical for the same camera.
    pub fn fog_range(&self) -> (f32, f32) {
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
/// helper so their fog and culling agree.
pub fn far_plane_for_render_distance(chunks: i32) -> f32 {
    (chunks.max(1) as f32 * 16.0 + 48.0).max(128.0)
}

/// Everything a frame needs besides the camera: the time of day, an optional
/// immediate-mode UI frame and an optional targeted-block outline.
///
/// The default environment is noon, so `SceneParams::default()` and
/// [`Environment::default()`] keep the pre-v0.0.5 daylight look. Existing
/// renderer methods delegate here with the default environment and no outline.
#[derive(Debug, Clone, Copy, Default)]
pub struct SceneParams<'a> {
    /// Time-of-day environment driving the sky and world light.
    pub environment: Environment,
    /// Optional HUD/menu frame drawn after the world and the outline.
    pub ui: Option<&'a UiFrame>,
    /// Optional targeted-block outline drawn after the world, before the UI.
    pub outline: Option<BlockOutline>,
}

impl<'a> SceneParams<'a> {
    /// Creates scene parameters for an environment with no UI and no outline.
    pub fn new(environment: Environment) -> Self {
        Self {
            environment,
            ui: None,
            outline: None,
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
}

/// One renderable vertex: world position, tile UV, texture-array layer and
/// baked light factor.
///
/// The layout is 28 bytes: `position` 12, `uv` 8, `layer` 4, `light` 4.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Tile-space UV covering the merged rectangle in block units.
    pub uv: [f32; 2],
    /// Index into the block texture array.
    pub layer: u32,
    /// Baked light: face brightness times per-corner ambient occlusion.
    pub light: f32,
}

// Keep the vertex layout and shader attribute offsets in sync.
const _: () = assert!(std::mem::size_of::<Vertex>() == 28);

/// A CPU-side mesh for one section.
#[derive(Debug, Default, Clone)]
pub struct MeshData {
    /// Triangle vertices (six per merged quad).
    pub vertices: Vec<Vertex>,
}

impl MeshData {
    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.vertices.len() / 3
    }

    /// Whether the mesh has no geometry.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }
}

/// Per-frame renderer counters.
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderStats {
    /// Draw calls issued for chunk geometry (sections plus far LOD batches).
    pub draw_calls: u32,
    /// Vertices drawn from all world geometry.
    pub vertices: u64,
    /// Full-detail section meshes drawn after frustum and distance culling.
    pub sections: u32,
    /// Merged far-LOD batches drawn.
    pub lod_batches: u32,
    /// Vertices drawn from far-LOD batches.
    pub lod_vertices: u64,
    /// Section meshes rejected by the frustum or distance checks.
    pub culled_sections: u32,
    /// Far-LOD batches rejected by the frustum or distance checks.
    pub culled_lod_batches: u32,
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

/// GPU resources for one section mesh.
#[derive(Debug)]
pub struct GpuMesh {
    /// Vertex buffer.
    pub buffer: wgpu::Buffer,
    /// Number of vertices in the buffer.
    pub vertex_count: u32,
}

/// GPU resources for one merged far-LOD batch.
#[derive(Debug)]
pub struct LodGpuMesh {
    /// Vertex buffer.
    pub buffer: wgpu::Buffer,
    /// Number of vertices in the buffer.
    pub vertex_count: u32,
    /// Conservative world-space bounds for frustum and distance culling.
    pub min: [f32; 3],
    /// Conservative world-space bounds for frustum and distance culling.
    pub max: [f32; 3],
}

/// The wgpu renderer.
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
    /// World render pipeline.
    pub(crate) pipeline: wgpu::RenderPipeline,
    /// Full-screen sky gradient pipeline.
    pub(crate) sky_pipeline: wgpu::RenderPipeline,
    /// Targeted-block outline line-list pipeline.
    pub(crate) outline_pipeline: wgpu::RenderPipeline,
    /// Dropped-item cube pipeline and instance buffers.
    pub(crate) entities: entities::EntitiesRenderer,
    /// Depth texture view.
    pub(crate) depth_view: wgpu::TextureView,
    /// Camera, fog and environment uniform buffer.
    pub(crate) uniform_buffer: wgpu::Buffer,
    /// Small per-frame outline vertex buffer (24 line-list vertices).
    pub(crate) outline_buffer: wgpu::Buffer,
    /// Block texture array and sampler (procedural or user resource pack).
    ///
    /// Kept alive here even though the bind group also retains its view.
    _textures: BlockTextureArray,
    /// Immediate-mode UI renderer (HUD, F3 overlay, menus).
    pub(crate) ui: ui::UiRenderer,
    /// Camera and texture bind group.
    pub(crate) bind_group: wgpu::BindGroup,
    /// Uploaded section meshes.
    pub(crate) meshes: HashMap<SectionPos, GpuMesh>,
    /// Uploaded merged far-LOD batches.
    pub(crate) lod_batches: HashMap<LodBatchKey, LodGpuMesh>,
    /// Adapter description.
    pub(crate) adapter_summary: String,
    /// Statistics from the last frame.
    pub(crate) stats: RenderStats,
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
        let depth_view = pipeline::create_depth_view(&device, width, height);

        let uniform_buffer = pipeline::create_camera_buffer(&device);
        let outline_buffer = pipeline::create_outline_buffer(&device);
        let textures = BlockTextureArray::new_with_source(&device, &queue, &source)?;
        let ui = ui::UiRenderer::new(&device, &queue, &textures, format);
        let bind_group_layout = pipeline::create_bind_group_layout(&device);
        let bind_group = pipeline::create_bind_group(&device, &bind_group_layout, &uniform_buffer, &textures);
        let pipeline = pipeline::create_pipeline(&device, format, &bind_group_layout);
        let sky_pipeline = pipeline::create_sky_pipeline(&device, format, &bind_group_layout);
        let outline_pipeline = pipeline::create_outline_pipeline(&device, format, &bind_group_layout);
        let entity_renderer = entities::EntitiesRenderer::new(&device, &queue, format, &bind_group_layout);

        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            sky_pipeline,
            outline_pipeline,
            entities: entity_renderer,
            depth_view,
            uniform_buffer,
            outline_buffer,
            _textures: textures,
            ui,
            bind_group,
            meshes: HashMap::new(),
            lod_batches: HashMap::new(),
            adapter_summary,
            stats: RenderStats::default(),
            window,
        })
    }

    /// Current drawable size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Reconfigures the surface after a resize. Zero-sized sizes are ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.depth_view = pipeline::create_depth_view(&self.device, width, height);
    }

    /// Rebuilds the block texture array and every bind group that references it.
    ///
    /// Cheap enough for a settings change: procedural textures are 16x16 or 32x32
    /// per layer and the world pipeline itself does not change. A resource-pack
    /// failure returns [`RenderError::Texture`] and leaves the current textures,
    /// bind group and UI untouched.
    pub fn set_texture_source(&mut self, source: TextureSource) -> Result<(), RenderError> {
        if self._textures.source() == &source {
            return Ok(());
        }
        let textures = BlockTextureArray::new_with_source(&self.device, &self.queue, &source)?;
        let layout = pipeline::create_bind_group_layout(&self.device);
        self.bind_group = pipeline::create_bind_group(&self.device, &layout, &self.uniform_buffer, &textures);
        // The UI renderer owns its font atlases and pipeline; only its block
        // bind group refers to the replaced textures, so keep the active UI
        // font instead of rebuilding the whole renderer.
        self.ui.set_block_textures(&textures);
        self._textures = textures;
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
        self._textures.style()
    }

    /// Currently active block texture source.
    /// Base-level texture edge length in texels (procedural or resource pack).
    pub fn texture_resolution(&self) -> u32 {
        self._textures.resolution()
    }

    pub fn texture_source(&self) -> &TextureSource {
        self._textures.source()
    }

    /// Uploads (or replaces) the mesh for a section.
    pub fn upload_mesh(&mut self, pos: SectionPos, mesh: MeshData) {
        if mesh.vertices.is_empty() {
            self.meshes.remove(&pos);
            return;
        }
        let vertex_count = mesh.vertices.len() as u32;
        let size = (mesh.vertices.len() * std::mem::size_of::<Vertex>()) as wgpu::BufferAddress;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("section-mesh"),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&buffer, 0, bytemuck::cast_slice(&mesh.vertices));
        self.meshes.insert(pos, GpuMesh { buffer, vertex_count });
    }

    /// Removes a section mesh (streaming unload).
    pub fn remove_mesh(&mut self, pos: SectionPos) {
        self.meshes.remove(&pos);
    }

    /// Removes all section meshes.
    pub fn clear_meshes(&mut self) {
        self.meshes.clear();
    }

    /// Uploads (or replaces) one merged far-LOD batch.
    ///
    /// `min`/`max` are the world-space bounds of the emitted vertices; the
    /// renderer uses them for frustum and distance culling. An empty mesh
    /// removes the batch instead of allocating a zero-length buffer.
    pub fn upload_lod_batch(&mut self, key: LodBatchKey, min: [f32; 3], max: [f32; 3], mesh: MeshData) {
        if mesh.vertices.is_empty() {
            self.lod_batches.remove(&key);
            return;
        }
        let vertex_count = mesh.vertices.len() as u32;
        let size = (mesh.vertices.len() * std::mem::size_of::<Vertex>()) as wgpu::BufferAddress;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lod-batch"),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&buffer, 0, bytemuck::cast_slice(&mesh.vertices));
        self.lod_batches.insert(
            key,
            LodGpuMesh {
                buffer,
                vertex_count,
                min,
                max,
            },
        );
    }

    /// Removes one merged far-LOD batch.
    pub fn remove_lod_batch(&mut self, key: LodBatchKey) {
        self.lod_batches.remove(&key);
    }

    /// Removes every merged far-LOD batch.
    pub fn clear_lod_batches(&mut self) {
        self.lod_batches.clear();
    }

    /// Number of uploaded far-LOD batches.
    pub fn lod_batch_count(&self) -> usize {
        self.lod_batches.len()
    }

    /// Total vertices held by uploaded far-LOD batches.
    pub fn lod_vertex_count(&self) -> u64 {
        self.lod_batches.values().map(|mesh| u64::from(mesh.vertex_count)).sum()
    }

    /// Replaces the dropped-item entities drawn with the world.
    ///
    /// The slice is copied into the renderer and kept until the next call; an
    /// empty slice disables the entity draw (no draw call is issued). Entities
    /// are drawn after the world geometry and before the outline and UI passes.
    pub fn set_entities(&mut self, entities: &[EntityInstance]) {
        self.entities.set(&self.device, &self.queue, entities);
    }

    /// Renders one frame with a day/night environment, an optional UI overlay
    /// and an optional targeted-block outline.
    ///
    /// The sky gradient, sun/moon and stars are drawn first into a cleared
    /// colour target (no depth). The world pass loads that colour, clears depth
    /// and draws the chunk meshes. Dropped-item entities are drawn next in the
    /// same pass, depth-tested against the world. The outline is drawn after
    /// them, with `LessEqual` depth testing and depth writes disabled, so it is
    /// hidden by nearer terrain but never occludes anything. The UI frame is
    /// drawn last with alpha blending and no depth attachment.
    ///
    /// The uploaded outline, entity instances and the uniform are shared by the
    /// whole frame; the statistics returned by [`Self::stats`] still count chunk
    /// draws only.
    pub fn render_scene(&mut self, camera: &Camera, scene: &SceneParams<'_>) -> Result<(), RenderError> {
        let (width, height) = self.size();
        let aspect = width as f32 / height as f32;
        let uniform = CameraUniform::new(camera, aspect, scene.environment);
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        let frustum = camera.frustum_planes(aspect);

        // A non-finite outline is skipped rather than uploaded as NaNs.
        let outline_vertices = scene
            .outline
            .filter(BlockOutline::is_finite)
            .map(|outline| outline.vertices());
        if let Some(vertices) = &outline_vertices {
            self.queue
                .write_buffer(&self.outline_buffer, 0, bytemuck::cast_slice(vertices));
        }

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

        // Sort the draws so the submitted command stream is deterministic even
        // though meshes live in a hash map; depth testing makes the result
        // independent of draw order either way. Culled sections are skipped
        // and never counted in the frame statistics.
        let mut order: Vec<SectionPos> = self.meshes.keys().copied().collect();
        order.sort_unstable();
        let mut lod_order: Vec<LodBatchKey> = self.lod_batches.keys().copied().collect();
        lod_order.sort_unstable();
        let eye = [
            camera.position.x as f32,
            camera.position.y as f32,
            camera.position.z as f32,
        ];
        // A small margin keeps geometry whose AABB touches the far plane from
        // popping a frame early. The client already evicts beyond the streaming
        // radius; this check is the renderer's own guard against stale caches.
        let draw_distance = camera.far + 24.0;

        let mut stats = RenderStats::default();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            // Sky first, with no depth attachment: one full-screen triangle.
            // The clear colour follows the current horizon as a fallback.
            let mut sky = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sky"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(uniform.sky_clear()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            sky.set_pipeline(&self.sky_pipeline);
            sky.set_bind_group(0, &self.bind_group, &[]);
            sky.draw(0..3, 0..1);
        }
        {
            // The world loads the sky already written above and clears depth
            // independently.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            for pos in order {
                let (min, max) = section_bounds(pos);
                if !section_visible(&frustum, pos) || !aabb_within_distance(eye, min, max, draw_distance) {
                    stats.culled_sections += 1;
                    continue;
                }
                let Some(mesh) = self.meshes.get(&pos) else {
                    continue;
                };
                if mesh.vertex_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, mesh.buffer.slice(..));
                pass.draw(0..mesh.vertex_count, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(mesh.vertex_count);
                stats.sections += 1;
            }
            // Far LOD batches are one draw per 2×2-chunk group. They are drawn
            // after the near sections; depth testing makes the order visually
            // irrelevant, and near sections win any overlap at the tier seam.
            for key in lod_order {
                let Some(batch) = self.lod_batches.get(&key) else {
                    continue;
                };
                if batch.vertex_count == 0
                    || !aabb_visible(&frustum, batch.min, batch.max)
                    || !aabb_within_distance(eye, batch.min, batch.max, draw_distance)
                {
                    stats.culled_lod_batches += 1;
                    continue;
                }
                pass.set_vertex_buffer(0, batch.buffer.slice(..));
                pass.draw(0..batch.vertex_count, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(batch.vertex_count);
                stats.lod_batches += 1;
                stats.lod_vertices += u64::from(batch.vertex_count);
            }
            // Dropped items sit in the world: depth-test them against the
            // terrain, then draw the outline over both.
            self.entities.draw(&mut pass, &self.bind_group);
            // Outline after the world geometry: the depth buffer now contains
            // the nearest opaque surface, so the outline is correctly hidden
            // behind terrain. Depth writes stay off; the UI pass follows.
            if outline_vertices.is_some() {
                pass.set_pipeline(&self.outline_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.outline_buffer.slice(..));
                pass.draw(0..OUTLINE_VERTEX_COUNT as u32, 0..1);
            }
        }
        self.stats = stats;
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
        self.stats
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
        assert_eq!(std::mem::size_of::<Vertex>(), 28);
        assert_eq!(std::mem::align_of::<Vertex>(), 4);
        assert_eq!(std::mem::offset_of!(Vertex, position), 0);
        assert_eq!(std::mem::offset_of!(Vertex, uv), 12);
        assert_eq!(std::mem::offset_of!(Vertex, layer), 20);
        assert_eq!(std::mem::offset_of!(Vertex, light), 24);
    }

    #[test]
    fn scene_params_default_to_noon_and_are_composable() {
        let scene = SceneParams::default();
        assert_eq!(scene.environment, Environment::default());
        assert_eq!(scene.environment.time_of_day, 0.5);
        assert!(scene.ui.is_none());
        assert!(scene.outline.is_none());

        let outline = BlockOutline::for_block([1, 2, 3]);
        let scene = SceneParams::new(Environment::new(0.0)).with_outline(outline);
        assert_eq!(scene.environment, Environment::new(0.0));
        assert_eq!(scene.outline, Some(outline));
        assert!(scene.ui.is_none());
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
    }
}

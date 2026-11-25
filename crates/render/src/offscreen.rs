//! Offscreen rendering: draw the world into a fixed-size colour texture and read
//! the pixels back as RGBA8.
//!
//! This path needs no window or display. It reuses the same scene renderer,
//! shader, vertex layout, texture array, camera uniform, shadow cascades, water
//! pass, bloom chain and ACES post pass as the windowed renderer, so it verifies
//! the full GPU pipeline as well as producing screenshots and benchmark frames.

use std::sync::{Arc, Mutex};

use ob_math::{N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};
use ob_world::World;

use crate::frame;
use crate::graphics::GraphicsSettings;
use crate::ui::{UiFrame, UiRenderer};
use crate::{Camera, EntityInstance, Environment, LodBatchKey, RenderError, RenderStats, SceneParams, lod, mesh};

/// Offscreen colour target format (sRGB, readable through `copy_texture_to_buffer`).
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A windowless renderer that draws the world into an RGBA8 texture.
pub struct OffscreenRenderer {
    /// GPU device.
    device: wgpu::Device,
    /// Submission queue.
    queue: wgpu::Queue,
    /// Shared scene renderer (pipelines, targets, shadows, water, post).
    scene: frame::SceneRenderer,
    /// Immediate-mode UI renderer for screenshots with HUD/menus.
    ui: UiRenderer,
    /// Offscreen colour texture.
    color_texture: wgpu::Texture,
    /// View of the colour texture used as the post-processing output target.
    color_view: wgpu::TextureView,
    /// Target width in pixels.
    width: u32,
    /// Target height in pixels.
    height: u32,
    /// Adapter description.
    adapter_summary: String,
    /// RGBA8 pixels of the last rendered frame.
    pixels: Vec<u8>,
}

impl OffscreenRenderer {
    /// Creates an offscreen renderer with a fixed `width` x `height` target.
    pub fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        Self::new_with_style(width, height, crate::TexturePackStyle::default())
    }

    /// Creates an offscreen renderer with a specific texture-pack style.
    pub fn new_with_style(width: u32, height: u32, style: crate::TexturePackStyle) -> Result<Self, RenderError> {
        Self::new_with_source(width, height, crate::TextureSource::Style(style))
    }

    /// Creates an offscreen renderer with an arbitrary block texture source.
    ///
    /// A resource-pack load failure returns [`RenderError::Texture`] before any
    /// render target or pipeline is created.
    pub fn new_with_source(width: u32, height: u32, source: crate::TextureSource) -> Result<Self, RenderError> {
        Self::new_with_source_and_graphics(width, height, source, GraphicsSettings::default())
    }

    /// Creates an offscreen renderer with an explicit graphics preset.
    pub fn new_with_source_and_graphics(
        width: u32,
        height: u32,
        source: crate::TextureSource,
        graphics: GraphicsSettings,
    ) -> Result<Self, RenderError> {
        let width = width.max(1);
        let height = height.max(1);

        // No surface and no display handle: this path works on headless hosts.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = request_adapter(&instance)?;
        let info = adapter.get_info();
        let adapter_summary = format!("{} ({}, {})", info.name, info.backend, info.driver);
        log::info!("offscreen GPU adapter: {adapter_summary}");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("openblock-offscreen"),
            ..Default::default()
        }))
        .map_err(|err| RenderError::Device(err.to_string()))?;

        let color_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-color"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("offscreen-color-view"),
            ..Default::default()
        });

        let scene = frame::SceneRenderer::new(&device, &queue, width, height, COLOR_FORMAT, &source, graphics)?;
        let ui = UiRenderer::new(&device, &queue, &scene.textures, COLOR_FORMAT);
        log::info!("graphics: {}", scene.graphics.summary());

        Ok(Self {
            device,
            queue,
            scene,
            ui,
            color_texture,
            color_view,
            width,
            height,
            adapter_summary,
            pixels: Vec::new(),
        })
    }

    /// Human-readable adapter description for logs and `--version`.
    pub fn adapter_summary(&self) -> &str {
        &self.adapter_summary
    }

    /// Statistics from the last rendered frame.
    pub fn stats(&self) -> RenderStats {
        self.scene.stats()
    }

    /// Applies a new visual quality without recompiling any shader.
    pub fn set_graphics(&mut self, graphics: GraphicsSettings) {
        self.scene.set_graphics(&self.device, graphics);
        log::info!("graphics: {}", self.scene.graphics.summary());
    }

    /// Current visual quality.
    pub fn graphics(&self) -> GraphicsSettings {
        self.scene.graphics
    }

    /// Short shadow-mode description for F3 and benchmark reports.
    pub fn shadow_summary(&self) -> String {
        self.scene.shadow_summary()
    }

    /// Current offscreen target size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Reallocates the offscreen colour target and every size-dependent scene
    /// target (HDR colour, depth, bloom mips and their bind groups).
    ///
    /// Zero-sized requests are ignored. Used by tools that capture several sizes
    /// and by the resize regression test; the next rendered frame has the new
    /// size and [`Self::read_rgba`] returns exactly `width * height * 4` bytes.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.color_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-color"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.color_view = self.color_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("offscreen-color-view"),
            ..Default::default()
        });
        self.scene.resize(&self.device, width, height);
        self.pixels.clear();
    }

    /// Debug helper: first-cascade shadow depth statistics.
    #[cfg(test)]
    pub(crate) fn debug_shadow_depth(&self) -> Option<(f32, f32, usize)> {
        self.scene.debug_shadow_depth(&self.device, &self.queue)
    }

    /// Pushes a wgpu validation error scope (tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn push_validation_scope(&self) -> wgpu::ErrorScopeGuard {
        self.device.push_error_scope(wgpu::ErrorFilter::Validation)
    }

    /// Pops a validation scope returned by [`Self::push_validation_scope`].
    #[cfg(test)]
    pub(crate) fn pop_validation_scope(scope: wgpu::ErrorScopeGuard) -> Option<wgpu::Error> {
        pollster::block_on(scope.pop())
    }

    /// Rebuilds the block texture array and its bind groups for a new source.
    ///
    /// The current textures survive a failed resource-pack load unchanged.
    pub fn set_texture_source(&mut self, source: crate::TextureSource) -> Result<(), RenderError> {
        if self.scene.textures.source() == &source {
            return Ok(());
        }
        self.scene
            .set_texture_source(&self.device, &self.queue, source.clone())?;
        // Keep the UI renderer (and any custom UI font) alive; only its block
        // bind group references the replaced textures.
        self.ui.set_block_textures(&self.scene.textures);
        log::info!("offscreen texture source set to {}", source.label());
        Ok(())
    }

    /// Replaces the Minecraft-style UI font used by [`UiFrame::text`].
    ///
    /// `font` is the raw contents of a TrueType/OpenType file; `None` restores
    /// the embedded Monocraft font. On error the previous font is kept and the
    /// error is returned. Titles drawn with [`UiFrame::title_text`] keep the
    /// built-in 8×8 bitmap font regardless.
    pub fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), crate::UiFontError> {
        self.ui.set_ui_font(font)
    }

    /// Display name of the active UI font: `"Monocraft"` or `"custom"`.
    pub fn ui_font_name(&self) -> &str {
        self.ui.ui_font_name()
    }

    /// Rebuilds the block textures for a procedural style.
    ///
    /// Infallible compatibility wrapper around [`Self::set_texture_source`].
    pub fn set_texture_style(&mut self, style: crate::TexturePackStyle) {
        if let Err(error) = self.set_texture_source(crate::TextureSource::Style(style)) {
            log::error!("failed to set offscreen texture style {}: {error}", style.name());
        }
    }

    /// Currently active procedural texture-pack style.
    ///
    /// For a resource-pack source this is the style used for missing layers;
    /// use [`Self::texture_source`] for the exact source.
    pub fn texture_style(&self) -> crate::TexturePackStyle {
        self.scene.textures.style()
    }

    /// Base-level texture edge length in texels (procedural or resource pack).
    pub fn texture_resolution(&self) -> u32 {
        self.scene.textures.resolution()
    }

    /// Currently active block texture source.
    pub fn texture_source(&self) -> &crate::TextureSource {
        self.scene.textures.source()
    }

    /// Replaces the dropped-item entities drawn with the world.
    ///
    /// The slice is copied and kept until the next call; an empty slice skips
    /// the entity draw entirely. The next rendered frame includes them and they
    /// cast into the sun-shadow cascades.
    pub fn set_entities(&mut self, entities: &[EntityInstance]) {
        self.scene.set_entities(&self.device, &self.queue, entities);
    }

    /// Uploads (or replaces) one section mesh in the cached benchmark scene.
    pub fn upload_mesh(&mut self, pos: SectionPos, section_mesh: crate::MeshData) {
        self.scene.upload_mesh(&self.device, &self.queue, pos, section_mesh);
    }

    /// Uploads (or replaces) one merged far-LOD batch in the cached scene.
    pub fn upload_lod_batch(&mut self, key: LodBatchKey, min: [f32; 3], max: [f32; 3], batch: crate::MeshData) {
        self.scene
            .upload_lod_batch(&self.device, &self.queue, key, min, max, batch);
    }

    /// Removes one far-LOD batch.
    pub fn remove_lod_batch(&mut self, key: LodBatchKey) {
        self.scene.remove_lod_batch(key);
    }

    /// Removes every far-LOD batch.
    pub fn clear_lod_batches(&mut self) {
        self.scene.clear_lod_batches();
    }

    /// Number of cached far-LOD batches.
    pub fn lod_batch_count(&self) -> usize {
        self.scene.lod_batch_count()
    }

    /// Total vertices held by cached far-LOD batches (opaque and water).
    pub fn lod_vertex_count(&self) -> u64 {
        self.scene.lod_vertex_count()
    }

    /// Uploads (or clears, for an empty mesh) the visual-only far-terrain mesh.
    ///
    /// Mirrors [`crate::Renderer::set_far_terrain`]. Screenshots, the F2 path
    /// and the offscreen benchmark use this to include the same 8,192-block
    /// horizon the windowed client draws.
    pub fn set_far_terrain(&mut self, mesh: crate::FarTerrainMesh) {
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

    /// Rebuilds the cached GPU meshes for every non-empty loaded section.
    ///
    /// Returns the number of cached sections and vertices. The benchmark path
    /// calls this once, then times cached frames so meshing and upload do not
    /// dominate the measurement.
    pub fn rebuild_mesh_cache(&mut self, world: &World) -> (u32, u64) {
        self.scene.clear_meshes();
        self.scene.clear_lod_batches();
        let mut sections = 0u32;
        let mut vertices = 0u64;
        let base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
        let mut positions: Vec<SectionPos> = world
            .chunks()
            .flat_map(|(chunk_pos, _column)| {
                (0..N_SECTIONS).map(move |index| SectionPos::new(chunk_pos.x, base_y + index as i32, chunk_pos.z))
            })
            .collect();
        positions.sort_unstable();
        for pos in positions {
            let Some(section) = world.section(pos) else {
                continue;
            };
            if section.is_empty() {
                continue;
            }
            let section_mesh = mesh::mesh_section(world, pos);
            if section_mesh.is_empty() {
                continue;
            }
            vertices += section_mesh.vertex_count() as u64;
            sections += 1;
            self.upload_mesh(pos, section_mesh);
        }
        (sections, vertices)
    }

    /// Rebuilds the cached benchmark scene with the distance-LOD policy.
    ///
    /// Near columns are meshed per section; far 2×2-chunk groups are meshed
    /// once as a surface-shell batch. Returns a summary for the benchmark
    /// report. This is the exact policy the client applies through its
    /// scheduler, implemented here without a scheduler so the offscreen tool
    /// stays dependency-free and deterministic.
    pub fn rebuild_lod_scene(
        &mut self,
        world: &World,
        camera: &Camera,
        config: &lod::LodConfig,
    ) -> crate::LodCacheStats {
        self.scene.clear_meshes();
        self.scene.clear_lod_batches();
        let config = config.normalised();
        let camera_chunk = ob_math::BlockPos::new(
            camera.position.x.floor() as i32,
            camera.position.y.floor() as i32,
            camera.position.z.floor() as i32,
        )
        .chunk();
        let mut stats = crate::LodCacheStats::default();
        let base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
        // Deterministic iteration order: chunk order is not stable in a hash
        // map, and the benchmark's counts must be reproducible.
        let mut columns: Vec<(ob_math::ChunkPos, &ob_world::ChunkColumn)> =
            world.chunks().map(|(pos, c)| (*pos, c)).collect();
        columns.sort_by_key(|(pos, _)| (pos.x, pos.z));
        let mut far_groups = std::collections::BTreeSet::new();
        for (chunk_pos, _) in &columns {
            if config.chunk_tier(camera_chunk, *chunk_pos) == crate::LodTier::Shell {
                far_groups.insert(LodBatchKey::containing(*chunk_pos, config.group_chunks));
                continue;
            }
            for index in 0..ob_math::N_SECTIONS {
                let pos = SectionPos::new(chunk_pos.x, base_y + index as i32, chunk_pos.z);
                let Some(section) = world.section(pos) else {
                    continue;
                };
                if section.is_empty() {
                    continue;
                }
                let section_mesh = mesh::mesh_section(world, pos);
                if section_mesh.is_empty() {
                    continue;
                }
                stats.sections += 1;
                stats.section_vertices += section_mesh.vertex_count() as u64;
                self.upload_mesh(pos, section_mesh);
            }
        }
        for key in far_groups {
            let batch = lod::mesh_lod_batch(world, key, &config);
            if batch.is_empty() {
                continue;
            }
            stats.batches += 1;
            stats.batch_sections += batch.shell_sections as u64;
            stats.skipped_sections += batch.skipped_sections as u64;
            stats.batch_vertices += batch.mesh.vertex_count() as u64;
            self.upload_lod_batch(key, batch.min, batch.max, batch.mesh);
        }
        stats
    }

    /// Renders the cached meshes inside the frustum and reads the frame back.
    ///
    /// Intended for benchmark loops: the returned timing includes rasterisation
    /// and the GPU-to-CPU readback, but not meshing or buffer upload.
    pub fn render_cached(&mut self, camera: &Camera) -> Result<RenderStats, RenderError> {
        self.render_cached_with_ui(camera, None)
    }

    /// Renders the cached meshes and an optional UI frame, then reads the frame back.
    pub fn render_cached_with_ui(&mut self, camera: &Camera, ui: Option<&UiFrame>) -> Result<RenderStats, RenderError> {
        self.render_cached_scene(
            camera,
            &SceneParams {
                environment: Environment::default(),
                ui,
                outline: None,
                time_seconds: 0.0,
            },
        )
    }

    /// Renders the cached meshes for a full scene and reads the frame back.
    ///
    /// This is the offscreen counterpart of [`crate::Renderer::render_scene`].
    /// The GPU-to-CPU readback is included in the returned timing, like
    /// [`Self::render_cached_with_ui`].
    pub fn render_cached_scene(
        &mut self,
        camera: &Camera,
        scene: &SceneParams<'_>,
    ) -> Result<RenderStats, RenderError> {
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen-cached-frame"),
        });
        let stats = self
            .scene
            .record(&self.device, &self.queue, &mut encoder, &self.color_view, camera, scene);
        if let Some(ui_frame) = scene.ui {
            self.ui.render(&mut encoder, &self.color_view, ui_frame);
        }
        self.queue.submit([encoder.finish()]);
        self.read_back()?;
        Ok(stats)
    }

    /// Number of cached section meshes (0 before `rebuild_mesh_cache`).
    pub fn cached_mesh_count(&self) -> usize {
        self.scene.mesh_count()
    }

    /// Renders a world using the distance-LOD policy.
    ///
    /// Screenshots and F2 captures use this path so a 512-block capture does not
    /// build tens of thousands of full section meshes or exhaust GPU memory.
    /// Render distances within the full-detail radius produce exactly the same
    /// geometry as [`Self::render_world_scene`].
    pub fn render_world_scene_lod(
        &mut self,
        world: &World,
        camera: &Camera,
        scene: &SceneParams<'_>,
        config: &lod::LodConfig,
    ) -> Result<(), RenderError> {
        let stats = self.rebuild_lod_scene(world, camera, config);
        log::info!(
            "offscreen LOD frame: {} near sections + {} far batches, {} cached vertices, {} shell sections, {} skipped",
            stats.sections,
            stats.batches,
            stats.section_vertices + stats.batch_vertices,
            stats.batch_sections,
            stats.skipped_sections
        );
        let _ = self.render_cached_scene(camera, scene)?;
        Ok(())
    }

    /// Renders every non-empty loaded section inside the frustum into the
    /// offscreen target, rebuilding the mesh cache first.
    ///
    /// Screenshots are one-shot; the cached benchmark path calls
    /// `rebuild_mesh_cache` once and then `render_cached` per frame.
    pub fn render_world(&mut self, world: &World, camera: &Camera) -> Result<(), RenderError> {
        self.render_world_with_ui(world, camera, None)
    }

    /// Like [`OffscreenRenderer::render_world`] but draws an optional UI frame.
    pub fn render_world_with_ui(
        &mut self,
        world: &World,
        camera: &Camera,
        ui: Option<&UiFrame>,
    ) -> Result<(), RenderError> {
        self.render_world_scene(
            world,
            camera,
            &SceneParams {
                environment: Environment::default(),
                ui,
                outline: None,
                time_seconds: 0.0,
            },
        )
    }

    /// Renders a world for a full scene: environment, optional outline and
    /// optional UI frame.
    ///
    /// This rebuilds the mesh cache first, exactly like
    /// [`OffscreenRenderer::render_world_with_ui`]; the benchmark path uses
    /// [`Self::rebuild_mesh_cache`] plus [`Self::render_cached_scene`] instead.
    pub fn render_world_scene(
        &mut self,
        world: &World,
        camera: &Camera,
        scene: &SceneParams<'_>,
    ) -> Result<(), RenderError> {
        self.rebuild_mesh_cache(world);
        let stats = self.render_cached_scene(camera, scene)?;
        log::info!(
            "offscreen frame: {} visible sections drawn, {} vertices, {} draws, {} water draws, {} shadow draws",
            stats.sections,
            stats.vertices,
            stats.draw_calls,
            stats.water_draw_calls,
            stats.shadow_draw_calls
        );
        Ok(())
    }

    /// Copies the colour texture into a 256-byte-row-aligned buffer, maps it and
    /// stores the unpadded RGBA8 pixels.
    fn read_back(&mut self) -> Result<(), RenderError> {
        let bytes_per_row =
            (self.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer_size = u64::from(bytes_per_row) * u64::from(self.height);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen-readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.color_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        // `map_async` only completes once the device is polled; wgpu 30's
        // `PollType::Wait` blocks until the copy has completed and callbacks ran.
        let slice = readback.slice(..);
        let mapped: Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> = Arc::new(Mutex::new(None));
        {
            let mapped = Arc::clone(&mapped);
            slice.map_async(wgpu::MapMode::Read, move |result| {
                if let Ok(mut slot) = mapped.lock() {
                    *slot = Some(result);
                }
            });
        }
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| RenderError::Device(format!("device poll failed: {err}")))?;
        let map_result = pollster::block_on(std::future::poll_fn(|_cx| {
            let mut slot = mapped.lock().expect("map callback state poisoned");
            match slot.take() {
                Some(result) => std::task::Poll::Ready(result),
                None => std::task::Poll::Pending,
            }
        }));
        if let Err(err) = map_result {
            return Err(RenderError::Device(format!("buffer mapping failed: {err}")));
        }

        self.pixels.clear();
        self.pixels.reserve_exact((self.width * self.height * 4) as usize);
        {
            let data = slice
                .get_mapped_range()
                .map_err(|err| RenderError::Device(format!("buffer view failed: {err}")))?;
            let row_bytes = (self.width * 4) as usize;
            for row in 0..self.height as usize {
                let start = row * bytes_per_row as usize;
                self.pixels.extend_from_slice(&data[start..start + row_bytes]);
            }
        }
        readback.unmap();
        Ok(())
    }

    /// Returns the RGBA8 pixels of the last rendered frame, top row first.
    pub fn read_rgba(&self) -> Vec<u8> {
        self.pixels.clone()
    }
}

/// Requests an adapter with no surface, preferring low power.
///
/// If the low-power request fails, a second request uses the default power
/// preference; only then is [`RenderError::NoAdapter`] reported with both
/// messages.
fn request_adapter(instance: &wgpu::Instance) -> Result<wgpu::Adapter, RenderError> {
    let request = |power_preference: wgpu::PowerPreference| wgpu::RequestAdapterOptions {
        power_preference,
        force_fallback_adapter: false,
        compatible_surface: None,
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
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos, Vec4, WorldVec};
    use ob_world::ChunkColumn;

    use super::*;
    use crate::BlockOutline;

    /// Builds one chunk column with a flat stone platform, a grass surface and
    /// two small pillars so the image contains several distinct shades.
    fn test_world(registry: &Arc<Registry>) -> World {
        let stone = registry.id_or_air("openblock:stone");
        let grass = registry.id_or_air("openblock:grass");
        assert_ne!(stone, BlockId::AIR);
        assert_ne!(grass, BlockId::AIR);

        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..SECTION_SIZE as i32 {
            for x in 0..SECTION_SIZE as i32 {
                column.set_block(BlockPos::new(x, 64, z), stone);
                column.set_block(BlockPos::new(x, 65, z), grass);
            }
        }
        // Pillars add top faces, side faces and ambient-occlusion shades.
        for y in 66..69 {
            column.set_block(BlockPos::new(6, y, 6), stone);
            column.set_block(BlockPos::new(9, y, 9), grass);
        }
        column.recompute_heightmap();

        let mut world = World::new(Arc::clone(registry), 1);
        world.insert_chunk(column);
        world
    }

    /// Mean RGB of one row, in sRGB bytes.
    fn row_rgb(pixels: &[u8], width: u32, y: u32) -> [f32; 3] {
        let start = (y * width * 4) as usize;
        let row = pixels[start..start + (width * 4) as usize].as_chunks::<4>().0;
        let mut sum = [0f64; 3];
        for pixel in row {
            sum[0] += f64::from(pixel[0]);
            sum[1] += f64::from(pixel[1]);
            sum[2] += f64::from(pixel[2]);
        }
        let count = row.len() as f64;
        [
            (sum[0] / count) as f32,
            (sum[1] / count) as f32,
            (sum[2] / count) as f32,
        ]
    }

    /// Euclidean distance between two sRGB colours.
    fn colour_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    }

    /// Asserts that the sky gradient only varies with Y: every row is uniform
    /// horizontally up to `tolerance` sRGB steps.
    fn assert_rows_uniform(pixels: &[u8], width: u32, height: u32, tolerance: u8) {
        for y in 0..height {
            let start = (y * width * 4) as usize;
            let row = pixels[start..start + (width * 4) as usize].as_chunks::<4>().0;
            let first = row[0];
            for pixel in row {
                assert!(
                    pixel[0].abs_diff(first[0]) <= tolerance
                        && pixel[1].abs_diff(first[1]) <= tolerance
                        && pixel[2].abs_diff(first[2]) <= tolerance,
                    "row {y} is not horizontally uniform: {first:?} vs {pixel:?}"
                );
            }
        }
    }

    /// Mean distance from a set of pixels to a target sRGB colour.
    fn mean_distance_to(pixels: &[u8], mask: &[bool], target: [f32; 3]) -> f32 {
        let mut sum = 0f64;
        let mut count = 0usize;
        for (index, selected) in mask.iter().copied().enumerate() {
            if !selected {
                continue;
            }
            let offset = index * 4;
            let colour = [
                f32::from(pixels[offset]),
                f32::from(pixels[offset + 1]),
                f32::from(pixels[offset + 2]),
            ];
            sum += f64::from(colour_distance(colour, target));
            count += 1;
        }
        if count == 0 {
            f32::INFINITY
        } else {
            (sum / count as f64) as f32
        }
    }

    #[test]
    fn offscreen_render_is_not_black() {
        // The renderer itself must not depend on worldgen or content beyond the
        // base registry, so the test builds its chunk by hand.
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);

        let mut renderer = match OffscreenRenderer::new(320, 180) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping offscreen_render_is_not_black: no GPU adapter ({reason})");
                return;
            }
            Err(err) => panic!("failed to create the offscreen renderer: {err}"),
        };
        eprintln!("offscreen test GPU: {}", renderer.adapter_summary());

        // Keep the sky deterministic for the culled-frame comparison: no
        // world-space clouds, bloom or FXAA; only the HDR tonemap remains.
        renderer.set_graphics(crate::GraphicsSettings {
            clouds: crate::CloudQuality::Off,
            bloom: false,
            fxaa: false,
            ..crate::GraphicsSettings::default()
        });
        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.6, -1.0);
        camera.far = 128.0;

        // Render the same sky pass with an empty world first, so every
        // background pixel is measured instead of assuming a clear colour.
        let empty = World::new(Arc::clone(&registry), 1);
        renderer.render_world(&empty, &camera).expect("render empty world");
        let background = renderer.read_rgba();
        assert_eq!(background.len(), 320 * 180 * 4);
        assert_eq!(renderer.stats().sections, 0);
        let top = row_rgb(&background, 320, 2);
        let bottom = row_rgb(&background, 320, 177);
        assert!(
            colour_distance(top, bottom) > 30.0,
            "the empty frame does not show a vertical sky gradient: {top:?} vs {bottom:?}"
        );

        renderer.render_world(&world, &camera).expect("render test platform");
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), 320 * 180 * 4);
        assert!(renderer.stats().sections > 0, "the test platform should be visible");
        assert_eq!(renderer.stats().sections, renderer.stats().draw_calls);
        assert!(renderer.stats().vertices > 0);

        // Count only pixels the geometry changed: the sky gradient itself is
        // not a uniform colour any more.
        let mut histogram: HashMap<[u8; 3], usize> = HashMap::new();
        let mut different = 0usize;
        for (index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
            let background_offset = index * 4;
            let differs = pixel[0].abs_diff(background[background_offset]) > 0
                || pixel[1].abs_diff(background[background_offset + 1]) > 0
                || pixel[2].abs_diff(background[background_offset + 2]) > 0;
            if differs {
                different += 1;
                *histogram.entry([pixel[0], pixel[1], pixel[2]]).or_insert(0) += 1;
            }
        }
        let total = (320 * 180) as usize;
        let fraction = different as f32 / total as f32;

        let mut top_colours: Vec<([u8; 3], usize)> = histogram.iter().map(|(k, v)| (*k, *v)).collect();
        top_colours.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
        eprintln!(
            "offscreen: differing={different}/{total} ({:.2}%), distinct geometry colours={}",
            fraction * 100.0,
            histogram.len()
        );
        for (rgb, count) in top_colours.iter().take(8) {
            eprintln!("  {rgb:?}: {count}");
        }

        assert!(
            fraction >= 0.01,
            "only {:.2}% of pixels differ from the sky background",
            fraction * 100.0
        );
        // Procedural 16×16 textures produce many more shades than the old
        // per-face vertex colours: water/stone/grass noise and AO alone give
        // well over sixteen distinct RGB values.
        assert!(
            histogram.len() >= 16,
            "only {} distinct geometry RGB values were drawn",
            histogram.len()
        );

        // A camera far above the world with a short far plane must cull every
        // section: the frame returns to the sky gradient and no section is drawn.
        // The sky is screen-space only, so the image is the same as the empty
        // world rendered above.
        let mut high_camera = Camera::new(WorldVec::new(8.0, 220.0, 8.0), 0.0, 0.0);
        high_camera.far = 64.0;
        renderer
            .render_world(&empty, &high_camera)
            .expect("render high empty world");
        let high_background = renderer.read_rgba();
        renderer
            .render_world(&world, &high_camera)
            .expect("render culled world");
        assert_eq!(renderer.stats().sections, 0);
        assert_eq!(renderer.stats().draw_calls, 0);
        let culled = renderer.read_rgba();
        assert_eq!(culled.len(), high_background.len());
        assert!(
            culled
                .iter()
                .zip(high_background.iter())
                .all(|(actual, expected)| actual.abs_diff(*expected) <= 1),
            "a fully culled frame must show the same sky gradient as an empty world",
        );
    }

    #[test]
    fn sky_gradient_is_vertical() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let empty = World::new(registry, 1);
        let mut renderer = match OffscreenRenderer::new(320, 180) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping sky_gradient_is_vertical: no GPU adapter ({reason})");
                return;
            }
            Err(err) => panic!("failed to create the offscreen renderer: {err}"),
        };

        renderer.set_graphics(crate::GraphicsSettings {
            clouds: crate::CloudQuality::Off,
            bloom: false,
            fxaa: false,
            ..crate::GraphicsSettings::default()
        });
        let camera = Camera::new(WorldVec::new(0.0, 128.0, 0.0), 0.0, 0.0);
        renderer.render_world(&empty, &camera).expect("render sky only");
        assert_eq!(renderer.stats().sections, 0);
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), 320 * 180 * 4);

        // The gradient is a function of screen Y only.
        assert_rows_uniform(&pixels, 320, 180, 8);

        let top = row_rgb(&pixels, 320, 1);
        let middle = row_rgb(&pixels, 320, 90);
        let bottom = row_rgb(&pixels, 320, 178);
        let luma = |colour: [f32; 3]| 0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2];

        eprintln!("sky gradient: top={top:?}, middle={middle:?}, bottom={bottom:?}");
        assert!(
            colour_distance(top, bottom) > 40.0,
            "top and bottom of the sky are too similar: {top:?} vs {bottom:?}"
        );
        // The zenith is darker and more saturated blue than the warm horizon.
        assert!(
            luma(top) + 20.0 < luma(bottom),
            "the zenith is not darker than the horizon: {} vs {}",
            luma(top),
            luma(bottom)
        );
        assert!(
            top[2] - top[0] > bottom[2] - bottom[0] + 40.0,
            "the zenith is not bluer than the horizon: {top:?} vs {bottom:?}"
        );
        // The screen centre is the horizon; the lower edge gets a subtle warm
        // haze, so the middle sits between the two ends in brightness.
        assert!(
            luma(top) < luma(middle),
            "the gradient is not monotonic towards the horizon"
        );
        assert!(
            luma(middle) < luma(bottom),
            "the horizon haze should not be darker than the horizon"
        );
    }

    #[test]
    fn fog_fades_geometry_into_the_sky() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let empty = World::new(Arc::clone(&registry), 1);
        let mut renderer = match OffscreenRenderer::new(320, 180) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping fog_fades_geometry_into_the_sky: no GPU adapter ({reason})");
                return;
            }
            Err(err) => panic!("failed to create the offscreen renderer: {err}"),
        };

        // Look across the platform from 40 blocks away. Its near edge is ~24
        // blocks away; with `far = 44` fog starts at 24.2 and is saturated at
        // 41.8, while the whole section still sits inside the far plane. The
        // clear configuration uses a far plane beyond any visible geometry, so
        // no fog is applied.
        let mut clear_camera = Camera::new(WorldVec::new(8.0, 76.0, 40.0), 0.0, -0.30);
        clear_camera.far = 512.0;
        let mut dense_camera = clear_camera;
        dense_camera.far = 44.0;

        // Sky-only reference for the horizon colour (screen centre).
        renderer.render_world(&empty, &clear_camera).expect("render sky only");
        let background = renderer.read_rgba();
        let horizon = row_rgb(&background, 320, 90);

        renderer
            .render_world(&world, &clear_camera)
            .expect("render clear platform");
        assert!(renderer.stats().sections > 0, "the platform must be visible");
        let clear = renderer.read_rgba();

        // The geometry mask comes from the unfogged frame. Perspective X/Y
        // coverage does not depend on the far plane, so the same screen pixels
        // show the platform in both configurations.
        let mut mask = vec![false; (320 * 180) as usize];
        for (index, pixel) in clear.as_chunks::<4>().0.iter().enumerate() {
            let offset = index * 4;
            let background_pixel = &background[offset..offset + 3];
            if pixel[0].abs_diff(background_pixel[0]) > 2
                || pixel[1].abs_diff(background_pixel[1]) > 2
                || pixel[2].abs_diff(background_pixel[2]) > 2
            {
                mask[index] = true;
            }
        }
        let geometry_pixels = mask.iter().filter(|selected| **selected).count();
        assert!(
            geometry_pixels > 100,
            "only {geometry_pixels} platform pixels were found"
        );

        renderer
            .render_world(&world, &dense_camera)
            .expect("render fogged platform");
        let dense = renderer.read_rgba();

        let clear_distance = mean_distance_to(&clear, &mask, horizon);
        let dense_distance = mean_distance_to(&dense, &mask, horizon);
        eprintln!(
            "fog test: {geometry_pixels} platform pixels, horizon={horizon:?}, \
             mean distance clear={clear_distance:.2}, dense={dense_distance:.2}"
        );

        assert!(
            clear_distance > 20.0,
            "the unfogged platform is too close to the sky horizon colour ({clear_distance:.2})"
        );
        assert!(
            dense_distance < clear_distance * 0.75,
            "dense fog did not pull the platform towards the horizon: \
             clear={clear_distance:.2}, dense={dense_distance:.2}"
        );
    }

    #[test]
    fn offscreen_ui_overlay_draws_over_the_world() {
        // Proves the full integration: world pass, then the alpha-blended UI
        // pass into the same colour target, then readback.
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let mut renderer = match OffscreenRenderer::new(320, 180) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping offscreen_ui_overlay_draws_over_the_world: no GPU adapter ({reason})");
                return;
            }
            Err(err) => panic!("failed to create the offscreen renderer: {err}"),
        };

        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.6, -1.0);
        camera.far = 128.0;
        renderer.render_world(&world, &camera).expect("render world without UI");
        let without_ui = renderer.read_rgba();

        let mut frame = UiFrame::new(320.0, 180.0);
        frame.rect(20.0, 20.0, 100.0, 50.0, [1.0, 0.0, 0.0, 1.0]);
        frame.text(24.0, 84.0, 2.0, [1.0, 1.0, 1.0, 1.0], "UI TEST");
        frame.block_icon(240.0, 20.0, 40.0, 0, 1);

        renderer
            .render_world_with_ui(&world, &camera, Some(&frame))
            .expect("render world with UI");
        let with_ui = renderer.read_rgba();
        assert_eq!(with_ui.len(), without_ui.len());

        // The filled rect must be opaque red in the final image.
        let index = ((45 * 320) + 70) * 4;
        let pixel = &with_ui[index..index + 4];
        assert!(
            pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60,
            "UI rect pixel is {pixel:?}, expected red"
        );
        assert_ne!(
            &with_ui[index..index + 3],
            &without_ui[index..index + 3],
            "the UI rect must replace world pixels"
        );

        // The text row must contain bright glyph pixels and differ from the
        // world-only frame.
        let mut text_pixels = 0usize;
        for y in 84..100 {
            for x in 24..120 {
                let offset = ((y * 320) + x) * 4;
                if with_ui[offset] > 220 && with_ui[offset + 1] > 220 && with_ui[offset + 2] > 220 {
                    text_pixels += 1;
                }
            }
        }
        assert!(text_pixels > 10, "text rendered only {text_pixels} bright pixels");

        // The block icon must add colours that the world-only frame did not have.
        let mut icon_colours = std::collections::HashSet::new();
        for y in 20..60 {
            for x in 240..280 {
                let offset = ((y * 320) + x) * 4;
                icon_colours.insert([with_ui[offset], with_ui[offset + 1], with_ui[offset + 2]]);
            }
        }
        assert!(
            icon_colours.len() > 8,
            "block icon produced only {} distinct colours",
            icon_colours.len()
        );
    }

    #[test]
    fn dropped_item_entities_draw_textured_and_flat_cubes() {
        let Some(mut renderer) = open_renderer(320, 180, "dropped_item_entities_draw_textured_and_flat_cubes") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        // Look down at the platform surface from the +Z side.
        let camera = look_at(WorldVec::new(8.0, 71.0, 17.0), [8.0, 66.5, 8.0]);

        renderer.render_world(&world, &camera).expect("render baseline");
        let baseline = renderer.read_rgba();
        assert_eq!(baseline.len(), (320 * 180 * 4) as usize);

        let mut planks = EntityInstance::block([6.8, 66.0, 10.0], 0.6, ob_content::textures::PLANKS.0 as u32);
        planks.yaw = 0.5;
        let entities = [
            planks,
            EntityInstance::flat([9.2, 66.0, 10.0], 0.6, [0.82, 0.12, 0.10]),
            EntityInstance::block([8.0, 66.0, 7.5], 0.5, ob_content::textures::COBBLESTONE.0 as u32),
        ];
        renderer.set_entities(&entities);
        renderer
            .render_world(&world, &camera)
            .expect("render with dropped items");
        let with_entities = renderer.read_rgba();

        let mut changed = 0usize;
        for (before, after) in baseline
            .as_chunks::<4>()
            .0
            .iter()
            .zip(with_entities.as_chunks::<4>().0.iter())
        {
            if before[0].abs_diff(after[0]) > 2 || before[1].abs_diff(after[1]) > 2 || before[2].abs_diff(after[2]) > 2
            {
                changed += 1;
            }
        }
        assert!(
            changed > 60,
            "only {changed} pixels changed when three entity cubes were added"
        );

        // The flat apple cube is strongly red at its projected centre.
        let apple = project(&camera, 320, 180, [9.2, 66.3, 10.0]).expect("apple cube is in front of the camera");
        let mut red_pixels = 0usize;
        for y in (apple[1] as i32 - 4).max(0)..=(apple[1] as i32 + 4).min(179) {
            for x in (apple[0] as i32 - 4).max(0)..=(apple[0] as i32 + 4).min(319) {
                let offset = ((y as u32 * 320 + x as u32) * 4) as usize;
                let pixel = &with_entities[offset..offset + 3];
                if pixel[0] > pixel[1] + 40 && pixel[0] > pixel[2] + 40 {
                    red_pixels += 1;
                }
            }
        }
        assert!(
            red_pixels > 3,
            "the flat apple cube produced only {red_pixels} red pixels around {apple:?}"
        );

        // The textured planks cube shows the block texture, not a flat fill.
        let planks_centre = project(&camera, 320, 180, [6.8, 66.3, 10.0]).expect("planks cube is visible");
        let mut colours = HashSet::new();
        let mut textured_changed = 0usize;
        for y in (planks_centre[1] as i32 - 5).max(0)..=(planks_centre[1] as i32 + 5).min(179) {
            for x in (planks_centre[0] as i32 - 5).max(0)..=(planks_centre[0] as i32 + 5).min(319) {
                let offset = ((y as u32 * 320 + x as u32) * 4) as usize;
                let pixel = [
                    with_entities[offset],
                    with_entities[offset + 1],
                    with_entities[offset + 2],
                ];
                colours.insert(pixel);
                if pixel[0].abs_diff(baseline[offset]) > 2
                    || pixel[1].abs_diff(baseline[offset + 1]) > 2
                    || pixel[2].abs_diff(baseline[offset + 2]) > 2
                {
                    textured_changed += 1;
                }
            }
        }
        assert!(
            colours.len() > 3,
            "the textured planks cube window has only {} colours",
            colours.len()
        );
        assert!(
            textured_changed > 5,
            "the textured planks cube replaced only {textured_changed} baseline pixels"
        );

        // Clearing the entities must restore the exact baseline frame.
        renderer.set_entities(&[]);
        renderer
            .render_world(&world, &camera)
            .expect("render without dropped items");
        let restored = renderer.read_rgba();
        assert_eq!(restored.len(), baseline.len());
        assert!(
            restored
                .iter()
                .zip(baseline.iter())
                .all(|(actual, expected)| actual.abs_diff(*expected) <= 1),
            "an empty entity list must render the same frame as the default state"
        );
    }

    // ------------------------------------------------------------------
    // Day/night environment and targeted-block outline GPU tests.
    // ------------------------------------------------------------------

    /// Opens an offscreen renderer, or returns `None` when the host has no GPU
    /// or the caller disabled GPU tests.
    fn open_renderer(width: u32, height: u32, test: &str) -> Option<OffscreenRenderer> {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping {test}: OPENBLOCK_NO_GPU_TESTS is set");
            return None;
        }
        match OffscreenRenderer::new(width, height) {
            Ok(mut renderer) => {
                // Sky/fog/outline GPU tests compare exact pixels. Clouds are
                // world-space and animate, bloom adds a highlight glow and FXAA
                // changes edge pixels, so the deterministic tests turn them off
                // and keep only the ACES/exposure/vignette post chain.
                renderer.set_graphics(crate::GraphicsSettings {
                    clouds: crate::CloudQuality::Off,
                    bloom: false,
                    fxaa: false,
                    ..crate::GraphicsSettings::default()
                });
                eprintln!("{test} GPU: {}", renderer.adapter_summary());
                Some(renderer)
            }
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping {test}: no GPU adapter ({reason})");
                None
            }
            Err(err) => panic!("failed to create the offscreen renderer: {err}"),
        }
    }

    /// Builds a camera at `eye` looking at a world-space target.
    fn look_at(eye: WorldVec, target: [f32; 3]) -> Camera {
        let delta = [
            f64::from(target[0]) - eye.x,
            f64::from(target[1]) - eye.y,
            f64::from(target[2]) - eye.z,
        ];
        let length = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        let yaw = (-delta[0]).atan2(-delta[2]) as f32;
        let pitch = (delta[1] / length).asin() as f32;
        let mut camera = Camera::new(eye, yaw, pitch);
        camera.far = 128.0;
        camera
    }

    /// Projects a world-space point to framebuffer pixels, top-left origin.
    fn project(camera: &Camera, width: u32, height: u32, point: [f32; 3]) -> Option<[f32; 2]> {
        let clip = camera.view_projection(width as f32 / height as f32) * Vec4::new(point[0], point[1], point[2], 1.0);
        if clip.w <= 0.0 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        Some([
            (ndc.x * 0.5 + 0.5) * width as f32,
            (1.0 - (ndc.y * 0.5 + 0.5)) * height as f32,
        ])
    }

    /// Expected sRGB byte of a uniform linear colour after the post chain.
    ///
    /// The sky/fog horizon is compared against the CPU environment; the render
    /// target now stores HDR and the post pass applies exposure, ACES and the
    /// vignette. `row` is the framebuffer row whose average is compared, so the
    /// vignette is averaged over the same columns the test averages.
    fn post_byte_for_uniform(channel: f32, exposure: f32, row: u32, width: u32, height: u32) -> f32 {
        let toned = crate::tonemap::aces_tonemap_channel(channel * exposure);
        let v = (row as f32 + 0.5) / height as f32;
        let mut vignette_sum = 0.0f32;
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            vignette_sum += crate::tonemap::vignette_factor(u, v);
        }
        let vignette = vignette_sum / width as f32;
        crate::tonemap::srgb_encode_channel(toned * vignette) * 255.0
    }

    fn is_outline_pixel(pixel: &[u8]) -> bool {
        // The outline is authored as linear white, so the ACES post pass writes
        // about 224 in sRGB rather than 255. Treat any near-neutral bright pixel
        // as outline; the world has no such flat white in these scenes.
        let max = pixel[0].max(pixel[1]).max(pixel[2]);
        let min = pixel[0].min(pixel[1]).min(pixel[2]);
        min >= 190 && max - min <= 24
    }

    fn count_outline_pixels(pixels: &[u8]) -> usize {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| is_outline_pixel(&pixel[..]))
            .count()
    }

    /// Whether a near-white outline pixel lies within `radius` of a point.
    fn outline_near(pixels: &[u8], width: u32, height: u32, point: [f32; 2], radius: i32) -> bool {
        let cx = point[0].round() as i32;
        let cy = point[1].round() as i32;
        for y in (cy - radius).max(0)..=(cy + radius).min(height as i32 - 1) {
            for x in (cx - radius).max(0)..=(cx + radius).min(width as i32 - 1) {
                let offset = ((y as u32 * width + x as u32) * 4) as usize;
                if is_outline_pixel(&pixels[offset..offset + 4]) {
                    return true;
                }
            }
        }
        false
    }

    /// Mean colour of the pixels selected by `mask`.
    fn mean_colour(pixels: &[u8], mask: &[bool]) -> [f32; 3] {
        let mut sum = [0f64; 3];
        let mut count = 0usize;
        for (index, selected) in mask.iter().copied().enumerate() {
            if !selected {
                continue;
            }
            let offset = index * 4;
            for channel in 0..3 {
                sum[channel] += f64::from(pixels[offset + channel]);
            }
            count += 1;
        }
        if count == 0 {
            return [0.0; 3];
        }
        [
            (sum[0] / count as f64) as f32,
            (sum[1] / count as f64) as f32,
            (sum[2] / count as f64) as f32,
        ]
    }

    /// Counts isolated bright points: pixels at least 25 luminance steps above
    /// the mean of a 7x7 ring around them. A smooth sky gradient has none; a
    /// procedural star field has many.
    fn count_star_pixels(pixels: &[u8], width: u32, height: u32) -> usize {
        let luma = |offset: usize| {
            0.2126 * f32::from(pixels[offset])
                + 0.7152 * f32::from(pixels[offset + 1])
                + 0.0722 * f32::from(pixels[offset + 2])
        };
        let mut count = 0usize;
        for y in 4..height.saturating_sub(4) {
            for x in 4..width.saturating_sub(4) {
                let offset = ((y * width + x) * 4) as usize;
                let centre = luma(offset);
                if centre < 100.0 {
                    continue;
                }
                let mut ring_sum = 0f32;
                let mut ring_count = 0f32;
                for dy in -3i32..=3 {
                    for dx in -3i32..=3 {
                        if dx.abs() < 3 && dy.abs() < 3 {
                            continue;
                        }
                        let nx = (x as i32 + dx) as u32;
                        let ny = (y as i32 + dy) as u32;
                        ring_sum += luma(((ny * width + nx) * 4) as usize);
                        ring_count += 1.0;
                    }
                }
                if centre - ring_sum / ring_count > 25.0 {
                    count += 1;
                }
            }
        }
        count
    }

    fn luma_srgb(colour: [f32; 3]) -> f32 {
        0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2]
    }

    #[test]
    fn day_night_sky_stars_and_twilight_horizon() {
        let Some(mut renderer) = open_renderer(320, 180, "day_night_sky_stars_and_twilight_horizon") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let empty = World::new(registry, 1);
        let camera = Camera::new(WorldVec::new(0.0, 128.0, 0.0), 0.0, 0.0);
        let render = |renderer: &mut OffscreenRenderer, env: Environment| {
            renderer
                .render_world_scene(&empty, &camera, &SceneParams::new(env))
                .expect("render sky-only environment");
            renderer.read_rgba()
        };

        let day = render(&mut renderer, Environment::new(0.5));
        let night = render(&mut renderer, Environment::new(0.0));
        // The star field is a pure function of the environment: the same clock
        // must produce bit-identical pixels on a second frame.
        let night_again = render(&mut renderer, Environment::new(0.0));
        assert_eq!(night, night_again, "the night sky is not deterministic");
        let dawn = render(&mut renderer, Environment::new(0.25));
        let dusk = render(&mut renderer, Environment::new(0.75));
        assert_eq!(day.len(), 320 * 180 * 4);

        let all = vec![true; 320 * 180];
        let day_mean = mean_colour(&day, &all);
        let night_mean = mean_colour(&night, &all);
        let day_top = row_rgb(&day, 320, 20);
        let night_top = row_rgb(&night, 320, 20);
        eprintln!("day mean={day_mean:?} top={day_top:?}; night mean={night_mean:?} top={night_top:?}");
        assert!(
            luma_srgb(day_mean) > luma_srgb(night_mean) * 2.0,
            "the night sky is not darker than the day sky: {} vs {}",
            luma_srgb(day_mean),
            luma_srgb(night_mean)
        );
        assert!(
            colour_distance(day_mean, night_mean) > 40.0,
            "day and night sky colours are too similar: {day_mean:?} vs {night_mean:?}"
        );
        assert!(
            luma_srgb(day_top) > luma_srgb(night_top) * 3.0,
            "the zenith is not much darker at night: {day_top:?} vs {night_top:?}"
        );

        // Dawn and dusk are warm above the horizon; noon is a cool blue.
        let noon_horizon = row_rgb(&day, 320, 90);
        for (name, pixels) in [("dawn", &dawn), ("dusk", &dusk)] {
            let horizon = row_rgb(pixels, 320, 90);
            eprintln!("{name} horizon={horizon:?}, noon horizon={noon_horizon:?}");
            assert!(
                horizon[0] - horizon[2] > noon_horizon[0] - noon_horizon[2] + 30.0,
                "{name} horizon is not redder than noon: {horizon:?} vs {noon_horizon:?}"
            );
            assert!(
                horizon[0] > horizon[2] + 20.0,
                "{name} horizon is not warm: {horizon:?}"
            );
            assert!(
                horizon[0] / horizon[1] > noon_horizon[0] / noon_horizon[1],
                "{name} horizon has the wrong warmth ratio: {horizon:?} vs {noon_horizon:?}"
            );
        }

        let day_stars = count_star_pixels(&day, 320, 180);
        let night_stars = count_star_pixels(&night, 320, 180);
        eprintln!("isolated star pixels: day={day_stars}, night={night_stars}");
        assert!(
            night_stars > 20,
            "the night sky only has {night_stars} isolated bright star pixels"
        );
        assert!(
            day_stars < 3,
            "the noon sky has {day_stars} star-like pixels; stars must be off by day"
        );
    }

    #[test]
    fn world_geometry_is_darker_at_night() {
        let Some(mut renderer) = open_renderer(320, 180, "world_geometry_is_darker_at_night") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let empty = World::new(registry, 1);
        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.6, -1.0);
        camera.far = 128.0;

        let render = |renderer: &mut OffscreenRenderer, world: &World, env: Environment| {
            renderer
                .render_world_scene(world, &camera, &SceneParams::new(env))
                .expect("render environment world");
            renderer.read_rgba()
        };

        renderer
            .render_world_scene(&empty, &camera, &SceneParams::new(Environment::default()))
            .expect("render empty background");
        let background = renderer.read_rgba();
        let day = render(&mut renderer, &world, Environment::new(0.5));
        let night = render(&mut renderer, &world, Environment::new(0.0));

        // The geometry mask is taken from the daylight frame: perspective
        // coverage does not depend on the environment, only the colours do.
        let mut mask = vec![false; 320 * 180];
        for (index, pixel) in day.as_chunks::<4>().0.iter().enumerate() {
            let offset = index * 4;
            if pixel[0].abs_diff(background[offset]) > 2
                || pixel[1].abs_diff(background[offset + 1]) > 2
                || pixel[2].abs_diff(background[offset + 2]) > 2
            {
                mask[index] = true;
            }
        }
        let geometry_pixels = mask.iter().filter(|selected| **selected).count();
        assert!(geometry_pixels > 1000, "only {geometry_pixels} geometry pixels");

        let day_mean = mean_colour(&day, &mask);
        let night_mean = mean_colour(&night, &mask);
        eprintln!(
            "world mean geometry: day={day_mean:?} luma={}, night={night_mean:?} luma={}",
            luma_srgb(day_mean),
            luma_srgb(night_mean)
        );
        assert!(
            luma_srgb(night_mean) < luma_srgb(day_mean) * 0.7,
            "night geometry is not darker: {} vs {}",
            luma_srgb(night_mean),
            luma_srgb(day_mean)
        );
        // Moonlight keeps the world readable rather than black.
        assert!(
            luma_srgb(night_mean) > 6.0,
            "night geometry is almost black: {night_mean:?}"
        );
    }

    #[test]
    fn fog_colour_follows_the_horizon_at_every_time_of_day() {
        let Some(mut renderer) = open_renderer(320, 180, "fog_colour_follows_the_horizon_at_every_time_of_day") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let stone = registry.id_or_air("openblock:stone");
        assert_ne!(stone, BlockId::AIR);

        // A tall wall at 43 m, just inside a 44 m far plane but beyond the fog
        // end (41.8 m): every wall pixel must be exactly the horizon colour.
        let mut column = ChunkColumn::new(ChunkPos::new(0, -1));
        for y in 40..120 {
            for x in 0..SECTION_SIZE as i32 {
                for z in -4..=-3 {
                    column.set_block(BlockPos::new(x, y, z), stone);
                }
            }
        }
        column.recompute_heightmap();
        let mut wall_world = World::new(Arc::clone(&registry), 1);
        wall_world.insert_chunk(column);
        let empty = World::new(registry, 1);

        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 40.0), 0.0, 0.0);
        camera.far = 44.0;
        let (fog_start, fog_end) = camera.fog_range();
        assert!(
            fog_end < 43.0,
            "the fog wall must be fully fogged: {fog_start}..{fog_end}"
        );

        for env in [Environment::new(0.5), Environment::new(0.75), Environment::new(0.0)] {
            renderer
                .render_world_scene(&empty, &camera, &SceneParams::new(env))
                .expect("render empty sky");
            let background = renderer.read_rgba();
            let horizon = row_rgb(&background, 320, 90);
            let expected = env.visual().horizon;
            let exposure = renderer.graphics().exposure;
            let expected = [
                post_byte_for_uniform(expected[0], exposure, 90, 320, 180),
                post_byte_for_uniform(expected[1], exposure, 90, 320, 180),
                post_byte_for_uniform(expected[2], exposure, 90, 320, 180),
            ];
            for channel in 0..3 {
                assert!(
                    (horizon[channel] - expected[channel]).abs() <= 4.0,
                    "time {}: horizon row {horizon:?} does not match the post-processed CPU horizon {expected:?}",
                    env.time_of_day
                );
            }

            renderer
                .render_world_scene(&wall_world, &camera, &SceneParams::new(env))
                .expect("render fogged wall");
            let wall = renderer.read_rgba();
            let mut mask = vec![false; 320 * 180];
            for (index, pixel) in wall.as_chunks::<4>().0.iter().enumerate() {
                let offset = index * 4;
                if pixel[0].abs_diff(background[offset]) > 2
                    || pixel[1].abs_diff(background[offset + 1]) > 2
                    || pixel[2].abs_diff(background[offset + 2]) > 2
                {
                    mask[index] = true;
                }
            }
            let wall_pixels = mask.iter().filter(|selected| **selected).count();
            assert!(
                wall_pixels > 1500,
                "time {}: only {wall_pixels} fogged wall pixels found",
                env.time_of_day
            );

            let distance = mean_distance_to(&wall, &mask, horizon);
            eprintln!(
                "time {}: horizon={horizon:?}, {wall_pixels} fully fogged pixels, mean distance={distance:.3}",
                env.time_of_day
            );
            assert!(
                distance < 5.0,
                "time {}: fogged geometry does not match the horizon ({distance:.3})",
                env.time_of_day
            );
        }
    }

    /// Projects the midpoint of each of the four top edges of an outline box.
    fn projected_top_edge_midpoints(outline: &BlockOutline, camera: &Camera, width: u32, height: u32) -> [[f32; 2]; 4] {
        let [x0, _, z0] = outline.min;
        let [x1, y1, z1] = outline.max;
        let corners = [[x0, y1, z0], [x1, y1, z0], [x1, y1, z1], [x0, y1, z1]];
        let mut points = [[0f32; 2]; 4];
        for edge in 0..4 {
            let a = corners[edge];
            let b = corners[(edge + 1) % 4];
            let midpoint = [(a[0] + b[0]) * 0.5, y1, (a[2] + b[2]) * 0.5];
            points[edge] = project(camera, width, height, midpoint).expect("an outline edge is behind the camera");
        }
        points
    }

    #[test]
    fn block_outline_is_drawn_at_the_projected_edges() {
        let Some(mut renderer) = open_renderer(480, 270, "block_outline_is_drawn_at_the_projected_edges") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let mut world = test_world(&registry);
        // Raise the selected block one metre above the flat field so its top
        // square is fully visible instead of being foreshortened against the
        // surrounding grass.
        let grass = registry.id_or_air("openblock:grass");
        assert_ne!(grass, BlockId::AIR);
        assert!(world.set_block(BlockPos::new(8, 66, 8), grass));

        let camera = look_at(WorldVec::new(8.5, 70.5, 16.0), [8.5, 66.8, 8.5]);
        let outline = BlockOutline::for_block([8, 66, 8]);

        let without = {
            renderer
                .render_world_scene(&world, &camera, &SceneParams::new(Environment::default()))
                .expect("render world without outline");
            renderer.read_rgba()
        };
        let with = {
            renderer
                .render_world_scene(
                    &world,
                    &camera,
                    &SceneParams::new(Environment::default()).with_outline(outline),
                )
                .expect("render world with outline");
            renderer.read_rgba()
        };

        let without_outline = count_outline_pixels(&without);
        let with_outline = count_outline_pixels(&with);
        eprintln!("outline pixels: without={without_outline}, with={with_outline}");
        assert!(
            with_outline > 60,
            "only {with_outline} outline-coloured pixels were drawn"
        );
        assert!(
            with_outline > without_outline + 60,
            "the outline did not add enough pixels: {without_outline} -> {with_outline}"
        );

        // Every top edge midpoint must have outline pixels within two pixels.
        let midpoints = projected_top_edge_midpoints(&outline, &camera, 480, 270);
        let mut found = 0;
        for point in midpoints {
            if outline_near(&with, 480, 270, point, 2) {
                found += 1;
            }
        }
        eprintln!("projected top edge midpoints: {midpoints:?}, found {found}/4");
        assert_eq!(found, 4, "only {found}/4 projected top edges have outline pixels");
    }

    #[test]
    fn block_outline_respects_depth_against_a_nearer_pillar() {
        let Some(mut renderer) = open_renderer(480, 270, "block_outline_respects_depth_against_a_nearer_pillar") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let stone = registry.id_or_air("openblock:stone");
        let grass = registry.id_or_air("openblock:grass");
        assert_ne!(stone, BlockId::AIR);
        assert_ne!(grass, BlockId::AIR);
        let mut world = test_world(&registry);
        assert!(world.set_block(BlockPos::new(8, 66, 8), grass));

        let camera = look_at(WorldVec::new(8.5, 72.0, 24.0), [8.5, 66.8, 8.5]);
        let outline = BlockOutline::for_block([8, 66, 8]);
        let scene = SceneParams::new(Environment::default()).with_outline(outline);

        // Baseline: with no pillar the whole outline is visible.
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render visible outline");
        let baseline = count_outline_pixels(&renderer.read_rgba());
        assert!(baseline > 30, "the unoccluded outline only has {baseline} pixels");

        // A solid pillar between the camera and the selected block. It is much
        // nearer, so every outline edge is behind it.
        for y in 66..76 {
            assert!(world.set_block(BlockPos::new(8, y, 14), stone));
        }
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render occluded outline");
        let pixels = renderer.read_rgba();
        let visible = count_outline_pixels(&pixels);
        eprintln!("outline pixels: unoccluded={baseline}, behind a nearer pillar={visible}");
        assert!(
            visible < 5,
            "the outline painted {visible} pixels over a much nearer opaque pillar"
        );
    }

    /// Writes one RGBA8 image to disk.
    fn write_test_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create screenshot directory");
        }
        let file = std::fs::File::create(path).expect("create screenshot png");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write screenshot png header");
        writer.write_image_data(rgba).expect("write screenshot png pixels");
    }

    /// Ignored GPU verification: noon, sunset and midnight skies over the test
    /// platform, plus the targeted-block outline.
    ///
    /// Run with:
    /// `./dev-cargo test -p ob_render --lib -- --ignored write_environment_screenshots`
    #[test]
    #[ignore = "GPU screenshot verification; writes /tmp/ob-env-*.png and /tmp/ob-outline.png"]
    fn write_environment_screenshots() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping write_environment_screenshots: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 360;
        let registry = Arc::new(Registry::base());
        let mut world = test_world(&registry);
        let grass = registry.id_or_air("openblock:grass");
        assert_ne!(grass, BlockId::AIR);
        // The outlined block sits one metre proud of the field so the screenshot
        // shows the whole wireframe.
        assert!(world.set_block(BlockPos::new(8, 66, 8), grass));
        let mut renderer = OffscreenRenderer::new(WIDTH, HEIGHT).expect("create screenshot renderer");
        eprintln!("environment screenshots GPU: {}", renderer.adapter_summary());

        // Looking west towards the sunset over the edge of the test platform.
        let horizon_camera = Camera::new(WorldVec::new(8.5, 72.0, 8.5), std::f32::consts::FRAC_PI_2, -0.08);
        // Looking up at the moon and the star field.
        let night_sky_camera = Camera::new(WorldVec::new(8.5, 72.0, 8.5), std::f32::consts::FRAC_PI_2, 1.0);
        let outline_camera = look_at(WorldVec::new(8.5, 70.5, 16.0), [8.5, 66.8, 8.5]);

        let shots: [(&str, &Camera, Environment, Option<BlockOutline>); 5] = [
            ("/tmp/ob-env-day.png", &horizon_camera, Environment::new(0.5), None),
            ("/tmp/ob-env-dusk.png", &horizon_camera, Environment::new(0.75), None),
            ("/tmp/ob-env-night.png", &horizon_camera, Environment::new(0.0), None),
            (
                "/tmp/ob-env-night-sky.png",
                &night_sky_camera,
                Environment::new(0.0),
                None,
            ),
            (
                "/tmp/ob-outline.png",
                &outline_camera,
                Environment::default(),
                Some(BlockOutline::for_block([8, 66, 8])),
            ),
        ];

        for (path, camera, environment, outline) in shots {
            let mut scene = SceneParams::new(environment);
            if let Some(outline) = outline {
                scene.outline = Some(outline);
            }
            renderer
                .render_world_scene(&world, camera, &scene)
                .expect("render environment screenshot");
            let pixels = renderer.read_rgba();
            assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);
            let path = std::path::Path::new(path);
            write_test_png(path, WIDTH, HEIGHT, &pixels);
            eprintln!(
                "wrote {} (time {:.2}, outline {})",
                path.display(),
                environment.time_of_day,
                outline.is_some()
            );
        }
    }

    /// A small lake with a sandy shore, stone terraces and two pillars, used for
    /// the transparent-water screenshot. Deterministic and one chunk only.
    fn screenshot_water_world(registry: &Arc<Registry>) -> World {
        let stone = registry.id_or_air("openblock:stone");
        let sand = registry.id_or_air("openblock:sand");
        let grass = registry.id_or_air("openblock:grass");
        let water = registry.id_or_air("openblock:water");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..SECTION_SIZE as i32 {
            for x in 0..SECTION_SIZE as i32 {
                column.set_block(BlockPos::new(x, 60, z), stone);
                // A shallow bowl: water in the middle, a sand/grass shore, then a
                // low stone rim that catches the late-morning shadows.
                let dx = (x as f32 - 7.5).abs();
                let dz = (z as f32 - 7.5).abs();
                let centre = dx.max(dz);
                if centre <= 4.5 {
                    // Water top surface at y = 65, one block below the rim.
                    for y in 61..=64 {
                        column.set_block(BlockPos::new(x, y, z), water);
                    }
                } else if centre <= 6.5 {
                    for y in 61..=65 {
                        column.set_block(BlockPos::new(x, y, z), if y == 65 { sand } else { stone });
                    }
                } else {
                    for y in 61..=65 {
                        column.set_block(BlockPos::new(x, y, z), if y == 65 { grass } else { stone });
                    }
                }
            }
        }
        // One pillar rises out of the water and casts a shadow across the
        // surface; the other stands on the shore for depth.
        for y in 65..73 {
            column.set_block(BlockPos::new(8, y, 8), stone);
        }
        for y in 63..69 {
            column.set_block(BlockPos::new(13, y, 12), stone);
        }
        column.recompute_heightmap();
        let mut world = World::new(Arc::clone(registry), 11);
        world.insert_chunk(column);
        world
    }

    #[test]
    fn water_receives_sun_shadows() {
        let Some(mut renderer) = open_renderer(640, 360, "water_receives_sun_shadows") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = screenshot_water_world(&registry);
        let mut camera = Camera::new(WorldVec::new(8.0, 70.5, 22.0), 0.0, -0.34);
        camera.far = 96.0;
        let scene = SceneParams {
            environment: Environment::new(0.40),
            ui: None,
            outline: None,
            time_seconds: 8.0,
        };
        let mut graphics = renderer.graphics();
        graphics.shadow_quality = crate::ShadowQuality::High;
        renderer.set_graphics(graphics);
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render shadowed water");
        let shadowed = renderer.read_rgba();
        assert!(renderer.stats().water_draw_calls > 0);
        let mut graphics = renderer.graphics();
        graphics.shadow_quality = crate::ShadowQuality::Off;
        renderer.set_graphics(graphics);
        renderer
            .render_cached_scene(&camera, &scene)
            .expect("render unshadowed water");
        let plain = renderer.read_rgba();

        // Count only pixels that are visibly blue in the shadowed frame, i.e.
        // water (or its tinted rim), so opaque terrain shadows cannot satisfy the
        // test on their own.
        let mut water_pixels = 0usize;
        let mut changed = 0usize;
        for (shadowed, plain) in shadowed.as_chunks::<4>().0.iter().zip(plain.as_chunks::<4>().0) {
            let blue = i32::from(shadowed[2]) - i32::from(shadowed[0]);
            let water = blue > 25 && shadowed[2] > shadowed[1];
            if !water {
                continue;
            }
            water_pixels += 1;
            if shadowed[0].abs_diff(plain[0]) > 2
                || shadowed[1].abs_diff(plain[1]) > 2
                || shadowed[2].abs_diff(plain[2]) > 2
            {
                changed += 1;
            }
        }
        assert!(water_pixels > 500, "only {water_pixels} water pixels were found");
        assert!(
            changed > 100,
            "only {changed} of {water_pixels} water pixels changed when shadows were disabled"
        );
    }

    #[test]
    #[ignore = "GPU screenshot verification; writes /tmp/openblock-shaders-water.png"]
    fn write_shader_water_screenshot() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping water screenshot: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = screenshot_water_world(&registry);
        let mut renderer = match OffscreenRenderer::new_with_source_and_graphics(
            1280,
            720,
            crate::TextureSource::Style(crate::TexturePackStyle::Classic),
            crate::GraphicsSettings::from_preset(crate::GraphicsPreset::High),
        ) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping water screenshot: no GPU adapter ({reason})");
                return;
            }
            Err(error) => panic!("failed to create the offscreen renderer: {error}"),
        };
        let mut camera = Camera::new(WorldVec::new(8.0, 70.5, 22.0), 0.0, -0.34);
        camera.far = 96.0;
        let scene = SceneParams {
            environment: Environment::new(0.40),
            ui: None,
            outline: None,
            time_seconds: 8.0,
        };
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render water screenshot");
        assert!(renderer.stats().water_draw_calls > 0);
        let rgba = renderer.read_rgba();
        let mut graphics = renderer.graphics();
        graphics.shadow_quality = crate::ShadowQuality::Off;
        renderer.set_graphics(graphics);
        renderer
            .render_cached_scene(&camera, &scene)
            .expect("render water without shadows");
        let no_shadow = renderer.read_rgba();
        let differing = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(no_shadow.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 2 || a[1].abs_diff(b[1]) > 2 || a[2].abs_diff(b[2]) > 2)
            .count();
        eprintln!("water screenshot shadow difference: {differing} pixels");
        let mut graphics = renderer.graphics();
        graphics.shadow_quality = crate::ShadowQuality::High;
        renderer.set_graphics(graphics);
        renderer
            .render_cached_scene(&camera, &scene)
            .expect("render water with shadows again");
        write_test_png(
            &std::env::temp_dir().join("openblock-shaders-water.png"),
            1280,
            720,
            &rgba,
        );
        eprintln!(
            "water screenshot: {:?}, {} water draws",
            renderer.stats(),
            renderer.stats().water_draw_calls
        );
    }

    /// Scratch directory for a synthetic resource pack, removed on drop.
    struct TempPack {
        path: PathBuf,
    }

    impl TempPack {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "ob-offscreen-pack-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                unique
            ));
            std::fs::create_dir_all(&path).expect("create the synthetic pack");
            Self { path }
        }
    }

    impl Drop for TempPack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Writes one solid RGBA8 block texture into a synthetic pack.
    fn write_pack_texture(block_dir: &Path, name: &str, size: u32, colour: [u8; 4]) {
        let mut pixels = vec![0u8; (size * size * 4) as usize];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&colour);
        }
        let file = std::fs::File::create(block_dir.join(name)).expect("create a pack texture");
        let mut encoder = png::Encoder::new(io::BufWriter::new(file), size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write a pack texture header");
        writer.write_image_data(&pixels).expect("write pack texture pixels");
    }

    /// Builds a tiny 8x8 pack with only the textures the test world needs.
    fn synthetic_resource_pack() -> TempPack {
        let pack = TempPack::new();
        let block_dir = pack.path.join("assets/minecraft/textures/block");
        std::fs::create_dir_all(&block_dir).expect("create the synthetic block directory");
        write_pack_texture(&block_dir, "stone.png", 8, [120, 120, 124, 255]);
        write_pack_texture(&block_dir, "grass_block_top.png", 8, [70, 150, 60, 255]);
        write_pack_texture(&block_dir, "grass_block_side.png", 8, [100, 80, 50, 255]);
        write_pack_texture(&block_dir, "dirt.png", 8, [110, 85, 55, 255]);
        write_pack_texture(&block_dir, "oak_leaves.png", 8, [40, 130, 45, 255]);
        pack
    }

    #[test]
    fn offscreen_ui_font_swap_draws_ui_and_title_text() {
        let Some(mut renderer) = open_renderer(320, 180, "offscreen_ui_font_swap_draws_ui_and_title_text") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let empty = World::new(registry, 1);
        let camera = Camera::new(WorldVec::new(0.0, 128.0, 0.0), 0.0, 0.0);
        assert_eq!(renderer.ui_font_name(), "Monocraft");

        renderer.render_world(&empty, &camera).expect("render baseline sky");
        let without_ui = renderer.read_rgba();

        let mut frame = UiFrame::new(320.0, 180.0);
        frame.rect(8.0, 8.0, 304.0, 40.0, [0.05, 0.05, 0.1, 1.0]);
        frame.text(12.0, 12.0, 2.0, [1.0; 4], "Monocraft UI 0123");
        frame.title_text(12.0, 56.0, 2.0, [0.9, 0.8, 0.2, 1.0], "TITLE 8x8");
        renderer
            .render_world_with_ui(&empty, &camera, Some(&frame))
            .expect("render UI and title text");
        let with_ui = renderer.read_rgba();
        assert_eq!(with_ui.len(), without_ui.len());

        let mut changed = 0usize;
        for (before, after) in without_ui
            .as_chunks::<4>()
            .0
            .iter()
            .zip(with_ui.as_chunks::<4>().0.iter())
        {
            if before[0].abs_diff(after[0]) > 2 || before[1].abs_diff(after[1]) > 2 || before[2].abs_diff(after[2]) > 2
            {
                changed += 1;
            }
        }
        assert!(changed > 100, "UI frame changed only {changed} pixels");

        let count_bright = |pixels: &[u8], x0: u32, x1: u32, y0: u32, y1: u32| {
            let mut count = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let offset = ((y * 320 + x) * 4) as usize;
                    if pixels[offset] > 200 && pixels[offset + 1] > 200 && pixels[offset + 2] > 200 {
                        count += 1;
                    }
                }
            }
            count
        };
        // The title is drawn in warm yellow, so count that instead of white.
        let count_yellow = |pixels: &[u8], x0: u32, x1: u32, y0: u32, y1: u32| {
            let mut count = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let offset = ((y * 320 + x) * 4) as usize;
                    if pixels[offset] > 170 && pixels[offset + 1] > 130 && pixels[offset + 2] < 130 {
                        count += 1;
                    }
                }
            }
            count
        };
        let ui_pixels = count_bright(&with_ui, 12, 300, 12, 28);
        let title_pixels = count_yellow(&with_ui, 12, 120, 56, 72);
        eprintln!("offscreen UI font: {ui_pixels} UI-font pixels, {title_pixels} bitmap-title pixels");
        assert!(ui_pixels > 40, "UI font drew only {ui_pixels} bright pixels");
        assert!(
            title_pixels > 10,
            "bitmap title font drew only {title_pixels} bright pixels"
        );

        // Invalid overrides are rejected without changing the active atlas.
        let error = renderer
            .set_ui_font(Some(b"definitely not a TTF"))
            .expect_err("invalid UI font must be rejected");
        assert!(
            matches!(error, crate::UiFontError::InvalidFont(_)),
            "unexpected error: {error}"
        );
        assert_eq!(renderer.ui_font_name(), "Monocraft");
        renderer
            .render_world_with_ui(&empty, &camera, Some(&frame))
            .expect("render after rejected UI font");
        let after_invalid = renderer.read_rgba();
        assert!(
            after_invalid
                .iter()
                .zip(with_ui.iter())
                .all(|(actual, expected)| actual.abs_diff(*expected) <= 1),
            "a rejected UI font must not change the rendered frame"
        );

        // Reloading the embedded bytes and `None` both restore the default.
        renderer
            .set_ui_font(Some(crate::default_ui_font()))
            .expect("default font bytes are valid");
        assert_eq!(renderer.ui_font_name(), "Monocraft");
        renderer
            .render_world_with_ui(&empty, &camera, Some(&frame))
            .expect("render after reloading the default UI font");
        let after_default = renderer.read_rgba();
        assert!(
            after_default
                .iter()
                .zip(with_ui.iter())
                .all(|(actual, expected)| actual.abs_diff(*expected) <= 1),
            "reloading the default font must render the same frame"
        );
        renderer
            .set_ui_font(None)
            .expect("the embedded default is always available");
        assert_eq!(renderer.ui_font_name(), "Monocraft");
    }

    /// Ignored GPU verification: writes a UI-font sample with both the
    /// Monocraft UI text and the bitmap title text.
    ///
    /// Run with:
    /// `./dev-cargo test -p ob_render --lib -- --ignored write_ui_font_sample`
    #[test]
    #[ignore = "GPU screenshot verification; writes /tmp/ui-font-sample.png"]
    fn write_ui_font_sample() {
        let Some(mut renderer) = open_renderer(640, 360, "write_ui_font_sample") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let camera = look_at(WorldVec::new(8.5, 72.0, 16.0), [8.5, 66.8, 8.5]);

        let mut frame = UiFrame::new(640.0, 360.0);
        frame.rect(16.0, 16.0, 608.0, 328.0, [0.04, 0.05, 0.08, 0.94]);
        frame.title_text(28.0, 28.0, 4.0, [1.0, 0.85, 0.3, 1.0], "OPENBLOCK");
        frame.title_text(28.0, 72.0, 1.0, [0.7, 0.8, 0.95, 1.0], "BITMAP TITLE FONT 8x8");
        frame.text(
            28.0,
            96.0,
            1.0,
            [1.0; 4],
            "Monocraft UI font 8 px: The quick brown fox jumps over the lazy dog.",
        );
        frame.text(
            28.0,
            112.0,
            1.0,
            [1.0; 4],
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ 0123456789 !@#$%^&*()_+-=[]{};:'\",.<>/?\\|",
        );
        frame.text(
            28.0,
            128.0,
            1.0,
            [0.6, 0.9, 0.6, 1.0],
            "abcdefghijklmnopqrstuvwxyz gpqyj descenders",
        );
        frame.text(28.0, 152.0, 2.0, [1.0; 4], "Scale 2 UI text: OpenBlock 1.0.1");
        frame.title_text(28.0, 180.0, 2.0, [1.0, 0.85, 0.3, 1.0], "Scale 2 title: OpenBlock");
        frame.text(28.0, 204.0, 3.0, [0.85, 0.9, 1.0, 1.0], "Scale 3 UI");
        frame.title_text(28.0, 240.0, 3.0, [1.0, 0.85, 0.3, 1.0], "SCALE 3 TITLE");
        frame.text(
            28.0,
            272.0,
            1.0,
            [0.8, 0.8, 0.8, 1.0],
            "UI font name shown in F3: Monocraft / custom",
        );

        renderer
            .render_world_with_ui(&world, &camera, Some(&frame))
            .expect("render UI font sample");
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), (640 * 360 * 4) as usize);
        write_test_png(&std::env::temp_dir().join("ui-font-sample.png"), 640, 360, &pixels);
        eprintln!("wrote /tmp/ui-font-sample.png");
    }

    #[test]
    fn offscreen_renderer_loads_a_resource_pack_source() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen_renderer_loads_a_resource_pack_source: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let pack = synthetic_resource_pack();
        let source = crate::TextureSource::ResourcePack(pack.path.clone());
        let mut renderer = match OffscreenRenderer::new_with_source(96, 54, source.clone()) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping offscreen_renderer_loads_a_resource_pack_source: no GPU adapter ({reason})");
                return;
            }
            Err(error) => panic!("failed to create the resource-pack offscreen renderer: {error}"),
        };
        eprintln!("resource-pack offscreen GPU: {}", renderer.adapter_summary());
        assert_eq!(renderer.texture_source(), &source);
        assert_eq!(renderer.texture_style(), crate::TexturePackStyle::Classic);

        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.6, -1.0);
        camera.far = 128.0;
        renderer
            .render_world(&world, &camera)
            .expect("render the test world with a resource pack");
        assert!(renderer.stats().sections > 0, "the test platform must be visible");
        assert_eq!(renderer.read_rgba().len(), 96 * 54 * 4);

        // A failed load must be reported, not panic, and must leave the current
        // bind group and UI usable.
        let missing = crate::TextureSource::ResourcePack(pack.path.join("does-not-exist"));
        let error = renderer
            .set_texture_source(missing)
            .expect_err("a missing pack path must fail");
        assert!(matches!(error, RenderError::Texture(_)), "unexpected error: {error}");
        assert_eq!(
            renderer.texture_source(),
            &source,
            "a failed load must keep the previous textures"
        );
        renderer
            .render_world(&world, &camera)
            .expect("render after a failed resource-pack load");

        renderer
            .set_texture_source(crate::TextureSource::Style(crate::TexturePackStyle::Classic))
            .expect("procedural styles always load");
        assert_eq!(
            renderer.texture_source(),
            &crate::TextureSource::Style(crate::TexturePackStyle::Classic)
        );
        renderer.set_texture_style(crate::TexturePackStyle::Minimal);
        assert_eq!(renderer.texture_style(), crate::TexturePackStyle::Minimal);
    }

    #[test]
    fn cached_mesh_beyond_the_far_plane_is_culled() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let mut renderer = match OffscreenRenderer::new(96, 54) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping cached_mesh_beyond_the_far_plane_is_culled: {reason}");
                return;
            }
            Err(error) => panic!("failed to create the offscreen renderer: {error}"),
        };
        let mesh = mesh::mesh_section(&world, SectionPos::new(0, 4, 0));
        assert!(!mesh.is_empty(), "the test platform must produce geometry");
        // Upload under a key far outside the 128-block far plane while the
        // camera looks along -Z. The vertices themselves are at the origin, so
        // any draw would be visible if distance culling failed.
        renderer.upload_mesh(SectionPos::new(0, 4, -30), mesh);
        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.0, 0.0);
        camera.far = 128.0;
        let stats = renderer.render_cached(&camera).expect("cached frame");
        assert_eq!(stats.sections, 0, "a section beyond the far plane must not draw");
        assert_eq!(stats.draw_calls, 0);
        assert!(stats.culled_sections >= 1);
    }

    #[test]
    fn far_lod_batches_carry_and_draw_water() {
        let Some(mut renderer) = open_renderer(320, 180, "far_lod_batches_carry_and_draw_water") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = water_world(&registry);
        let camera = Camera::new(WorldVec::new(8.0, 72.0, 20.0), 0.0, -0.55);
        let config = crate::LodConfig {
            full_detail_radius_chunks: 0,
            group_chunks: 2,
            shell_depth_blocks: 48,
        };
        let cache = renderer.rebuild_lod_scene(&world, &camera, &config);
        assert_eq!(cache.sections, 0, "full radius 0 must not mesh sections");
        assert!(cache.batches > 0, "the water column must become a far batch");
        assert!(cache.batch_vertices > 0);
        let stats = renderer.render_cached(&camera).expect("far water frame");
        assert_eq!(stats.sections, 0);
        assert!(stats.lod_batches > 0);
        assert!(stats.water_draw_calls > 0, "the far batch water buffer was not drawn");
        assert!(stats.water_vertices > 0);
        let pixels = renderer.read_rgba();
        assert!(pixels.iter().any(|value| *value != 0));
    }

    #[test]
    fn lod_scene_meshes_far_groups_and_draws_them() {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping offscreen GPU tests: OPENBLOCK_NO_GPU_TESTS is set");
            return;
        }
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let mut renderer = match OffscreenRenderer::new(96, 54) {
            Ok(renderer) => renderer,
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping lod_scene_meshes_far_groups_and_draws_them: {reason}");
                return;
            }
            Err(error) => panic!("failed to create the offscreen renderer: {error}"),
        };
        let mut camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.0, 0.0);
        camera.far = 128.0;
        let config = crate::LodConfig {
            full_detail_radius_chunks: 0,
            group_chunks: 2,
            shell_depth_blocks: 48,
        };
        let cache = renderer.rebuild_lod_scene(&world, &camera, &config);
        assert_eq!(cache.sections, 0, "full radius 0 must not mesh sections");
        assert!(cache.batches > 0, "the test group must become a far batch");
        assert!(cache.batch_vertices > 0);
        assert!(cache.batch_sections > 0);
        let stats = renderer.render_cached(&camera).expect("cached LOD frame");
        assert!(stats.lod_batches > 0, "the far batch must be inside the frustum");
        assert_eq!(stats.sections, 0);
        assert_eq!(stats.vertices, stats.lod_vertices);
        assert!(renderer.read_rgba().iter().any(|value| *value != 0));
    }
    /// A small stone basin filled with water for the transparent-water tests.
    fn water_world(registry: &Arc<Registry>) -> World {
        let stone = registry.id_or_air("openblock:stone");
        let water = registry.id_or_air("openblock:water");
        assert_ne!(water, BlockId::AIR);
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for z in 0..SECTION_SIZE as i32 {
            for x in 0..SECTION_SIZE as i32 {
                column.set_block(BlockPos::new(x, 64, z), stone);
                column.set_block(BlockPos::new(x, 65, z), water);
            }
        }
        column.recompute_heightmap();
        let mut world = World::new(Arc::clone(registry), 2);
        world.insert_chunk(column);
        world
    }

    fn mean_luma(pixels: &[u8]) -> f32 {
        let mut sum = 0f64;
        for pixel in pixels.as_chunks::<4>().0 {
            sum += 0.2126 * f64::from(pixel[0]) + 0.7152 * f64::from(pixel[1]) + 0.0722 * f64::from(pixel[2]);
        }
        (sum / (pixels.len() / 4) as f64) as f32
    }

    #[test]
    fn sun_shadows_darken_shadowed_terrain_and_report_cascades() {
        let Some(mut renderer) = open_renderer(480, 270, "sun_shadows_darken_shadowed_terrain_and_report_cascades")
        else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        // Late-morning sun: clearly slanted, so the pillars cast visible
        // shadows across the platform the camera looks down on.
        let environment = Environment::new(0.40);
        let scene = SceneParams::new(environment);
        let mut camera = Camera::new(WorldVec::new(5.0, 78.0, 8.0), 0.6, -0.85);
        camera.far = 128.0;

        let render = |renderer: &mut OffscreenRenderer, quality: crate::ShadowQuality| {
            let mut graphics = renderer.graphics();
            graphics.shadow_quality = quality;
            renderer.set_graphics(graphics);
            renderer
                .render_world_scene(&world, &camera, &scene)
                .expect("render shadowed world");
            (renderer.read_rgba(), renderer.stats())
        };

        let (lit, lit_stats) = render(&mut renderer, crate::ShadowQuality::High);
        // The cascade must actually contain rasterised casters; a map that is all
        // 1.0 would make the pixel comparison below fail for the wrong reason.
        let (min_depth, max_depth, written) = renderer
            .debug_shadow_depth()
            .expect("shadow map readback for the first cascade");
        assert!(
            written > 1000,
            "shadow map has no casters (min {min_depth}, max {max_depth}, written {written})"
        );
        let (flat, flat_stats) = render(&mut renderer, crate::ShadowQuality::Off);
        assert_eq!(lit_stats.shadow_cascades, 2);
        assert_eq!(lit_stats.shadow_resolution, 2048);
        assert!(lit_stats.shadow_draw_calls > 0, "the shadow pass drew no casters");
        assert_eq!(flat_stats.shadow_cascades, 0);
        assert_eq!(flat_stats.shadow_resolution, 0);

        let darker = lit
            .as_chunks::<4>()
            .0
            .iter()
            .zip(flat.as_chunks::<4>().0)
            .filter(|(shadowed, plain)| {
                let luma = |pixel: [u8; 4]| {
                    0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2])
                };
                luma(**shadowed) + 4.0 < luma(**plain)
            })
            .count();
        assert!(darker > 200, "only {darker} pixels were darkened by shadows");
        // The same quality and environment must produce identical pixels.
        let (lit_again, _) = render(&mut renderer, crate::ShadowQuality::High);
        assert_eq!(lit, lit_again, "shadow rendering is not deterministic");
    }

    #[test]
    fn water_uses_a_separate_pass_and_animates() {
        let Some(mut renderer) = open_renderer(320, 180, "water_uses_a_separate_pass_and_animates") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = water_world(&registry);
        let mut camera = Camera::new(WorldVec::new(8.0, 72.0, 20.0), 0.0, -0.55);
        camera.far = 128.0;
        let scene = SceneParams::new(Environment::default());
        let mut graphics = renderer.graphics();
        graphics.water = crate::WaterQuality::High;
        renderer.set_graphics(graphics);

        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render water world");
        let moving = renderer.read_rgba();
        let stats = renderer.stats();
        assert!(stats.water_draw_calls > 0, "the water pass drew nothing");
        assert!(stats.water_vertices > 0);
        assert!(stats.draw_calls > 0, "the stone basin should still be opaque");

        // A different animation clock must change the water surface pixels (the
        // rest of the scene is static).
        let animated = SceneParams {
            time_seconds: 4.0,
            ..scene
        };
        renderer
            .render_cached_scene(&camera, &animated)
            .expect("render animated water");
        let later = renderer.read_rgba();
        let changed = moving
            .as_chunks::<4>()
            .0
            .iter()
            .zip(later.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 1 || a[1].abs_diff(b[1]) > 1 || a[2].abs_diff(b[2]) > 1)
            .count();
        assert!(changed > 50, "the animated water clock changed only {changed} pixels");

        // Opaque water (`Off`) must still draw, but looks different.
        let mut graphics = renderer.graphics();
        graphics.water = crate::WaterQuality::Off;
        renderer.set_graphics(graphics);
        renderer
            .render_cached_scene(&camera, &scene)
            .expect("render opaque water");
        assert!(renderer.stats().water_draw_calls > 0);
        let opaque = renderer.read_rgba();
        let different = moving
            .as_chunks::<4>()
            .0
            .iter()
            .zip(opaque.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 2 || a[1].abs_diff(b[1]) > 2 || a[2].abs_diff(b[2]) > 2)
            .count();
        assert!(different > 100, "water quality changed only {different} pixels");
    }

    #[test]
    fn water_only_sections_draw_in_the_water_pass() {
        let Some(mut renderer) = open_renderer(160, 90, "water_only_sections_draw_in_the_water_pass") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let empty = World::new(registry, 1);
        let mut camera = Camera::new(WorldVec::new(8.0, 72.0, 8.0), 0.0, -1.4);
        camera.far = 128.0;
        let scene = SceneParams::new(Environment::default());
        // Start from an empty cache and capture the sky as the background.
        renderer
            .render_world_scene(&empty, &camera, &scene)
            .expect("render empty world");
        let background = renderer.read_rgba();

        // A single water quad at world y = 65 in section (0, 4, 0), with no opaque
        // vertices at all. This is exactly the case the opaque/water split must
        // handle: the section must not be dropped and must issue one water draw.
        let corners = [
            [0.0f32, 65.0, 0.0],
            [0.0, 65.0, 16.0],
            [16.0, 65.0, 16.0],
            [16.0, 65.0, 0.0],
        ];
        let uvs = [[0.0f32, 16.0], [0.0, 0.0], [16.0, 0.0], [16.0, 16.0]];
        let mut water = Vec::new();
        for index in [0usize, 1, 2, 0, 2, 3] {
            water.push(crate::Vertex {
                position: corners[index],
                uv: uvs[index],
                layer: u32::from(ob_content::textures::WATER.0),
                light: crate::Vertex::pack_light(255, 0),
                reserved: 0,
            });
        }
        renderer.upload_mesh(
            SectionPos::new(0, 4, 0),
            crate::MeshData {
                vertices: Vec::new(),
                water_vertices: water,
            },
        );
        let stats = renderer
            .render_cached_scene(&camera, &scene)
            .expect("render water-only section");
        assert_eq!(stats.sections, 0, "there are no opaque triangles to draw");
        assert_eq!(stats.draw_calls, 0);
        assert_eq!(stats.water_draw_calls, 1);
        assert_eq!(stats.water_vertices, 6);
        let pixels = renderer.read_rgba();
        let different = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(background.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 1 || a[1].abs_diff(b[1]) > 1 || a[2].abs_diff(b[2]) > 1)
            .count();
        assert!(different > 100, "the water quad changed only {different} pixels");
    }

    #[test]
    fn clouds_cover_the_sky_deterministically() {
        let Some(mut renderer) = open_renderer(320, 180, "clouds_cover_the_sky_deterministically") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let empty = World::new(registry, 1);
        let camera = Camera::new(WorldVec::new(0.0, 128.0, 0.0), 0.0, 0.18);
        let scene = |time| SceneParams {
            environment: Environment::default(),
            ui: None,
            outline: None,
            time_seconds: time,
        };

        let mut graphics = renderer.graphics();
        graphics.clouds = crate::CloudQuality::High;
        graphics.cloud_coverage = 0.5;
        renderer.set_graphics(graphics);
        renderer
            .render_world_scene(&empty, &camera, &scene(8.0))
            .expect("render clouds");
        let cloudy = renderer.read_rgba();
        renderer
            .render_cached_scene(&camera, &scene(8.0))
            .expect("render clouds again");
        assert_eq!(
            cloudy,
            renderer.read_rgba(),
            "clouds are not deterministic at a fixed time"
        );
        renderer
            .render_cached_scene(&camera, &scene(40.0))
            .expect("render moved clouds");
        let moved = renderer.read_rgba();
        let moved_pixels = cloudy
            .as_chunks::<4>()
            .0
            .iter()
            .zip(moved.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 1 || a[1].abs_diff(b[1]) > 1 || a[2].abs_diff(b[2]) > 1)
            .count();
        assert!(moved_pixels > 500, "the cloud clock moved only {moved_pixels} pixels");

        let mut graphics = renderer.graphics();
        graphics.clouds = crate::CloudQuality::Off;
        renderer.set_graphics(graphics);
        renderer
            .render_cached_scene(&camera, &scene(8.0))
            .expect("render clear sky");
        let clear = renderer.read_rgba();
        let covered = cloudy
            .as_chunks::<4>()
            .0
            .iter()
            .zip(clear.as_chunks::<4>().0)
            .filter(|(a, b)| a[0].abs_diff(b[0]) > 2 || a[1].abs_diff(b[1]) > 2 || a[2].abs_diff(b[2]) > 2)
            .count();
        assert!(covered > 2000, "clouds only covered {covered} pixels");
    }

    #[test]
    fn every_graphics_preset_renders_and_outputs_sane_colours() {
        let Some(mut renderer) = open_renderer(320, 180, "every_graphics_preset_renders_and_outputs_sane_colours")
        else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = water_world(&registry);
        let mut camera = Camera::new(WorldVec::new(8.0, 74.0, 22.0), 0.0, -0.42);
        camera.far = 160.0;
        let scene = SceneParams::new(Environment::new(0.42));
        let mut summaries = Vec::new();
        for preset in crate::GraphicsPreset::ALL {
            renderer.set_graphics(crate::GraphicsSettings::from_preset(preset));
            renderer
                .render_world_scene(&world, &camera, &scene)
                .expect("render preset");
            let stats = renderer.stats();
            let pixels = renderer.read_rgba();
            let mean = mean_luma(&pixels);
            assert!(
                (5.0..250.0).contains(&mean),
                "{} preset produced an implausible frame (mean {mean:.1})",
                preset.name()
            );
            assert!(
                stats.passes >= 3,
                "{} preset recorded only {} passes",
                preset.name(),
                stats.passes
            );
            assert!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|pixel| pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32 > 3)
                    .count()
                    > 1000,
                "{} preset rendered a black frame",
                preset.name()
            );
            summaries.push(preset.name().to_string());
        }
        assert_eq!(summaries.len(), crate::GraphicsPreset::ALL.len());
    }

    #[test]
    fn post_target_resize_reallocates_and_keeps_rendering() {
        let Some(mut renderer) = open_renderer(320, 180, "post_target_resize_reallocates_and_keeps_rendering") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        let camera = Camera::new(WorldVec::new(8.0, 76.0, 8.0), 0.6, -1.0);
        renderer.render_world(&world, &camera).expect("first frame");
        assert_eq!(renderer.read_rgba().len(), 320 * 180 * 4);

        renderer.resize(160, 90);
        assert_eq!(renderer.size(), (160, 90));
        renderer.render_world(&world, &camera).expect("resized frame");
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), 160 * 90 * 4, "readback must follow the resized target");
        assert!(pixels.iter().any(|value| *value != 0));
        assert!(renderer.stats().passes >= 3);

        // Resizing back and to a 1-pixel target must stay valid.
        renderer.resize(320, 180);
        renderer.render_world(&world, &camera).expect("restored frame");
        renderer.resize(1, 1);
        renderer.render_world(&world, &camera).expect("1x1 frame");
        assert_eq!(renderer.read_rgba().len(), 4);
    }

    #[test]
    fn gpu_validation_scopes_are_clean_for_shadow_water_and_post() {
        let Some(mut renderer) = open_renderer(320, 180, "gpu_validation_scopes_are_clean_for_shadow_water_and_post")
        else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = water_world(&registry);
        let mut camera = Camera::new(WorldVec::new(8.0, 74.0, 22.0), 0.0, -0.42);
        camera.far = 160.0;
        let scene = SceneParams::new(Environment::new(0.42));
        renderer.set_graphics(crate::GraphicsSettings::from_preset(crate::GraphicsPreset::Ultra));

        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("shadow/water/post frame");
        renderer.render_cached_scene(&camera, &scene).expect("second frame");
        let error = OffscreenRenderer::pop_validation_scope(scope);
        assert!(error.is_none(), "wgpu validation error: {error:?}");
    }

    // ------------------------------------------------------------------
    // 1.2 lighting and cross-content GPU tests.
    // ------------------------------------------------------------------

    /// A sealed 7×7×3 room inside a 16×16 stone block from y=60..=80.
    ///
    /// The room is carved from (4,64,4)..=(10,66,10); the camera looks at the
    /// far wall, so the frame is black until a torch is placed on the floor.
    fn sealed_light_room(registry: &Arc<Registry>) -> World {
        let stone = registry.id_or_air("openblock:stone");
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        for y in 60..=80 {
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(x, y, z), stone);
                }
            }
        }
        for y in 64..=66 {
            for z in 4..=10 {
                for x in 4..=10 {
                    column.set_block(BlockPos::new(x, y, z), BlockId::AIR);
                }
            }
        }
        column.recompute_heightmap();
        let mut world = World::new(Arc::clone(registry), 11);
        world.insert_chunk(column);
        world
    }

    /// Mean luma of a small square around a framebuffer point.
    fn luma_near(pixels: &[u8], width: u32, height: u32, point: [f32; 2], radius: i32) -> f32 {
        let cx = point[0].round() as i32;
        let cy = point[1].round() as i32;
        let mut sum = 0f64;
        let mut count = 0usize;
        for y in (cy - radius).max(0)..=(cy + radius).min(height as i32 - 1) {
            for x in (cx - radius).max(0)..=(cx + radius).min(width as i32 - 1) {
                let offset = ((y as u32 * width + x as u32) * 4) as usize;
                sum += 0.2126 * f64::from(pixels[offset])
                    + 0.7152 * f64::from(pixels[offset + 1])
                    + 0.0722 * f64::from(pixels[offset + 2]);
                count += 1;
            }
        }
        if count == 0 { 0.0 } else { (sum / count as f64) as f32 }
    }

    #[test]
    fn dark_cave_becomes_readable_with_a_torch() {
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 360;
        let Some(mut renderer) = open_renderer(WIDTH, HEIGHT, "dark_cave_becomes_readable_with_a_torch") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let mut world = sealed_light_room(&registry);
        let torch = registry.id_or_air("openblock:torch");
        assert_ne!(torch, BlockId::AIR);
        let camera = look_at(WorldVec::new(7.5, 66.4, 10.5), [7.5, 65.8, 4.5]);
        let scene = SceneParams::new(Environment::new(0.75));

        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render the dark room");
        assert!(OffscreenRenderer::pop_validation_scope(scope).is_none());
        let dark = renderer.read_rgba();
        let dark_luma = mean_luma(&dark);
        write_test_png(
            &std::env::temp_dir().join("openblock-light-cave-dark.png"),
            WIDTH,
            HEIGHT,
            &dark,
        );

        assert!(world.set_block(BlockPos::new(7, 65, 6), torch));
        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render the torch-lit room");
        assert!(OffscreenRenderer::pop_validation_scope(scope).is_none());
        let lit = renderer.read_rgba();
        let lit_luma = mean_luma(&lit);
        write_test_png(
            &std::env::temp_dir().join("openblock-light-torch.png"),
            WIDTH,
            HEIGHT,
            &lit,
        );

        eprintln!("sealed room mean luma: dark={dark_luma:.2}, torch={lit_luma:.2}");
        assert!(
            dark_luma < 3.0,
            "a sealed cave at night must be nearly black, got {dark_luma}"
        );
        assert!(
            lit_luma > dark_luma + 12.0,
            "a torch must visibly light the room ({dark_luma} -> {lit_luma})"
        );
        // The warm torch tint must reach the visible wall, not just the source.
        let warm = lit
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| {
                i32::from(pixel[0]) > i32::from(pixel[1]) + 18 && i32::from(pixel[1]) > i32::from(pixel[2]) + 8
            })
            .count();
        assert!(warm > 50, "the torch-lit frame has only {warm} warm pixels");
    }

    #[test]
    fn torch_light_crosses_a_chunk_border_in_the_shaded_image() {
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 360;
        let Some(mut renderer) = open_renderer(WIDTH, HEIGHT, "torch_light_crosses_a_chunk_border_in_the_shaded_image")
        else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let stone = registry.id_or_air("openblock:stone");
        let torch = registry.id_or_air("openblock:torch");
        let mut world = World::new(Arc::clone(&registry), 12);
        for cx in 0..2 {
            let mut column = ChunkColumn::new(ChunkPos::new(cx, 0));
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(cx * 16 + x, 64, z), stone);
                }
            }
            world.insert_chunk(column);
        }
        assert!(world.set_block(BlockPos::new(15, 65, 8), torch));

        let camera = look_at(WorldVec::new(15.5, 74.0, 12.0), [15.5, 65.0, 7.0]);
        let scene = SceneParams::new(Environment::new(0.75));
        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render torch across the border");
        assert!(OffscreenRenderer::pop_validation_scope(scope).is_none());
        let pixels = renderer.read_rgba();

        // Sample the floor on the torch side, on the neighbouring-chunk side of
        // x=16, and far away where the torch does not reach.
        let near = project(&camera, WIDTH, HEIGHT, [15.5, 65.02, 10.5]).expect("near point on screen");
        let across = project(&camera, WIDTH, HEIGHT, [16.5, 65.02, 10.5]).expect("across point on screen");
        let far = project(&camera, WIDTH, HEIGHT, [27.5, 65.02, 10.5]).expect("far point on screen");
        let near_luma = luma_near(&pixels, WIDTH, HEIGHT, near, 2);
        let across_luma = luma_near(&pixels, WIDTH, HEIGHT, across, 2);
        let far_luma = luma_near(&pixels, WIDTH, HEIGHT, far, 2);
        eprintln!("torch across border luma: near={near_luma:.2}, across={across_luma:.2}, far={far_luma:.2}");
        assert!(
            near_luma > far_luma + 8.0,
            "the torch must brighten its side ({near_luma} vs {far_luma})"
        );
        assert!(
            across_luma > far_luma + 4.0,
            "light must cross the chunk border ({across_luma} vs {far_luma})"
        );
        assert!(
            (near_luma - across_luma).abs() < 18.0,
            "the border must not show a hard light discontinuity ({near_luma} vs {across_luma})"
        );
    }

    #[test]
    fn cross_cutout_torch_and_wheat_render_on_the_gpu() {
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 360;
        let Some(mut renderer) = open_renderer(WIDTH, HEIGHT, "cross_cutout_torch_and_wheat_render_on_the_gpu") else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let mut world = test_world(&registry);
        let torch = registry.id_or_air("openblock:torch");
        let wheat = registry.id_or_air("openblock:wheat_3");
        let camera = look_at(WorldVec::new(8.5, 66.8, 13.0), [8.5, 66.5, 8.0]);
        let scene = SceneParams::new(Environment::default());

        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render the baseline");
        let baseline = renderer.read_rgba();

        assert!(world.set_block(BlockPos::new(8, 66, 8), torch));
        assert!(world.set_block(BlockPos::new(9, 66, 8), wheat));
        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render cross content");
        assert!(OffscreenRenderer::pop_validation_scope(scope).is_none());
        let lit = renderer.read_rgba();
        write_test_png(
            &std::env::temp_dir().join("openblock-light-cross.png"),
            WIDTH,
            HEIGHT,
            &lit,
        );

        // Restrict the comparison to the projected cross area.
        let center = project(&camera, WIDTH, HEIGHT, [8.5, 66.5, 8.5]).expect("cross center on screen");
        let radius = 55i32;
        let mut changed = 0usize;
        let mut warm = 0usize;
        let mut green = 0usize;
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        let cx = center[0] as i32;
        let cy = center[1] as i32;
        for y in (cy - radius).max(0)..=(cy + radius).min(HEIGHT as i32 - 1) {
            for x in (cx - radius).max(0)..=(cx + radius).min(WIDTH as i32 - 1) {
                let offset = ((y as u32 * WIDTH + x as u32) * 4) as usize;
                let before = &baseline[offset..offset + 4];
                let after = &lit[offset..offset + 4];
                if before[..3]
                    .iter()
                    .zip(after[..3].iter())
                    .any(|(a, b)| a.abs_diff(*b) > 3)
                {
                    changed += 1;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    if i32::from(after[0]) > i32::from(after[1]) + 18 && i32::from(after[1]) > i32::from(after[2]) + 8 {
                        warm += 1;
                    }
                    if i32::from(after[1]) > i32::from(after[0]) + 10 && i32::from(after[1]) > i32::from(after[2]) + 5 {
                        green += 1;
                    }
                }
            }
        }
        eprintln!("cross pixels: changed={changed}, warm={warm}, green={green}");
        assert!(
            changed > 80,
            "the crosses must be visible (only {changed} changed pixels)"
        );
        assert!(warm > 20, "the torch cross has only {warm} warm pixels");
        assert!(green > 5, "the wheat cross has only {green} green pixels");
        let bbox = ((max_x - min_x + 1) * (max_y - min_y + 1)) as usize;
        assert!(
            changed * 10 < bbox * 9,
            "cut-out crosses filled {changed} of {bbox} bounding-box pixels; alpha test may be broken"
        );
    }

    #[test]
    fn far_512_scene_still_renders_with_the_new_vertex_layout() {
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 360;
        let Some(mut renderer) = open_renderer(WIDTH, HEIGHT, "far_512_scene_still_renders_with_the_new_vertex_layout")
        else {
            return;
        };
        let registry = Arc::new(Registry::base());
        let world = test_world(&registry);
        // A synthetic far batch at x≈500 exercises the 512-block far plane, LOD
        // buffer format and fog with the 32-byte vertex. Lighting adds no extra
        // draw pass, so a rendered far quad is the streaming smoke test here.
        let layer = u32::from(ob_content::textures::STONE.0);
        let mut mesh = crate::MeshData::default();
        for index in [0usize, 1, 2, 0, 2, 3] {
            let corner = [
                [500.0f32, 70.0, 0.0],
                [500.0, 70.0, 16.0],
                [516.0, 70.0, 16.0],
                [516.0, 70.0, 0.0],
            ][index];
            mesh.vertices.push(crate::Vertex {
                position: corner,
                uv: [[0.0f32, 16.0], [0.0, 0.0], [16.0, 0.0], [16.0, 16.0]][index],
                layer,
                light: crate::Vertex::pack_light(255, 0),
                reserved: 0,
            });
        }
        renderer.upload_lod_batch(
            crate::LodBatchKey::containing(ChunkPos::new(31, 0), 2),
            [500.0, 70.0, 0.0],
            [516.0, 70.0, 16.0],
            mesh,
        );

        let mut camera = Camera::new(WorldVec::new(0.0, 90.0, 8.0), std::f32::consts::FRAC_PI_2, -0.05);
        camera.far = 512.0;
        let scene = SceneParams::new(Environment::default());
        let scope = renderer.push_validation_scope();
        renderer
            .render_world_scene(&world, &camera, &scene)
            .expect("render the 512-block scene");
        assert!(OffscreenRenderer::pop_validation_scope(scope).is_none());
        let pixels = renderer.read_rgba();
        assert!(mean_luma(&pixels) > 5.0, "the far scene must not be fully black");
    }
}

//! Offscreen rendering: draw the world into a fixed-size colour texture and read
//! the pixels back as RGBA8.
//!
//! This path needs no window or display. It reuses the same sky pass, shader,
//! vertex layout, texture array, camera uniform, fog, frustum culling and sky
//! palette as the windowed renderer, so it verifies the GPU pipeline as well as
//! producing screenshots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ob_math::{N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};
use ob_world::World;

use crate::textures::BlockTextureArray;
use crate::ui::{UiFrame, UiRenderer};
use crate::{
    BlockOutline, Camera, Environment, OUTLINE_VERTEX_COUNT, RenderError, RenderStats, SceneParams, Vertex, mesh,
    pipeline, section_visible,
};

/// Offscreen colour target format (sRGB, readable through `copy_texture_to_buffer`).
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A windowless renderer that draws the world into an RGBA8 texture.
pub struct OffscreenRenderer {
    /// GPU device.
    device: wgpu::Device,
    /// Submission queue.
    queue: wgpu::Queue,
    /// Chunk render pipeline.
    pipeline: wgpu::RenderPipeline,
    /// Full-screen sky gradient pipeline.
    sky_pipeline: wgpu::RenderPipeline,
    /// Targeted-block outline line-list pipeline.
    outline_pipeline: wgpu::RenderPipeline,
    /// Camera, fog and environment uniform buffer.
    uniform_buffer: wgpu::Buffer,
    /// Small per-frame outline vertex buffer.
    outline_buffer: wgpu::Buffer,
    /// Procedural block texture array and sampler.
    ///
    /// Kept alive here even though the bind group also retains its view.
    _textures: BlockTextureArray,
    /// Camera and texture bind group.
    bind_group: wgpu::BindGroup,
    /// Offscreen colour texture.
    color_texture: wgpu::Texture,
    /// View of the colour texture used as the render target.
    color_view: wgpu::TextureView,
    /// Depth texture view.
    depth_view: wgpu::TextureView,
    /// Target width in pixels.
    width: u32,
    /// Target height in pixels.
    height: u32,
    /// Adapter description.
    adapter_summary: String,
    /// RGBA8 pixels of the last rendered frame.
    pixels: Vec<u8>,
    /// Statistics from the last rendered frame.
    stats: RenderStats,
    /// Immediate-mode UI renderer for screenshots with HUD/menus.
    ui: UiRenderer,
    /// Cached section meshes (rebuilt on demand; used by the benchmark path).
    meshes: HashMap<SectionPos, CachedMesh>,
}

/// A cached vertex buffer for one section.
struct CachedMesh {
    buffer: wgpu::Buffer,
    vertex_count: u32,
}

impl OffscreenRenderer {
    /// Creates an offscreen renderer with a fixed `width` x `height` target.
    pub fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        Self::new_with_style(width, height, crate::TexturePackStyle::default())
    }

    /// Creates an offscreen renderer with a specific texture-pack style.
    pub fn new_with_style(width: u32, height: u32, style: crate::TexturePackStyle) -> Result<Self, RenderError> {
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
        let depth_view = pipeline::create_depth_view(&device, width, height);

        let uniform_buffer = pipeline::create_camera_buffer(&device);
        let outline_buffer = pipeline::create_outline_buffer(&device);
        let textures = BlockTextureArray::new_with_style(&device, &queue, style);
        let ui = UiRenderer::new(&device, &queue, &textures, COLOR_FORMAT);
        let bind_group_layout = pipeline::create_bind_group_layout(&device);
        let bind_group = pipeline::create_bind_group(&device, &bind_group_layout, &uniform_buffer, &textures);
        let pipeline = pipeline::create_pipeline(&device, COLOR_FORMAT, &bind_group_layout);
        let sky_pipeline = pipeline::create_sky_pipeline(&device, COLOR_FORMAT, &bind_group_layout);
        let outline_pipeline = pipeline::create_outline_pipeline(&device, COLOR_FORMAT, &bind_group_layout);

        Ok(Self {
            device,
            queue,
            pipeline,
            sky_pipeline,
            outline_pipeline,
            uniform_buffer,
            outline_buffer,
            _textures: textures,
            bind_group,
            color_texture,
            color_view,
            depth_view,
            width,
            height,
            adapter_summary,
            pixels: Vec::new(),
            stats: RenderStats::default(),
            ui,
            meshes: HashMap::new(),
        })
    }

    /// Human-readable adapter description for logs and `--version`.
    pub fn adapter_summary(&self) -> &str {
        &self.adapter_summary
    }

    /// Statistics from the last rendered frame.
    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Rebuilds the block texture array and its bind groups for a new style.
    pub fn set_texture_style(&mut self, style: crate::TexturePackStyle) {
        if self._textures.style() == style {
            return;
        }
        let textures = BlockTextureArray::new_with_style(&self.device, &self.queue, style);
        let layout = pipeline::create_bind_group_layout(&self.device);
        self.bind_group = pipeline::create_bind_group(&self.device, &layout, &self.uniform_buffer, &textures);
        self.ui = UiRenderer::new(&self.device, &self.queue, &textures, COLOR_FORMAT);
        self._textures = textures;
    }

    /// Currently active procedural texture-pack style.
    pub fn texture_style(&self) -> crate::TexturePackStyle {
        self._textures.style()
    }

    /// Rebuilds the cached GPU meshes for every non-empty loaded section.
    ///
    /// Returns the number of cached sections and vertices. The benchmark path
    /// calls this once, then times cached frames so meshing and upload do not
    /// dominate the measurement.
    pub fn rebuild_mesh_cache(&mut self, world: &World) -> (u32, u64) {
        self.meshes.clear();
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
            let vertex_count = section_mesh.vertices.len() as u32;
            let size = (section_mesh.vertices.len() * std::mem::size_of::<Vertex>()) as wgpu::BufferAddress;
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("offscreen-cached-section"),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue
                .write_buffer(&buffer, 0, bytemuck::cast_slice(&section_mesh.vertices));
            self.meshes.insert(pos, CachedMesh { buffer, vertex_count });
            sections += 1;
            vertices += u64::from(vertex_count);
        }
        (sections, vertices)
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
            },
        )
    }

    /// Renders the cached meshes for a full scene and reads the frame back.
    ///
    /// This is the offscreen counterpart of [`crate::Renderer::render_scene`]:
    /// the sky pass uses the scene environment, the world loads that colour and
    /// draws the chunk meshes, the outline (when present) is drawn next with
    /// `LessEqual` depth testing and no depth writes, and the UI frame is drawn
    /// last. The GPU-to-CPU readback is included in the returned timing, like
    /// [`Self::render_cached_with_ui`].
    pub fn render_cached_scene(
        &mut self,
        camera: &Camera,
        scene: &SceneParams<'_>,
    ) -> Result<RenderStats, RenderError> {
        let aspect = self.width as f32 / self.height as f32;
        let uniform = pipeline::CameraUniform::new(camera, aspect, scene.environment);
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

        let mut stats = RenderStats::default();
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen-cached-frame"),
        });
        {
            // Sky first, with no depth attachment: one full-screen triangle.
            let mut sky = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen-sky"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
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
                label: Some("offscreen-main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
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
            for (pos, cached) in &self.meshes {
                if !section_visible(&frustum, *pos) {
                    continue;
                }
                pass.set_vertex_buffer(0, cached.buffer.slice(..));
                pass.draw(0..cached.vertex_count, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(cached.vertex_count);
                stats.sections += 1;
            }
            // Outline after the world geometry, before the UI pass. The depth
            // buffer already holds the nearest opaque surface, so terrain in
            // front of the selection hides it.
            if outline_vertices.is_some() {
                pass.set_pipeline(&self.outline_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.outline_buffer.slice(..));
                pass.draw(0..OUTLINE_VERTEX_COUNT as u32, 0..1);
            }
        }
        if let Some(ui_frame) = scene.ui {
            self.ui.render(&mut encoder, &self.color_view, ui_frame);
        }
        self.queue.submit([encoder.finish()]);
        self.read_back()?;
        self.stats = stats;
        Ok(stats)
    }

    /// Number of cached section meshes (0 before `rebuild_mesh_cache`).
    pub fn cached_mesh_count(&self) -> usize {
        self.meshes.len()
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
        let _ = self.render_cached_scene(camera, scene)?;
        log::info!(
            "offscreen frame: {} visible sections drawn, {} vertices, {} draws",
            self.stats.sections,
            self.stats.vertices,
            self.stats.draw_calls
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
    use std::collections::HashMap;

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos, Vec4, WorldVec};
    use ob_world::ChunkColumn;

    use super::*;

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
            .render_world(&world, &high_camera)
            .expect("render culled world");
        assert_eq!(renderer.stats().sections, 0);
        assert_eq!(renderer.stats().draw_calls, 0);
        let culled = renderer.read_rgba();
        assert_eq!(culled.len(), background.len());
        assert!(
            culled
                .iter()
                .zip(background.iter())
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

        let camera = Camera::new(WorldVec::new(0.0, 128.0, 0.0), 0.0, 0.0);
        renderer.render_world(&empty, &camera).expect("render sky only");
        assert_eq!(renderer.stats().sections, 0);
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), 320 * 180 * 4);

        // The gradient is a function of screen Y only.
        assert_rows_uniform(&pixels, 320, 180, 1);

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
            Ok(renderer) => {
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

    /// sRGB byte value of a linear RGB channel, matching the sRGB render target.
    fn linear_to_srgb_byte(value: f32) -> f32 {
        let srgb = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        (srgb.clamp(0.0, 1.0) * 255.0).round()
    }

    fn is_outline_pixel(pixel: &[u8]) -> bool {
        pixel[0] >= 240 && pixel[1] >= 240 && pixel[2] >= 240
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
            let expected = [
                linear_to_srgb_byte(expected[0]),
                linear_to_srgb_byte(expected[1]),
                linear_to_srgb_byte(expected[2]),
            ];
            for channel in 0..3 {
                assert!(
                    (horizon[channel] - expected[channel]).abs() <= 2.0,
                    "time {}: horizon row {horizon:?} does not match the CPU horizon {expected:?}",
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
}

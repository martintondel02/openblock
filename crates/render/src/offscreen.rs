//! Offscreen rendering: draw the world into a fixed-size colour texture and read
//! the pixels back as RGBA8.
//!
//! This path needs no window or display. It reuses the same shader, vertex
//! layout, texture array, camera uniform, frustum culling and sky clear colour
//! as the windowed renderer, so it verifies the GPU pipeline as well as
//! producing screenshots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ob_math::{N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};
use ob_world::World;

use crate::textures::BlockTextureArray;
use crate::ui::{UiFrame, UiRenderer};
use crate::{Camera, RenderError, RenderStats, Vertex, mesh, pipeline, section_visible};

/// Offscreen colour target format (sRGB, readable through `copy_texture_to_buffer`).
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Upper bound on the number of sections meshed and drawn in one offscreen frame.
const MAX_SECTIONS: usize = 4096;

/// Sky clear colour, matching the windowed renderer's clear.
const SKY_COLOR: wgpu::Color = wgpu::Color {
    r: 0.55,
    g: 0.72,
    b: 0.95,
    a: 1.0,
};

/// A windowless renderer that draws the world into an RGBA8 texture.
pub struct OffscreenRenderer {
    /// GPU device.
    device: wgpu::Device,
    /// Submission queue.
    queue: wgpu::Queue,
    /// Chunk render pipeline.
    pipeline: wgpu::RenderPipeline,
    /// Camera uniform buffer.
    uniform_buffer: wgpu::Buffer,
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
        let textures = BlockTextureArray::new(&device, &queue);
        let ui = UiRenderer::new(&device, &queue, &textures, COLOR_FORMAT);
        let bind_group_layout = pipeline::create_bind_group_layout(&device);
        let bind_group = pipeline::create_bind_group(&device, &bind_group_layout, &uniform_buffer, &textures);
        let pipeline = pipeline::create_pipeline(&device, COLOR_FORMAT, &bind_group_layout);

        Ok(Self {
            device,
            queue,
            pipeline,
            uniform_buffer,
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
        let aspect = self.width as f32 / self.height as f32;
        let view_projection = camera.view_projection(aspect);
        let uniform = pipeline::CameraUniform {
            view_proj: view_projection.to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        let frustum = camera.frustum_planes(aspect);

        let mut stats = RenderStats::default();
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen-cached-frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen-main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(SKY_COLOR),
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
        }
        if let Some(ui_frame) = ui {
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
        self.rebuild_mesh_cache(world);
        let _ = self.render_cached_with_ui(camera, ui)?;
        log::info!(
            "offscreen frame: {} visible sections drawn, {} vertices, {} draws",
            self.stats.sections,
            self.stats.vertices,
            self.stats.draw_calls
        );
        Ok(())
    }

    /// Old uncached body kept for reference; unreachable.
    #[allow(dead_code)]
    fn render_world_uncached(&mut self, world: &World, camera: &Camera) -> Result<(), RenderError> {
        let aspect = self.width as f32 / self.height as f32;
        let view_projection = camera.view_projection(aspect);
        let uniform = pipeline::CameraUniform {
            view_proj: view_projection.to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        let frustum = camera.frustum_planes(aspect);

        // Collect the loaded section positions in a deterministic order, then
        // cull before meshing so offscreen screenshots skip invisible work.
        let base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
        let mut positions: Vec<SectionPos> = world
            .chunks()
            .flat_map(|(chunk_pos, _column)| {
                (0..N_SECTIONS).map(move |index| SectionPos::new(chunk_pos.x, base_y + index as i32, chunk_pos.z))
            })
            .collect();
        positions.sort_unstable();

        let mut draws: Vec<(wgpu::Buffer, u32)> = Vec::new();
        let mut stats = RenderStats::default();
        'sections: for pos in positions {
            if draws.len() >= MAX_SECTIONS {
                break 'sections;
            }
            if !section_visible(&frustum, pos) {
                continue;
            }
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
                label: Some("offscreen-section-mesh"),
                size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue
                .write_buffer(&buffer, 0, bytemuck::cast_slice(&section_mesh.vertices));
            draws.push((buffer, vertex_count));
            stats.draw_calls += 1;
            stats.vertices += u64::from(vertex_count);
            stats.sections += 1;
        }
        self.stats = stats;

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("offscreen-frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen-main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(SKY_COLOR),
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
            for (buffer, vertex_count) in &draws {
                pass.set_vertex_buffer(0, buffer.slice(..));
                pass.draw(0..*vertex_count, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);

        self.read_back()?;
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
    use ob_math::{BlockPos, ChunkPos, WorldVec};
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

    #[test]
    fn offscreen_render_is_not_black() {
        // The renderer itself must not depend on worldgen or content beyond the
        // base registry, so the test builds its chunk by hand.
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

        // Render the same clear pass with an empty world first, so the expected
        // sky colour is measured instead of assumed.
        let empty = World::new(Arc::clone(&registry), 1);
        renderer.render_world(&empty, &camera).expect("render empty world");
        let background = renderer.read_rgba();
        assert_eq!(background.len(), 320 * 180 * 4);
        assert_eq!(renderer.stats().sections, 0);
        let clear = [background[0], background[1], background[2]];
        assert!(
            background
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] == clear[0] && p[1] == clear[1] && p[2] == clear[2]),
            "an empty offscreen frame must be a uniform clear colour"
        );

        renderer.render_world(&world, &camera).expect("render test platform");
        let pixels = renderer.read_rgba();
        assert_eq!(pixels.len(), 320 * 180 * 4);
        assert!(renderer.stats().sections > 0, "the test platform should be visible");
        assert_eq!(renderer.stats().sections, renderer.stats().draw_calls);
        assert!(renderer.stats().vertices > 0);

        let mut histogram: HashMap<[u8; 3], usize> = HashMap::new();
        let mut different = 0usize;
        for pixel in pixels.as_chunks::<4>().0 {
            let rgb = [pixel[0], pixel[1], pixel[2]];
            if rgb != clear {
                different += 1;
            }
            *histogram.entry(rgb).or_insert(0) += 1;
        }
        let total = (320 * 180) as usize;
        let fraction = different as f32 / total as f32;

        let mut top: Vec<([u8; 3], usize)> = histogram.iter().map(|(k, v)| (*k, *v)).collect();
        top.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
        eprintln!(
            "offscreen: clear={clear:?}, differing={different}/{total} ({:.2}%), distinct={}",
            fraction * 100.0,
            histogram.len()
        );
        for (rgb, count) in top.iter().take(8) {
            eprintln!("  {rgb:?}: {count}");
        }

        assert!(
            fraction >= 0.01,
            "only {:.2}% of pixels differ from the clear colour",
            fraction * 100.0
        );
        // Procedural 16×16 textures produce many more shades than the old
        // per-face vertex colours: water/stone/grass noise and AO alone give
        // well over sixteen distinct RGB values.
        assert!(
            histogram.len() >= 16,
            "only {} distinct RGB values were drawn",
            histogram.len()
        );

        // A camera far above the world with a short far plane must cull every
        // section: the image returns to the uniform sky and no section is drawn.
        let mut high_camera = Camera::new(WorldVec::new(8.0, 220.0, 8.0), 0.0, 0.0);
        high_camera.far = 64.0;
        renderer
            .render_world(&world, &high_camera)
            .expect("render culled world");
        assert_eq!(renderer.stats().sections, 0);
        assert_eq!(renderer.stats().draw_calls, 0);
        let culled = renderer.read_rgba();
        assert!(
            culled
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0] == clear[0] && p[1] == clear[1] && p[2] == clear[2]),
            "a fully culled frame must be the clear colour",
        );
    }

    #[test]
    fn offscreen_ui_overlay_draws_over_the_world() {
        // Proves the full integration: world pass, then the alpha-blended UI
        // pass into the same colour target, then readback.
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
}

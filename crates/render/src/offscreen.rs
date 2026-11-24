//! Offscreen rendering: draw the world into a fixed-size colour texture and read
//! the pixels back as RGBA8.
//!
//! This path needs no window or display. It reuses the same shader, vertex
//! layout, camera uniform and sky clear colour as the windowed renderer, so it
//! verifies the GPU pipeline as well as producing screenshots.

use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use ob_math::{N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};
use ob_world::World;

use crate::{Camera, RenderError, Vertex, mesh};

/// Depth buffer format used by the forward pass.
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Offscreen colour target format (sRGB, readable through `copy_texture_to_buffer`).
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Byte size of the camera uniform block (`mat4x4<f32>`).
const CAMERA_UNIFORM_SIZE: wgpu::BufferAddress = 64;

/// Upper bound on the number of sections meshed and drawn in one offscreen frame.
const MAX_SECTIONS: usize = 4096;

/// Sky clear colour, matching the windowed renderer's clear.
const SKY_COLOR: wgpu::Color = wgpu::Color {
    r: 0.55,
    g: 0.72,
    b: 0.95,
    a: 1.0,
};

/// Camera uniform uploaded once per frame (group 0, binding 0).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    /// Column-major combined view-projection matrix.
    view_proj: [[f32; 4]; 4],
}

// Keep the GPU layout in sync with the shader's `Camera` struct.
const _: () = assert!(std::mem::size_of::<CameraUniform>() == CAMERA_UNIFORM_SIZE as usize);

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
    /// Camera bind group.
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
        let depth_view = Self::create_depth_view(&device, width, height);

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offscreen-camera-uniform"),
            size: CAMERA_UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("offscreen-camera-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(CAMERA_UNIFORM_SIZE),
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("offscreen-camera"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniform_buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(CAMERA_UNIFORM_SIZE),
                }),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("offscreen-chunk-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("offscreen-chunk-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let vertex_attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 12,
                shader_location: 1,
            },
        ];
        let vertex_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &vertex_attributes,
        })];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("offscreen-chunk-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &vertex_buffers,
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
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            uniform_buffer,
            bind_group,
            color_texture,
            color_view,
            depth_view,
            width,
            height,
            adapter_summary,
            pixels: Vec::new(),
        })
    }

    /// Creates the depth texture and view for the fixed drawable size.
    fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("offscreen-depth-view"),
            ..Default::default()
        })
    }

    /// Human-readable adapter description for logs and `--version`.
    pub fn adapter_summary(&self) -> &str {
        &self.adapter_summary
    }

    /// Renders every non-empty loaded section into the offscreen target.
    ///
    /// Vertex buffers are rebuilt on every call: screenshots are one-shot and
    /// tests need no cache, so caching would only add invalidation complexity.
    pub fn render_world(&mut self, world: &World, camera: &Camera) -> Result<(), RenderError> {
        let aspect = self.width as f32 / self.height as f32;
        let uniform = CameraUniform {
            view_proj: camera.view_projection(aspect).to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        // Mesh and upload the loaded sections. `world.chunks()` supplies the
        // columns, `world.section()` the absolute section positions.
        let mut draws: Vec<(wgpu::Buffer, u32)> = Vec::new();
        'sections: for (chunk_pos, _column) in world.chunks() {
            for index in 0..N_SECTIONS {
                if draws.len() >= MAX_SECTIONS {
                    break 'sections;
                }
                let y = WORLD_MIN_Y / SECTION_SIZE as i32 + index as i32;
                let pos = SectionPos::new(chunk_pos.x, y, chunk_pos.z);
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
            }
        }

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
        assert!(
            histogram.len() >= 8,
            "only {} distinct RGB values were drawn",
            histogram.len()
        );
    }
}

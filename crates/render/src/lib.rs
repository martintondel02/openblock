//! The GPU renderer built on wgpu: voxel meshing, mesh upload and shading.
//!
//! v0.0.1 scope: one forward pass, vertex-coloured chunk geometry with baked
//! face shading and ambient occlusion, a depth buffer and a sky clear. The
//! render-graph, GPU-driven submission and post chain in
//! `docs/spec/03-rendering.md` arrive at MS-04.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

pub mod mesh;
pub mod offscreen;

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use ob_math::{Mat4, SectionPos, WorldVec};
use winit::window::Window;

pub use mesh::{MesherStats, mesh_section};
pub use offscreen::OffscreenRenderer;

/// Depth buffer format used by the forward pass.
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Byte size of the camera uniform block (`mat4x4<f32>`).
const CAMERA_UNIFORM_SIZE: wgpu::BufferAddress = 64;

/// Camera uniform uploaded once per frame (group 0, binding 0).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    /// Column-major combined view-projection matrix.
    view_proj: [[f32; 4]; 4],
}

// Keep the GPU layout in sync with the shader's `Camera` struct.
const _: () = assert!(std::mem::size_of::<CameraUniform>() == CAMERA_UNIFORM_SIZE as usize);

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

    /// Combined view-projection matrix for an aspect ratio.
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        let forward = ob_math::forward_from_angles(self.yaw, self.pitch);
        let eye = self.position.as_vec3();
        let view = ob_math::look_to_rh(eye, forward, ob_math::RenderVec::Y);
        let proj = ob_math::perspective_rh_zo(self.fov_y, aspect.max(0.01), self.near, self.far);
        proj * view
    }
}

/// One renderable vertex: world position plus baked RGBA colour.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Baked colour (face shading and ambient occlusion included).
    pub color: [f32; 4],
}

/// A CPU-side mesh for one section.
#[derive(Debug, Default, Clone)]
pub struct MeshData {
    /// Triangle vertices (six per visible face).
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
    /// Draw calls issued for chunk geometry.
    pub draw_calls: u32,
    /// Vertices drawn.
    pub vertices: u64,
    /// Number of uploaded section meshes.
    pub sections: u32,
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
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::NoAdapter(reason) => write!(f, "no suitable GPU adapter: {reason}"),
            RenderError::Device(reason) => write!(f, "GPU device creation failed: {reason}"),
            RenderError::Surface(reason) => write!(f, "window surface error: {reason}"),
            RenderError::SurfaceOutOfDate => write!(f, "window surface is out of date"),
            RenderError::SurfaceLost => write!(f, "window surface was lost"),
        }
    }
}

impl std::error::Error for RenderError {}

/// GPU resources for one section mesh.
#[derive(Debug)]
pub struct GpuMesh {
    /// Vertex buffer.
    pub buffer: wgpu::Buffer,
    /// Number of vertices in the buffer.
    pub vertex_count: u32,
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
    /// Render pipeline.
    pub(crate) pipeline: wgpu::RenderPipeline,
    /// Depth texture view.
    pub(crate) depth_view: wgpu::TextureView,
    /// Camera uniform buffer.
    pub(crate) uniform_buffer: wgpu::Buffer,
    /// Camera bind group.
    pub(crate) bind_group: wgpu::BindGroup,
    /// Uploaded section meshes.
    pub(crate) meshes: HashMap<SectionPos, GpuMesh>,
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
        let depth_view = Self::create_depth_view(&device, width, height);

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera-uniform"),
            size: CAMERA_UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera-layout"),
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
            label: Some("camera"),
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
            label: Some("chunk-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("chunk-shader"),
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
            label: Some("chunk-pipeline"),
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
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            depth_view,
            uniform_buffer,
            bind_group,
            meshes: HashMap::new(),
            adapter_summary,
            stats: RenderStats::default(),
            window,
        })
    }

    /// Creates the depth texture and view for the given drawable size.
    fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
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
            label: Some("depth-view"),
            ..Default::default()
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
        self.depth_view = Self::create_depth_view(&self.device, width, height);
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

    /// Renders one frame of all uploaded section meshes.
    pub fn render(&mut self, camera: &Camera) -> Result<(), RenderError> {
        let (width, height) = self.size();
        let aspect = width as f32 / height as f32;
        let uniform = CameraUniform {
            view_proj: camera.view_projection(aspect).to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

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
        // independent of draw order either way.
        let mut order: Vec<SectionPos> = self.meshes.keys().copied().collect();
        order.sort_unstable();

        let mut stats = RenderStats {
            sections: self.meshes.len() as u32,
            ..RenderStats::default()
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.55,
                            g: 0.72,
                            b: 0.95,
                            a: 1.0,
                        }),
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
            }
        }
        self.stats = stats;
        self.queue.submit([encoder.finish()]);
        drop(view);
        self.queue.present(frame);
        Ok(())
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

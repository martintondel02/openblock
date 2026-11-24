//! Shared pipeline construction used by the windowed and offscreen renderers.
//!
//! Both paths bind the camera uniform, the block texture array and its sampler
//! with the same layout (`group(0)`, bindings 0..2), and use the same vertex
//! format, so the pipeline and bind-group code lives here instead of being
//! duplicated. The screen-space sky pipeline and the targeted-block outline
//! pipeline reuse that camera bind group; their shaders only read binding 0, and
//! unused bindings in the layout are allowed.
//!
//! [`CameraUniform`] carries the camera matrices and fog bounds plus the
//! CPU-computed day/night state (palette, sun/moon, stars, world light). Keeping
//! it in one uniform means the sky, the world and the outline can never disagree
//! about the current environment.

use bytemuck::{Pod, Zeroable};

use crate::environment::Environment;
use crate::outline::{OUTLINE_VERTEX_COUNT, OutlineVertex};
use crate::textures::BlockTextureArray;
use crate::{Camera, Vertex};

/// Depth buffer format used by the forward pass.
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Byte size of the camera uniform block: four `mat4x4<f32>` rows followed by ten
/// `vec4<f32>` rows.
pub(crate) const CAMERA_UNIFORM_SIZE: wgpu::BufferAddress = 224;

/// Number of bytes in one outline vertex buffer.
pub(crate) const OUTLINE_BUFFER_SIZE: wgpu::BufferAddress =
    OUTLINE_VERTEX_COUNT as wgpu::BufferAddress * std::mem::size_of::<OutlineVertex>() as wgpu::BufferAddress;

/// Camera, fog, sky and lighting uniform uploaded once per frame (group 0,
/// binding 0).
///
/// The 224-byte layout is fourteen 16-byte-aligned rows: the view-projection
/// matrix, the camera world position, the fog bounds and the day/night state.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct CameraUniform {
    /// Column-major combined view-projection matrix.
    pub(crate) view_proj: [[f32; 4]; 4],
    /// Eye position in world space; `w` is unused.
    pub(crate) camera_pos: [f32; 4],
    /// `x` fog start distance, `y` fog end distance, `zw` unused.
    pub(crate) fog: [f32; 4],
    /// `xyz` unit vector towards the sun; `w` sun-disc intensity.
    pub(crate) sun: [f32; 4],
    /// `rgb` linear sun-disc colour; `w` star visibility.
    pub(crate) sun_colour: [f32; 4],
    /// `rgb` linear zenith colour; `w` ambient light multiplier.
    pub(crate) zenith: [f32; 4],
    /// `rgb` linear horizon colour (also the fog colour); `w` daylight factor
    /// multiplied into baked block light.
    pub(crate) horizon: [f32; 4],
    /// `xyz` unit camera forward; `w` `tan(fov_y / 2)`.
    pub(crate) basis_forward: [f32; 4],
    /// `xyz` unit camera right; `w` viewport aspect ratio.
    pub(crate) basis_right: [f32; 4],
    /// `xyz` unit camera up; `w` star-field rotation in radians.
    pub(crate) basis_up: [f32; 4],
    /// `rgb` daylight-scaled light tint for block geometry; `w` moon
    /// visibility.
    pub(crate) world_light: [f32; 4],
}

// Keep the GPU layout in sync with the shader's `Camera` struct.
const _: () = assert!(std::mem::size_of::<CameraUniform>() == CAMERA_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<CameraUniform>() == 16);
const _: () = assert!(std::mem::offset_of!(CameraUniform, view_proj) == 0);
const _: () = assert!(std::mem::offset_of!(CameraUniform, camera_pos) == 64);
const _: () = assert!(std::mem::offset_of!(CameraUniform, fog) == 80);
const _: () = assert!(std::mem::offset_of!(CameraUniform, sun) == 96);
const _: () = assert!(std::mem::offset_of!(CameraUniform, sun_colour) == 112);
const _: () = assert!(std::mem::offset_of!(CameraUniform, zenith) == 128);
const _: () = assert!(std::mem::offset_of!(CameraUniform, horizon) == 144);
const _: () = assert!(std::mem::offset_of!(CameraUniform, basis_forward) == 160);
const _: () = assert!(std::mem::offset_of!(CameraUniform, basis_right) == 176);
const _: () = assert!(std::mem::offset_of!(CameraUniform, basis_up) == 192);
const _: () = assert!(std::mem::offset_of!(CameraUniform, world_light) == 208);

impl CameraUniform {
    /// Builds the per-frame uniform for a camera, viewport aspect ratio and
    /// day/night environment.
    ///
    /// Fog bounds come from [`Camera::fog_range`], so the windowed and offscreen
    /// renderers always use the same distances for the same camera. The palette
    /// comes from [`Environment::visual`], so both paths also agree on the sky,
    /// sun/moon, stars and world light.
    pub(crate) fn new(camera: &Camera, aspect: f32, environment: Environment) -> Self {
        let eye = camera.position.as_vec3();
        let (fog_start, fog_end) = camera.fog_range();
        let visual = environment.visual();
        let aspect = aspect.max(0.01);
        let forward = camera.forward();
        let right = camera.right();
        let up = camera.up();
        let tangent = (camera.fov_y * 0.5).tan();
        Self {
            view_proj: camera.view_projection(aspect).to_cols_array_2d(),
            camera_pos: [eye.x, eye.y, eye.z, 1.0],
            fog: [fog_start, fog_end, 0.0, 0.0],
            sun: [
                visual.sun_direction[0],
                visual.sun_direction[1],
                visual.sun_direction[2],
                visual.sun_intensity,
            ],
            sun_colour: [
                visual.sun_color[0],
                visual.sun_color[1],
                visual.sun_color[2],
                visual.star_visibility,
            ],
            zenith: [
                visual.zenith[0],
                visual.zenith[1],
                visual.zenith[2],
                visual.ambient_light,
            ],
            horizon: [visual.horizon[0], visual.horizon[1], visual.horizon[2], visual.daylight],
            basis_forward: [forward[0], forward[1], forward[2], tangent],
            basis_right: [right[0], right[1], right[2], aspect],
            basis_up: [up[0], up[1], up[2], visual.star_rotation],
            world_light: [
                visual.light_tint[0] * visual.daylight,
                visual.light_tint[1] * visual.daylight,
                visual.light_tint[2] * visual.daylight,
                visual.moon_visibility,
            ],
        }
    }

    /// Clear colour matching the sky horizon, used behind the full-screen sky
    /// triangle.
    ///
    /// The triangle covers every pixel, so this value is only a fallback; using
    /// the current horizon keeps a partial sky pass from flashing a contrasting
    /// colour at night.
    pub(crate) fn sky_clear(&self) -> wgpu::Color {
        wgpu::Color {
            r: f64::from(self.horizon[0]),
            g: f64::from(self.horizon[1]),
            b: f64::from(self.horizon[2]),
            a: 1.0,
        }
    }
}

/// Creates the camera uniform buffer (uniform + copy destination).
pub(crate) fn create_camera_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("camera-uniform"),
        size: CAMERA_UNIFORM_SIZE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Creates the small per-frame outline vertex buffer.
pub(crate) fn create_outline_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("block-outline-vertices"),
        size: OUTLINE_BUFFER_SIZE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Creates the bind-group layout shared by every chunk pipeline.
pub(crate) fn create_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("chunk-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                // The vertex stage projects positions; the fragment stage needs
                // the camera position, fog bounds and environment palette.
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(CAMERA_UNIFORM_SIZE),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
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

/// Binds the camera buffer, texture array view and sampler.
pub(crate) fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    camera: &wgpu::Buffer,
    textures: &BlockTextureArray,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("chunk-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: camera,
                    offset: 0,
                    size: wgpu::BufferSize::new(CAMERA_UNIFORM_SIZE),
                }),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&textures.view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&textures.sampler),
            },
        ],
    })
}

/// Creates a shader module for the chunk, sky or outline pipelines.
fn create_shader_module(device: &wgpu::Device, label: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    })
}

/// Creates the forward chunk pipeline: textured, depth-tested, no culling.
pub(crate) fn create_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("chunk-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "chunk-shader");
    // Keep these offsets in sync with `Vertex` and shader.wgsl.
    let vertex_attributes = [
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 12,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 20,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 24,
            shader_location: 3,
        },
    ];
    let vertex_buffers = [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &vertex_attributes,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                format: target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the sky pipeline: one full-screen triangle, camera uniform only, no
/// depth.
pub(crate) fn create_sky_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sky-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "sky-shader");
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sky-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_sky"),
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
        // The sky is pure background: it runs before the world pass and must
        // not interact with depth.
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_sky"),
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

/// Creates the targeted-block outline pipeline: a line list with alpha
/// blending, `LessEqual` depth testing and no depth writes.
///
/// Line primitives rasterise one physical pixel wide on the Vulkan backend, so
/// the outline stays crisp at any resolution.
pub(crate) fn create_outline_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("outline-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "outline-shader");
    // Keep these offsets in sync with `OutlineVertex` and shader.wgsl.
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
        array_stride: std::mem::size_of::<OutlineVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &vertex_attributes,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("outline-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_outline"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::LineList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            // The outline must be occluded by nearer terrain but must not stop
            // later geometry from drawing over it. wgpu forbids a depth bias on
            // line topology, so `vs_outline` nudges the clip-space depth instead;
            // see the comment there.
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_outline"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the depth texture and view for the given drawable size.
pub(crate) fn create_depth_view(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
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

#[cfg(test)]
mod tests {
    use crate::Camera;
    use crate::environment::Environment;
    use ob_math::{RenderVec, Vec4, WorldVec};

    use super::*;

    #[test]
    fn camera_uniform_matches_the_shader_layout() {
        assert_eq!(std::mem::size_of::<CameraUniform>(), 224);
        assert_eq!(std::mem::align_of::<CameraUniform>(), 16);
        assert_eq!(std::mem::offset_of!(CameraUniform, view_proj), 0);
        assert_eq!(std::mem::offset_of!(CameraUniform, camera_pos), 64);
        assert_eq!(std::mem::offset_of!(CameraUniform, fog), 80);
        assert_eq!(std::mem::offset_of!(CameraUniform, sun), 96);
        assert_eq!(std::mem::offset_of!(CameraUniform, sun_colour), 112);
        assert_eq!(std::mem::offset_of!(CameraUniform, zenith), 128);
        assert_eq!(std::mem::offset_of!(CameraUniform, horizon), 144);
        assert_eq!(std::mem::offset_of!(CameraUniform, basis_forward), 160);
        assert_eq!(std::mem::offset_of!(CameraUniform, basis_right), 176);
        assert_eq!(std::mem::offset_of!(CameraUniform, basis_up), 192);
        assert_eq!(std::mem::offset_of!(CameraUniform, world_light), 208);
    }

    #[test]
    fn camera_uniform_uses_the_camera_fog_range() {
        let mut camera = Camera::new(WorldVec::new(1.5, 70.0, -3.0), 0.0, 0.0);
        camera.far = 200.0;
        let uniform = CameraUniform::new(&camera, 16.0 / 9.0, Environment::default());
        assert_eq!(uniform.camera_pos, [1.5, 70.0, -3.0, 1.0]);
        let (start, end) = camera.fog_range();
        assert_eq!(uniform.fog, [start, end, 0.0, 0.0]);
        assert_eq!(uniform.basis_forward[3], (camera.fov_y * 0.5).tan());
        assert!((uniform.basis_right[3] - 16.0 / 9.0).abs() < 1e-6);
    }

    #[test]
    fn camera_basis_is_orthonormal_and_matches_the_view_ray() {
        let yaw = 0.7f32;
        let pitch = -0.35f32;
        let camera = Camera::new(WorldVec::new(3.0, 70.0, -4.0), yaw, pitch);
        let uniform = CameraUniform::new(&camera, 16.0 / 9.0, Environment::default());
        let forward = [
            uniform.basis_forward[0],
            uniform.basis_forward[1],
            uniform.basis_forward[2],
        ];
        let right = [uniform.basis_right[0], uniform.basis_right[1], uniform.basis_right[2]];
        let up = [uniform.basis_up[0], uniform.basis_up[1], uniform.basis_up[2]];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        for (name, vector) in [("forward", forward), ("right", right), ("up", up)] {
            let length = dot(vector, vector).sqrt();
            assert!((length - 1.0).abs() < 1e-5, "{name} is not unit: {length}");
        }
        assert!(dot(forward, right).abs() < 1e-5);
        assert!(dot(forward, up).abs() < 1e-5);
        assert!(dot(right, up).abs() < 1e-5);
        // The shader reconstructs the forward direction at NDC (0, 0).
        assert!((forward[0] - camera.forward()[0]).abs() < 1e-5);
        assert!((forward[1] - camera.forward()[1]).abs() < 1e-5);
        assert!((forward[2] - camera.forward()[2]).abs() < 1e-5);
        // Basis vectors follow the documented yaw convention (0 faces -Z).
        let level = Camera::new(WorldVec::new(0.0, 0.0, 0.0), 0.0, 0.0);
        assert_eq!(level.forward(), [0.0, 0.0, -1.0]);
        assert_eq!(level.right(), [1.0, 0.0, 0.0]);
        assert_eq!(level.up(), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn reconstructed_view_rays_match_the_projection() {
        let yaw = 0.7f32;
        let pitch = 0.25f32;
        let camera = Camera::new(WorldVec::new(1.0, 70.0, -2.0), yaw, pitch);
        let aspect = 16.0 / 9.0;
        let uniform = CameraUniform::new(&camera, aspect, Environment::default());
        let eye = camera.position.as_vec3();
        let forward = RenderVec::from_array([
            uniform.basis_forward[0],
            uniform.basis_forward[1],
            uniform.basis_forward[2],
        ]);
        let right = RenderVec::from_array([uniform.basis_right[0], uniform.basis_right[1], uniform.basis_right[2]]);
        let up = RenderVec::from_array([uniform.basis_up[0], uniform.basis_up[1], uniform.basis_up[2]]);
        let tangent = uniform.basis_forward[3];
        let view_projection = camera.view_projection(aspect);

        for (ndc_x, ndc_y) in [(-0.8f32, -0.5f32), (0.0, 0.0), (0.5, 0.9), (1.0, -1.0)] {
            // This is exactly the shader's `view_ray` reconstruction.
            let direction = (forward + right * (ndc_x * tangent * aspect) + up * (ndc_y * tangent)).normalize();
            let point = eye + direction * 100.0;
            let clip = view_projection * Vec4::new(point.x, point.y, point.z, 1.0);
            assert!(clip.w > 0.0);
            let ndc = clip.truncate() / clip.w;
            assert!(
                (ndc.x - ndc_x).abs() < 1e-4 && (ndc.y - ndc_y).abs() < 1e-4,
                "ndc ({ndc_x}, {ndc_y}) reconstructed as ({}, {})",
                ndc.x,
                ndc.y
            );
        }
    }

    #[test]
    fn uniform_palette_follows_the_environment() {
        let camera = Camera::new(WorldVec::new(0.0, 70.0, 0.0), 0.0, 0.0);
        let day = CameraUniform::new(&camera, 1.0, Environment::default());
        let day_visual = Environment::default().visual();
        assert_eq!(&day.horizon[..3], &day_visual.horizon);
        assert_eq!(&day.zenith[..3], &day_visual.zenith);
        assert_eq!(day.world_light, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(day.sky_clear().a, 1.0);

        let night = CameraUniform::new(&camera, 1.0, Environment::new(0.0));
        let night_visual = Environment::new(0.0).visual();
        assert_eq!(&night.horizon[..3], &night_visual.horizon);
        assert_eq!(&night.zenith[..3], &night_visual.zenith);
        assert_eq!(night.sun_colour[3], night_visual.star_visibility);
        assert!(night.world_light[0] < day.world_light[0] * 0.3);
        assert!(night.world_light[3] > 0.9);
        // Fog uses the horizon colour: the uniform stores exactly that value.
        assert_eq!(&night.horizon[..3], &night_visual.fog_color());
        assert!((night.sky_clear().r - f64::from(night_visual.horizon[0])).abs() < 1e-7);
    }

    #[test]
    fn outline_buffer_holds_twelve_edges() {
        assert_eq!(OUTLINE_BUFFER_SIZE, 24 * 28);
        assert_eq!(std::mem::size_of::<OutlineVertex>(), 28);
    }
}

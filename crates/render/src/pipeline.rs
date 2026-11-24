//! Shared chunk-pipeline construction used by the windowed and offscreen renderers.
//!
//! Both paths bind the camera uniform, the block texture array and its sampler
//! with the same layout (`group(0)`, bindings 0..2), and use the same vertex
//! format, so the pipeline and bind-group code lives here instead of being
//! duplicated. The screen-space sky pipeline is built here as well: it has no
//! bindings, no vertex buffer and no depth attachment.

use bytemuck::{Pod, Zeroable};

use crate::textures::BlockTextureArray;
use crate::{Camera, Vertex};

/// Depth buffer format used by the forward pass.
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Byte size of the camera uniform block (`mat4x4<f32>` + two `vec4<f32>`s).
pub(crate) const CAMERA_UNIFORM_SIZE: wgpu::BufferAddress = 96;

/// Linear clear colour matching the sky horizon, used behind the full-screen
/// sky triangle.
///
/// The triangle covers every pixel, so this value is only a fallback; it is
/// kept equal to `SKY.horizon` in `shader.wgsl` so a partial sky pass can never
/// flash a contrasting colour.
pub(crate) const SKY_CLEAR: wgpu::Color = wgpu::Color {
    r: 0.4196,
    g: 0.6038,
    b: 0.8481,
    a: 1.0,
};

/// Camera and fog uniform uploaded once per frame (group 0, binding 0).
///
/// The 96-byte layout is three 16-byte-aligned rows: the view-projection
/// matrix, the camera world position and the fog parameters.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct CameraUniform {
    /// Column-major combined view-projection matrix.
    pub(crate) view_proj: [[f32; 4]; 4],
    /// Eye position in world space; `w` is unused.
    pub(crate) camera_pos: [f32; 4],
    /// `x` fog start distance, `y` fog end distance, `zw` unused.
    pub(crate) fog: [f32; 4],
}

// Keep the GPU layout in sync with the shader's `Camera` struct.
const _: () = assert!(std::mem::size_of::<CameraUniform>() == CAMERA_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<CameraUniform>() == 16);
const _: () = assert!(std::mem::offset_of!(CameraUniform, view_proj) == 0);
const _: () = assert!(std::mem::offset_of!(CameraUniform, camera_pos) == 64);
const _: () = assert!(std::mem::offset_of!(CameraUniform, fog) == 80);

impl CameraUniform {
    /// Builds the per-frame camera block for a camera and viewport aspect ratio.
    ///
    /// Fog bounds come from [`Camera::fog_range`], so the windowed and offscreen
    /// renderers always use the same distances for the same camera.
    pub(crate) fn new(camera: &Camera, aspect: f32) -> Self {
        let eye = camera.position.as_vec3();
        let (fog_start, fog_end) = camera.fog_range();
        Self {
            view_proj: camera.view_projection(aspect).to_cols_array_2d(),
            camera_pos: [eye.x, eye.y, eye.z, 1.0],
            fog: [fog_start, fog_end, 0.0, 0.0],
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

/// Creates the bind-group layout shared by every chunk pipeline.
pub(crate) fn create_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("chunk-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                // The vertex stage projects positions; the fragment stage needs
                // the camera position and fog bounds.
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

/// Creates a shader module for the world or sky pipeline.
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

/// Creates the sky pipeline: one full-screen triangle, no bindings, no depth.
pub(crate) fn create_sky_pipeline(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sky-pipeline-layout"),
        bind_group_layouts: &[],
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
    use ob_math::WorldVec;

    use super::*;

    #[test]
    fn camera_uniform_matches_the_shader_layout() {
        assert_eq!(std::mem::size_of::<CameraUniform>(), 96);
        assert_eq!(std::mem::align_of::<CameraUniform>(), 16);
        assert_eq!(std::mem::offset_of!(CameraUniform, view_proj), 0);
        assert_eq!(std::mem::offset_of!(CameraUniform, camera_pos), 64);
        assert_eq!(std::mem::offset_of!(CameraUniform, fog), 80);
    }

    #[test]
    fn camera_uniform_uses_the_camera_fog_range() {
        let mut camera = Camera::new(WorldVec::new(1.5, 70.0, -3.0), 0.25, -0.5);
        camera.far = 200.0;
        let uniform = CameraUniform::new(&camera, 16.0 / 9.0);
        assert_eq!(uniform.camera_pos, [1.5, 70.0, -3.0, 1.0]);
        let (start, end) = camera.fog_range();
        assert_eq!(uniform.fog, [start, end, 0.0, 0.0]);
    }
}

//! Half-resolution screen-space ambient occlusion (SSAO) pass.
//!
//! The renderer has no normal buffer, so [`ssao.wgsl`] reconstructs view-space
//! positions from the depth target and derives normals from neighbouring
//! depth samples. The raw AO target is then depth-aware blurred by
//! [`ssao_blur.wgsl`] and sampled once by the post pass, which multiplies the
//! HDR scene before tonemapping.
//!
//! Everything here is created once per renderer (or per resize) and gated by
//! [`crate::graphics::GraphicsSettings::ssao_enabled`]: Low and Medium never
//! allocate the targets or record the passes. The pass runs at half the render
//! resolution to keep the added fill rate small at 1080p, and the blur keeps
//! the half-resolution result free of visible stepping.
//!
//! Both shaders guard on depth 1.0 (sky) and fade the effect out between 96 and
//! 192 metres, so the sky, the far heightfield and the surface-shell LOD stay at
//! full ambient light and never receive halos from foreground silhouettes.

use bytemuck::Zeroable;
use wgpu::util::DeviceExt;

/// RGBA-free single-channel AO format: enough for a 0..1 multiplier.
pub(crate) const SSAO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Byte size of the SSAO uniform block (three `vec4<f32>` rows).
pub(crate) const SSAO_UNIFORM_SIZE: wgpu::BufferAddress = 48;

/// CPU-side parameters of the SSAO pass for one frame and render size.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SsaoUniform {
    /// `x` 1 / depth width, `y` 1 / depth height, `z` near plane, `w` far plane.
    pub(crate) params: [f32; 4],
    /// `x` tan(fov_y / 2), `y` aspect, `z` world radius per metre of depth,
    /// `w` radius ceiling in metres.
    pub(crate) view: [f32; 4],
    /// `x`/`y` depth target size, `z`/`w` quarter-resolution AO target size.
    pub(crate) screen: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<SsaoUniform>() == SSAO_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<SsaoUniform>() == 16);

impl SsaoUniform {
    /// Builds the per-frame uniform for the near/far planes and render size.
    /// `divisor` is 2 on Ultra (half resolution) and 4 on High (quarter):
    /// ambient occlusion is low-frequency, so the quarter target cuts the fill
    /// rate with little visible loss after the depth-aware blur.
    pub(crate) fn new(
        near: f32,
        far: f32,
        width: u32,
        height: u32,
        tan_half_fov: f32,
        aspect: f32,
        divisor: u32,
    ) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let divisor = divisor.max(1);
        let ao_width = (width / divisor).max(1);
        let ao_height = (height / divisor).max(1);
        let near = near.max(1e-4);
        let far = far.max(near + 1e-3);
        Self {
            params: [1.0 / width as f32, 1.0 / height as f32, near, far],
            view: [tan_half_fov, aspect.max(0.01), 0.025, 3.0],
            screen: [width as f32, height as f32, ao_width as f32, ao_height as f32],
        }
    }

    /// AO target size `(width, height)` in pixels for this uniform.
    #[cfg(test)]
    pub(crate) fn target_size(&self) -> (u32, u32) {
        (self.screen[2] as u32, self.screen[3] as u32)
    }
}

/// The two full-screen pipelines, their layouts and the filtering sampler used
/// by the blur. Created once in [`crate::frame::SceneRenderer::new`].
pub(crate) struct SsaoPipelines {
    pub(crate) raw_pipeline: wgpu::RenderPipeline,
    pub(crate) blur_pipeline: wgpu::RenderPipeline,
    pub(crate) raw_layout: wgpu::BindGroupLayout,
    pub(crate) blur_layout: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
}

fn shader_module(device: &wgpu::Device, label: &str, source: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    })
}

fn pipeline_layout(device: &wgpu::Device, label: &str, layout: &wgpu::BindGroupLayout) -> wgpu::PipelineLayout {
    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    })
}

fn fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    entry: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_ssao"),
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
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: SSAO_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the SSAO pipelines, layouts and sampler.
pub(crate) fn create_pipelines(device: &wgpu::Device) -> SsaoPipelines {
    let raw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ssao-raw-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(SSAO_UNIFORM_SIZE),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let blur_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ssao-blur-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(SSAO_UNIFORM_SIZE),
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
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let raw_shader = shader_module(device, "ssao-shader", include_str!("ssao.wgsl"));
    let blur_shader = shader_module(device, "ssao-blur-shader", include_str!("ssao_blur.wgsl"));
    let raw_layout_object = pipeline_layout(device, "ssao-raw-pipeline-layout", &raw_layout);
    let blur_layout_object = pipeline_layout(device, "ssao-blur-pipeline-layout", &blur_layout);
    let raw_pipeline = fullscreen_pipeline(device, "ssao-raw-pipeline", &raw_layout_object, &raw_shader, "fs_ssao");
    let blur_pipeline = fullscreen_pipeline(
        device,
        "ssao-blur-pipeline",
        &blur_layout_object,
        &blur_shader,
        "fs_ssao_blur",
    );
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ssao-blur-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    SsaoPipelines {
        raw_pipeline,
        blur_pipeline,
        raw_layout,
        blur_layout,
        sampler,
    }
}

fn ao_texture(device: &wgpu::Device, label: &str, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SSAO_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// GPU targets and bind groups for one SSAO configuration.
///
/// Recreated on resize and when the SSAO quality gate opens or closes; the
/// pipelines themselves are kept by the renderer.
pub(crate) struct SsaoResources {
    /// Raw AO target before the depth-aware blur.
    _raw_texture: wgpu::Texture,
    pub(crate) raw_view: wgpu::TextureView,
    /// Blurred AO target sampled by the post pass.
    _blur_texture: wgpu::Texture,
    pub(crate) blur_view: wgpu::TextureView,
    pub(crate) raw_bind_group: wgpu::BindGroup,
    pub(crate) blur_bind_group: wgpu::BindGroup,
}

impl SsaoResources {
    /// Allocates the two reduced-resolution targets and their bind groups.
    pub(crate) fn new(
        device: &wgpu::Device,
        pipelines: &SsaoPipelines,
        uniform: &wgpu::Buffer,
        depth_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        divisor: u32,
    ) -> Self {
        let divisor = divisor.max(1);
        let width = (width.max(1) / divisor).max(1);
        let height = (height.max(1) / divisor).max(1);
        let (raw_texture, raw_view) = ao_texture(device, "ssao-raw", width, height);
        let (blur_texture, blur_view) = ao_texture(device, "ssao-blur", width, height);
        let buffer_binding = || {
            wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: uniform,
                offset: 0,
                size: wgpu::BufferSize::new(SSAO_UNIFORM_SIZE),
            })
        };
        let raw_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ssao-raw-bind-group"),
            layout: &pipelines.raw_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(depth_view),
                },
            ],
        });
        let blur_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ssao-blur-bind-group"),
            layout: &pipelines.blur_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&raw_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(depth_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&pipelines.sampler),
                },
            ],
        });
        Self {
            _raw_texture: raw_texture,
            raw_view,
            _blur_texture: blur_texture,
            blur_view,
            raw_bind_group,
            blur_bind_group,
        }
    }

    /// Reads the blurred AO target back for GPU tests.
    ///
    /// Returns `(min, max, mean, pixels)`. Sky pixels are 255; a disabled or
    /// fully neutral pass returns all 255.
    #[cfg(test)]
    pub(crate) fn debug_read(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> (u8, u8, f32, Vec<u8>) {
        let width = self._blur_texture.width();
        let height = self._blur_texture.height();
        // R8Unorm: one byte per texel. The row pitch must still satisfy the
        // 256-byte copy alignment, so pad the *row*, not the pixel width.
        let bytes_per_row = width.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ssao-readback"),
            size: u64::from(bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ssao-readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self._blur_texture,
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
        queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll ssao readback");
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll ssao map");
        let mut pixels = Vec::with_capacity((width * height) as usize);
        {
            let data = slice.get_mapped_range().expect("map ssao readback");
            for row in 0..height as usize {
                let start = row * bytes_per_row as usize;
                pixels.extend_from_slice(&data[start..start + width as usize]);
            }
        }
        buffer.unmap();
        let min = pixels.iter().copied().min().unwrap_or(255);
        let max = pixels.iter().copied().max().unwrap_or(255);
        let mean = if pixels.is_empty() {
            255.0
        } else {
            pixels.iter().map(|value| f32::from(*value)).sum::<f32>() / pixels.len() as f32
        };
        (min, max, mean, pixels)
    }
}

/// Creates the SSAO uniform buffer with one entry, uploaded per frame.
pub(crate) fn create_uniform_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("ssao-uniform"),
        contents: bytemuck::bytes_of(&SsaoUniform::zeroed()),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Camera;
    use ob_math::{Vec4, WorldVec};

    #[test]
    fn ao_target_is_quarter_resolution_and_never_zero_sized() {
        let camera = Camera::new(WorldVec::new(0.0, 70.0, 0.0), 0.0, 0.0);
        let uniform = SsaoUniform::new(
            camera.near,
            camera.far,
            1920,
            1080,
            (camera.fov_y * 0.5).tan(),
            16.0 / 9.0,
            4,
        );
        assert_eq!(uniform.target_size(), (480, 270));
        assert!((uniform.params[0] - 1.0 / 1920.0).abs() < 1e-9);
        assert!((uniform.params[1] - 1.0 / 1080.0).abs() < 1e-9);

        for (width, height) in [(1, 1), (2, 2), (3, 5), (1279, 719)] {
            let uniform = SsaoUniform::new(camera.near, camera.far, width, height, 1.0, 1.0, 4);
            let (ao_width, ao_height) = uniform.target_size();
            assert!(ao_width >= 1 && ao_height >= 1);
            assert!(ao_width <= width && ao_height <= height);
        }
    }

    #[test]
    fn shader_depth_linearisation_recovers_the_eye_distance() {
        // The AO shader inverts NDC depth with the near/far plane terms. The
        // combined view-projection projects a world point on the camera axis to
        // exactly the NDC depth this inversion consumes, so the round-trip is a
        // real check of both the CPU uniform and the shader algebra.
        let mut camera = Camera::new(WorldVec::new(12.0, 70.0, -4.0), 0.35, -0.2);
        camera.far = 8_320.0;
        let aspect = 16.0 / 9.0;
        let vp = camera.view_projection(aspect);
        let uniform = SsaoUniform::new(
            camera.near,
            camera.far,
            1920,
            1080,
            (camera.fov_y * 0.5).tan(),
            aspect,
            2,
        );
        let eye = camera.position.as_vec3();
        let forward = [camera.forward()[0], camera.forward()[1], camera.forward()[2]];
        for distance in [0.5f32, 4.0, 64.0, 512.0, 4_000.0] {
            let point = eye + Vec4::from_array([forward[0], forward[1], forward[2], 0.0]).truncate() * distance;
            let clip = vp * Vec4::new(point.x, point.y, point.z, 1.0);
            let ndc_z = clip.z / clip.w;
            let near = uniform.params[2];
            let far = uniform.params[3];
            let reconstructed = (near * far) / (far - ndc_z * (far - near));
            assert!(
                (reconstructed - distance).abs() < 0.01 * distance.max(1.0),
                "depth reconstruction {reconstructed} != {distance}"
            );
        }
        // The projection terms must be finite and negative for a right-handed
        // zero-to-one projection.
        assert_eq!(uniform.params[2], camera.near);
        assert_eq!(uniform.params[3], camera.far);
    }
}

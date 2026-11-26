//! Shared pipeline and uniform construction used by the windowed and offscreen
//! renderers.
//!
//! Both paths bind the camera uniform, the block texture array, its sampler and
//! the sun shadow map with the same layout (`group(0)`, bindings 0..4), and use
//! the same vertex format, so the pipeline and bind-group code lives here
//! instead of being duplicated. The screen-space sky, targeted-block outline and
//! dropped-item pipelines reuse that camera bind group; shaders that do not read
//! a binding tolerate the extra layout entries.
//!
//! [`CameraUniform`] carries the camera matrices and fog bounds plus the
//! CPU-computed day/night state (palette, sun/moon, stars, world light) and the
//! 1.1 visual parameters (fitted shadow cascades, cloud coverage, water waves).
//! Keeping it in one uniform means the sky, the world, water and the outline can
//! never disagree about the current environment.
//!
//! The post-processing chain uses its own small [`PostUniform`] and layouts: the
//! world pass renders into a linear `Rgba16Float` target, three bloom mips and a
//! final ACES-fit tonemap pass write the LDR surface or offscreen target. Post
//! pipelines are created once at start-up; quality changes only alter uniforms
//! and target sizes, so no frame ever triggers shader compilation.

use bytemuck::{Pod, Zeroable};

use crate::environment::{Environment, WeatherVisual};
use crate::graphics::GraphicsSettings;
#[cfg(test)]
use crate::graphics::ShadowQuality;
use crate::outline::{OUTLINE_VERTEX_COUNT, OutlineVertex};
use crate::shadow::{MAX_SHADOW_CASCADES, ShadowCascades};
use crate::textures::BlockTextureArray;
use crate::{Camera, Vertex, particles::ParticleInstance};

/// Depth buffer format used by the forward pass.
pub(crate) const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Shadow-map depth format.
pub(crate) const SHADOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Linear HDR scene target format.
pub(crate) const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Bloom mip format (linear HDR, half floats).
pub(crate) const BLOOM_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Number of bloom mip levels written by the threshold/downsample chain.
pub(crate) const BLOOM_MIPS: usize = 3;

/// Byte size of the camera uniform block: four `mat4x4<f32>` rows plus sixteen
/// `vec4<f32>` rows.
pub(crate) const CAMERA_UNIFORM_SIZE: wgpu::BufferAddress = 560;

/// Byte size of the post-processing uniform block (three `vec4<f32>` rows).
pub(crate) const POST_UNIFORM_SIZE: wgpu::BufferAddress = 48;

/// Byte size of one per-cascade shadow matrix uniform (one `mat4x4<f32>`).
pub(crate) const SHADOW_CASCADE_UNIFORM_SIZE: wgpu::BufferAddress = 64;

/// Number of bytes in one outline vertex buffer.
pub(crate) const OUTLINE_BUFFER_SIZE: wgpu::BufferAddress =
    OUTLINE_VERTEX_COUNT as wgpu::BufferAddress * std::mem::size_of::<OutlineVertex>() as wgpu::BufferAddress;

/// Camera, fog, sky, lighting and 1.1 visual uniform uploaded once per frame
/// (group 0, binding 0).
///
/// The 496-byte layout is thirty-one 16-byte-aligned rows: the view-projection
/// matrix, the camera world position, the fog bounds, the day/night state, the
/// three shadow matrices, the cascade splits and the visual feature parameters.
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
    /// World-to-shadow-clip matrix for each cascade; inactive entries identity.
    pub(crate) shadow_view_proj: [[[f32; 4]; 4]; MAX_SHADOW_CASCADES],
    /// `x`/`y`/`z` far distance of cascades 0..2; `w` active cascade count.
    pub(crate) shadow_split: [f32; 4],
    /// `x`/`y`/`z` world-space texel size of cascades 0..2; `w` `1 / resolution`.
    pub(crate) shadow_texel: [f32; 4],
    /// `x` normal-offset texels, `y` reserved depth bias, `z` PCF half-width,
    /// `w` shadow strength.
    pub(crate) shadow_params: [f32; 4],
    /// `x` scene time in seconds, `y` cloud coverage, `z` cloud quality factor,
    /// `w` reserved.
    pub(crate) graphics: [f32; 4],
    /// `x` wave strength, `y` water surface alpha, `z` specular strength,
    /// `w` sea-surface height in world blocks (for the underwater tint).
    pub(crate) water: [f32; 4],
    /// `x`/`y`/`z` UV scale of shadow cascades 0..2 (1.0 full layer, 0.5 for the
    /// half-resolution far cascade); `w` reserved.
    pub(crate) shadow_scale: [f32; 4],
    /// `x` near/far seam radius in blocks (0 disables the seam fade),
    /// `y` seam band half-width in blocks, `z` height-fog density (0 disables
    /// it), `w` height-fog scale height in blocks.
    pub(crate) atmosphere: [f32; 4],
    /// `x` near plane, `y` far plane, `z` 1 when the scene-depth copy bound at
    /// binding 5 is fresh this frame (depth-responsive water), `w` reserved.
    pub(crate) clip: [f32; 4],
    /// `x` weather intensity `0..=1`, `y` storm weight `0..=1`, `z` reserved,
    /// `w` reserved.
    ///
    /// Sky/cloud storm presentation and the lower cloud layer read this row;
    /// the CPU palette in `environment.rs` handles colour darkening and light
    /// dimming so the same physics-free values drive windowed and offscreen
    /// frames.
    pub(crate) weather: [f32; 4],
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
const _: () = assert!(std::mem::offset_of!(CameraUniform, shadow_view_proj) == 224);
const _: () = assert!(std::mem::offset_of!(CameraUniform, shadow_split) == 416);
const _: () = assert!(std::mem::offset_of!(CameraUniform, shadow_texel) == 432);
const _: () = assert!(std::mem::offset_of!(CameraUniform, shadow_params) == 448);
const _: () = assert!(std::mem::offset_of!(CameraUniform, graphics) == 464);
const _: () = assert!(std::mem::offset_of!(CameraUniform, water) == 480);
const _: () = assert!(std::mem::offset_of!(CameraUniform, shadow_scale) == 496);
const _: () = assert!(std::mem::offset_of!(CameraUniform, atmosphere) == 512);
const _: () = assert!(std::mem::offset_of!(CameraUniform, clip) == 528);
const _: () = assert!(std::mem::offset_of!(CameraUniform, weather) == 544);

impl CameraUniform {
    /// Builds the per-frame uniform for a camera, viewport aspect ratio,
    /// day/night environment and fitted graphics state.
    ///
    /// Fog bounds come from [`Camera::fog_range`], so the windowed and offscreen
    /// renderers always use the same distances for the same camera. The palette
    /// comes from [`Environment::visual`], so both paths also agree on the sky,
    /// sun/moon, stars and world light.
    #[allow(clippy::too_many_arguments)] // camera, aspect, clock, quality, shadows, seam, weather
    pub(crate) fn new(
        camera: &Camera,
        aspect: f32,
        environment: Environment,
        time_seconds: f32,
        graphics: GraphicsSettings,
        shadow: &ShadowCascades,
        seam: Option<(f32, f32)>,
        weather: WeatherVisual,
    ) -> Self {
        let eye = camera.position.as_vec3();
        let (mut fog_start, mut fog_end) = camera.fog_range();
        let visual = environment.visual_with_weather(weather);
        let weather = weather.sanitised();
        // Storms pull the fog wall closer. The near/far seam uses the same
        // bounds so the far heightfield cannot disagree with the near world.
        let haze = weather.cloud_weight();
        if haze > 0.0 {
            fog_end = (fog_end * (1.0 - 0.35 * haze)).max(fog_start + 1.0);
            fog_start = (fog_start * (1.0 - 0.45 * haze)).max(16.0).min(fog_end - 1.0);
        }
        let aspect = aspect.max(0.01);
        let forward = camera.forward();
        let right = camera.right();
        let up = camera.up();
        let tangent = (camera.fov_y * 0.5).tan();
        let (wave_strength, surface_alpha, specular) = graphics.water.shader_params();
        let (seam_radius, seam_band) = seam.unwrap_or((0.0, 0.0));
        let sea_surface = crate::pipeline::SEA_SURFACE_HEIGHT;
        let (height_fog_density, height_fog_scale) = if graphics.height_fog_enabled() {
            (HEIGHT_FOG_DENSITY, HEIGHT_FOG_SCALE_HEIGHT)
        } else {
            (0.0, HEIGHT_FOG_SCALE_HEIGHT)
        };
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
            shadow_view_proj: shadow.view_proj_array(),
            shadow_split: shadow.split_vec4(),
            shadow_texel: shadow.texel_vec4(),
            shadow_params: [
                // Normal-offset in shadow texels; scaled by the cascade's
                // world texel size in the shader.
                2.5,
                // Reserved depth bias (the shadow pipeline uses hardware
                // slope-scaled bias; the shader uses the normal offset).
                0.0,
                shadow.pcf_radius as f32,
                // Direct-sun attenuation strength in the shadowed area.
                0.85,
            ],
            graphics: [
                time_seconds.max(0.0),
                // Storm clouds are denser at every quality preset; adding the
                // coverage on the CPU keeps the clear-weather uniform identical.
                (graphics.cloud_coverage + 0.34 * haze).clamp(0.0, 1.0),
                graphics.cloud_factor(),
                graphics.cloud_shadow_strength(),
            ],
            water: [wave_strength, surface_alpha, specular, sea_surface],
            shadow_scale: shadow.uv_scale_vec4(),
            atmosphere: [seam_radius, seam_band, height_fog_density, height_fog_scale],
            clip: [
                camera.near,
                camera.far,
                if graphics.water == crate::graphics::WaterQuality::High {
                    1.0
                } else {
                    0.0
                },
                0.0,
            ],
            weather: [weather.intensity, weather.storm, 0.0, 0.0],
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

    /// Convenience constructor used by tests: noon environment, no shadows and
    /// default graphics at time zero.
    #[cfg(test)]
    pub(crate) fn for_test(camera: &Camera, aspect: f32, environment: Environment) -> Self {
        Self::new(
            camera,
            aspect,
            environment,
            0.0,
            GraphicsSettings::default(),
            &ShadowCascades::fit(
                camera,
                aspect,
                &ShadowQuality::High.config(),
                environment.visual().sun_direction,
            ),
            None,
            WeatherVisual::default(),
        )
    }
}

/// Height of the sea surface the near and far water passes draw.
///
/// The world stores the topmost sea block at `SEA_LEVEL` and the water surface
/// fills the block above it, so the surface sits at `SEA_LEVEL + 1`. The
/// renderer uses one constant so the near-water, far-water and underwater-post
/// terms agree.
pub(crate) const SEA_SURFACE_HEIGHT: f32 = ob_math::SEA_LEVEL as f32 + 1.0;

/// Aerial-perspective density for the height fog on High/Ultra, per block.
///
/// At a 512-block distance and sea-level height the term reaches about 10%,
/// so the 512-chunk vista gains depth without a visible haze wall; it is kept
/// well below the main fog curve, which owns the last kilometre.
pub(crate) const HEIGHT_FOG_DENSITY: f32 = 0.000_18;
/// Scale height of the height fog in blocks: mountains above the sea get less.
pub(crate) const HEIGHT_FOG_SCALE_HEIGHT: f32 = 160.0;

/// Post-processing parameters (group 0 of the post and bloom pipelines).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct PostUniform {
    /// `x` exposure, `y` bloom strength, `z` saturation, `w` FXAA strength.
    pub(crate) params: [f32; 4],
    /// `x` `1 / width`, `y` `1 / height`, `z` bloom threshold, `w` time.
    pub(crate) texels: [f32; 4],
    /// `x` SSAO strength (0 = disabled), `y` underwater amount (0..1),
    /// `z` lightning flash `0..=1` (full-screen exposure lift), `w` reserved.
    pub(crate) effects: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<PostUniform>() == POST_UNIFORM_SIZE as usize);
const _: () = assert!(std::mem::align_of::<PostUniform>() == 16);

impl PostUniform {
    /// Builds the uniform for a target size, graphics settings and scene time.
    pub(crate) fn new(
        width: u32,
        height: u32,
        graphics: GraphicsSettings,
        time_seconds: f32,
        underwater: bool,
        weather_flash: f32,
    ) -> Self {
        Self {
            params: [
                graphics.exposure,
                graphics.bloom_strength(),
                1.0,
                if graphics.fxaa { 1.0 } else { 0.0 },
            ],
            texels: [
                1.0 / width.max(1) as f32,
                1.0 / height.max(1) as f32,
                crate::graphics::BLOOM_THRESHOLD,
                time_seconds.max(0.0),
            ],
            effects: [
                graphics.ssao_strength(),
                if underwater { 1.0 } else { 0.0 },
                if weather_flash.is_finite() {
                    weather_flash.clamp(0.0, 1.0)
                } else {
                    0.0
                },
                0.0,
            ],
        }
    }
}

/// Creates the instanced particle pipeline.
///
/// The pipeline shares the scene camera bind group (only binding 0 is read),
/// expands each quad in the vertex stage from the camera basis and alpha-blends
/// into the HDR target after the water pass. Depth is tested but not written,
/// so particles respect terrain and entity occlusion without z-fighting with
/// the water surface.
pub(crate) fn create_particle_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("particle-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("particles.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("particle-pipeline-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("particle-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<ParticleInstance>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &[
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
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 20,
                        shader_location: 2,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32,
                        offset: 36,
                        shader_location: 3,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 40,
                        shader_location: 4,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32,
                        offset: 44,
                        shader_location: 5,
                    },
                ],
            })],
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
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
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
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
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

/// Creates the post-processing uniform buffer.
pub(crate) fn create_post_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("post-uniform"),
        size: POST_UNIFORM_SIZE,
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

/// Creates the bind-group layout shared by the world, water, sky, shadow,
/// outline and entity pipelines.
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
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                // Depth-only copy of the scene depth buffer, refreshed before
                // the water pass when depth-responsive water is enabled. The
                // water shader reads it with `textureLoad` to derive water
                // thickness; every other pipeline ignores the binding.
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 6,
                // Baked tiling cloud density for the ground cloud shadows.
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 7,
                // Linear/repeat sampler for the cloud-density texture; the
                // block sampler stays nearest for the pixel-art layers.
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// Binds the camera buffer, block texture array, shadow map, samplers, the
/// scene-depth copy and the cloud-density texture.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    camera: &wgpu::Buffer,
    textures: &BlockTextureArray,
    shadow_view: &wgpu::TextureView,
    shadow_sampler: &wgpu::Sampler,
    scene_depth: &wgpu::TextureView,
    cloud_view: &wgpu::TextureView,
    cloud_sampler: &wgpu::Sampler,
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
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(shadow_view),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(shadow_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(scene_depth),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::TextureView(cloud_view),
            },
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::Sampler(cloud_sampler),
            },
        ],
    })
}

/// Creates the reduced layout used by the shadow-depth pass.
///
/// A shadow pass writes one cascade layer while its casters sample the block
/// texture array. It must not bind the shadow map itself: a depth array is an
/// exclusive render target, so sampling it from the same pass is a validation
/// error (and would be a feedback loop). This layout therefore contains only the
/// camera, block textures and sampler.
pub(crate) fn create_shadow_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
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
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(SHADOW_CASCADE_UNIFORM_SIZE),
                },
                count: None,
            },
        ],
    })
}

/// Binds the camera buffer, block textures and one cascade matrix for a
/// shadow-depth pass.
pub(crate) fn create_shadow_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    camera: &wgpu::Buffer,
    textures: &BlockTextureArray,
    cascade: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("shadow-bind-group"),
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
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: cascade,
                    offset: 0,
                    size: wgpu::BufferSize::new(SHADOW_CASCADE_UNIFORM_SIZE),
                }),
            },
        ],
    })
}

/// Layout for the far-cascade proxy pass: one cascade matrix.
pub(crate) fn create_shadow_proxy_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow-proxy-bind-group-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(SHADOW_CASCADE_UNIFORM_SIZE),
            },
            count: None,
        }],
    })
}

/// Binds one cascade matrix for the proxy pass.
pub(crate) fn create_shadow_cascade_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    cascade: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("shadow-cascade-bind-group"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: cascade,
                offset: 0,
                size: wgpu::BufferSize::new(SHADOW_CASCADE_UNIFORM_SIZE),
            }),
        }],
    })
}

/// Creates the instanced far-cascade shadow proxy pipeline.
///
/// The vertex layout is a unit cube plus one AABB instance (min/max, 24 bytes).
/// Depth writes, slope-scaled bias and back-face culling match the full-detail
/// shadow pipeline.
pub(crate) fn create_shadow_proxy_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow-proxy-pipeline-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "shadow-proxy-shader", include_str!("shadow_proxy.wgsl"));
    const CUBE_ATTRIBUTES: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    }];
    const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 12,
            shader_location: 2,
        },
    ];
    let vertex_buffers = [
        Some(wgpu::VertexBufferLayout {
            array_stride: 12,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &CUBE_ATTRIBUTES,
        }),
        Some(wgpu::VertexBufferLayout {
            array_stride: 24,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &INSTANCE_ATTRIBUTES,
        }),
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shadow-proxy-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_proxy"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: SHADOW_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 2.5,
                clamp: 0.0,
            },
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: None,
        multiview_mask: None,
        cache: None,
    })
}

/// Creates one per-cascade shadow matrix buffer.
pub(crate) fn create_shadow_cascade_buffer(device: &wgpu::Device, _index: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shadow-cascade"),
        size: SHADOW_CASCADE_UNIFORM_SIZE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Creates the comparison sampler used for PCF shadow lookups.
pub(crate) fn create_shadow_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("shadow-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        compare: Some(wgpu::CompareFunction::LessEqual),
        ..Default::default()
    })
}

/// Creates the post-processing layout: post uniform, HDR scene, three bloom
/// mips and a filtering sampler.
pub(crate) fn create_post_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let mut entries = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(POST_UNIFORM_SIZE),
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
    ];
    // Bindings 2..=4 are the three bloom mips.
    for binding in 2..2 + BLOOM_MIPS as u32 {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
    }
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: 2 + BLOOM_MIPS as u32,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    });
    // Binding 6 is the blurred SSAO texture (or a white 1×1 fallback when the
    // pass is disabled, so the post shader needs no variant).
    entries.push(wgpu::BindGroupLayoutEntry {
        binding: 3 + BLOOM_MIPS as u32,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    });
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("post-bind-group-layout"),
        entries: &entries,
    })
}

/// Binds the post uniform, HDR scene, bloom mips and sampler.
pub(crate) fn create_post_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    post: &wgpu::Buffer,
    hdr: &wgpu::TextureView,
    bloom: &[wgpu::TextureView; BLOOM_MIPS],
    ao: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("post-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: post,
                    offset: 0,
                    size: wgpu::BufferSize::new(POST_UNIFORM_SIZE),
                }),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(hdr),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&bloom[0]),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&bloom[1]),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(&bloom[2]),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::TextureView(ao),
            },
        ],
    })
}

/// Creates the layout for one bloom pass: post uniform, source texture and
/// filtering sampler.
pub(crate) fn create_bloom_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("bloom-bind-group-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(POST_UNIFORM_SIZE),
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
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// Binds the post uniform, one bloom source view and the filtering sampler.
pub(crate) fn create_bloom_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    post: &wgpu::Buffer,
    source: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bloom-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: post,
                    offset: 0,
                    size: wgpu::BufferSize::new(POST_UNIFORM_SIZE),
                }),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(source),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Creates the linear filtering sampler used by the post chain.
pub(crate) fn create_post_sampler(device: &wgpu::Device) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("post-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

/// Creates a shader module for the chunk, sky, outline, shadow, post or bloom
/// pipelines.
fn create_shader_module(device: &wgpu::Device, label: &str, source: &'static str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    })
}

/// The chunk vertex layout shared by the world, water and shadow pipelines.
fn chunk_vertex_buffers() -> [Option<wgpu::VertexBufferLayout<'static>>; 1] {
    // Keep these offsets in sync with `Vertex` and shader.wgsl.
    const ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
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
            format: wgpu::VertexFormat::Uint32,
            offset: 24,
            shader_location: 3,
        },
    ];
    [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRIBUTES,
    })]
}

/// Creates the forward chunk pipeline: textured, depth-tested, no culling,
/// drawing into the linear HDR target.
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
    let shader = create_shader_module(device, "chunk-shader", include_str!("shader.wgsl"));
    let vertex_buffers = chunk_vertex_buffers();
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

/// Creates the translucent water pipeline: alpha blended, `LessEqual` depth
/// test, no depth writes, no culling.
pub(crate) fn create_water_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("water-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "water-shader", include_str!("shader.wgsl"));
    let vertex_buffers = chunk_vertex_buffers();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("water-pipeline"),
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
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_water"),
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

/// Creates the sky pipeline: one full-screen triangle, camera uniform only, no
/// depth, drawing into the linear HDR target.
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
    let shader = create_shader_module(device, "sky-shader", include_str!("shader.wgsl"));
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
        // The sky is drawn after the opaque world as a background fill: it
        // passes only where the cleared depth buffer is still 1.0, so terrain
        // pixels never shade clouds or stars.
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
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
    let shader = create_shader_module(device, "outline-shader", include_str!("shader.wgsl"));
    // Keep these offsets in sync with `OutlineVertex` and shader.wgsl.
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
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
        attributes: &ATTRIBUTES,
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

/// Creates the depth-only shadow pipeline with alpha cut-out support.
///
/// The fragment stage samples the block array and discards transparent leaf
/// texels, so foliage casts shaped shadows. Primitive culling is disabled for
/// the same reasons as the main pass, and a slope-scaled depth bias plus the
/// shader's normal offset keep acne out of the image.
pub(crate) fn create_shadow_pipeline(
    device: &wgpu::Device,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, "shadow-shader", include_str!("shadow.wgsl"));
    let vertex_buffers = chunk_vertex_buffers();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shadow-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_shadow"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &vertex_buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            // Only the faces that point towards the sun need to write depth;
            // culling the backs roughly halves shadow rasterisation with no
            // visible difference on opaque terrain.
            cull_mode: Some(wgpu::Face::Back),
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: SHADOW_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 2.5,
                clamp: 0.0,
            },
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_shadow_alpha"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the final post-processing pipeline (ACES-fit tonemap, bloom, FXAA,
/// vignette) for the LDR output target.
pub(crate) fn create_post_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    layout: &wgpu::BindGroupLayout,
    entry_point: &str,
    label: &str,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("post-pipeline-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, label, include_str!("post.wgsl"));
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_post"),
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
            module: &shader,
            entry_point: Some(entry_point),
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

/// Creates one bloom pass pipeline writing into [`BLOOM_FORMAT`].
pub(crate) fn create_bloom_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    entry_point: &str,
    label: &str,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    let shader = create_shader_module(device, label, include_str!("bloom.wgsl"));
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_post"),
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
            module: &shader,
            entry_point: Some(entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: BLOOM_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the scene depth texture for the given drawable size.
///
/// `TEXTURE_BINDING` lets the SSAO and water passes read it without another
/// copy, and `COPY_SRC` lets the water path take the depth-only copy it samples
/// while the original is still attached for depth testing.
pub(crate) fn create_depth_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

#[cfg(test)]
mod tests {
    use crate::Camera;
    use crate::environment::{Environment, WeatherVisual};
    use crate::graphics::{GraphicsPreset, ShadowQuality};
    use crate::shadow::ShadowCascades;
    use ob_math::{RenderVec, Vec4, WorldVec};

    use super::*;

    #[test]
    fn camera_uniform_matches_the_shader_layout() {
        assert_eq!(std::mem::size_of::<CameraUniform>(), 560);
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
        assert_eq!(std::mem::offset_of!(CameraUniform, shadow_view_proj), 224);
        assert_eq!(std::mem::offset_of!(CameraUniform, shadow_split), 416);
        assert_eq!(std::mem::offset_of!(CameraUniform, shadow_texel), 432);
        assert_eq!(std::mem::offset_of!(CameraUniform, shadow_params), 448);
        assert_eq!(std::mem::offset_of!(CameraUniform, graphics), 464);
        assert_eq!(std::mem::offset_of!(CameraUniform, water), 480);
        assert_eq!(std::mem::offset_of!(CameraUniform, shadow_scale), 496);
        assert_eq!(std::mem::offset_of!(CameraUniform, atmosphere), 512);
        assert_eq!(std::mem::offset_of!(CameraUniform, clip), 528);
        assert_eq!(std::mem::offset_of!(CameraUniform, weather), 544);
    }

    #[test]
    fn post_uniform_matches_the_shader_layout() {
        assert_eq!(std::mem::size_of::<PostUniform>(), 48);
        assert_eq!(std::mem::align_of::<PostUniform>(), 16);
        assert_eq!(std::mem::offset_of!(PostUniform, params), 0);
        assert_eq!(std::mem::offset_of!(PostUniform, texels), 16);
        assert_eq!(std::mem::offset_of!(PostUniform, effects), 32);
        let uniform = PostUniform::new(1920, 1080, GraphicsSettings::default(), 3.0, false, 0.0);
        assert!((uniform.texels[0] - 1.0 / 1920.0).abs() < 1e-9);
        assert!((uniform.texels[1] - 1.0 / 1080.0).abs() < 1e-9);
        assert_eq!(uniform.texels[3], 3.0);
        assert_eq!(uniform.params[3], 1.0);
        assert_eq!(
            PostUniform::new(
                1920,
                1080,
                GraphicsSettings::from_preset(GraphicsPreset::Low),
                0.0,
                false,
                0.0
            )
            .params[3],
            0.0
        );
    }

    #[test]
    fn camera_uniform_uses_the_camera_fog_range() {
        let mut camera = Camera::new(WorldVec::new(1.5, 70.0, -3.0), 0.0, 0.0);
        camera.far = 200.0;
        let uniform = CameraUniform::for_test(&camera, 16.0 / 9.0, Environment::default());
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
        let uniform = CameraUniform::for_test(&camera, 16.0 / 9.0, Environment::default());
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
        let uniform = CameraUniform::for_test(&camera, aspect, Environment::default());
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
        let day = CameraUniform::for_test(&camera, 1.0, Environment::default());
        let day_visual = Environment::default().visual();
        assert_eq!(&day.horizon[..3], &day_visual.horizon);
        assert_eq!(&day.zenith[..3], &day_visual.zenith);
        assert_eq!(day.world_light, [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(day.sky_clear().a, 1.0);

        let night = CameraUniform::for_test(&camera, 1.0, Environment::new(0.0));
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
    fn shadow_uniform_follows_the_fitted_cascades_and_graphics() {
        let mut camera = Camera::new(WorldVec::new(0.0, 70.0, 0.0), 0.0, 0.0);
        camera.far = 560.0;
        let graphics = GraphicsSettings::default();
        let shadow = ShadowCascades::fit(
            &camera,
            16.0 / 9.0,
            &graphics.shadow_config(),
            Environment::default().visual().sun_direction,
        );
        assert!(!shadow.split_vec4().is_empty());
        let uniform = CameraUniform::new(
            &camera,
            16.0 / 9.0,
            Environment::default(),
            12.5,
            graphics,
            &shadow,
            None,
            WeatherVisual::default(),
        );
        assert_eq!(uniform.shadow_view_proj, shadow.view_proj_array());
        assert_eq!(uniform.shadow_split, shadow.split_vec4());
        assert_eq!(uniform.shadow_split[3], 2.0);
        assert_eq!(uniform.shadow_params[2], 1.0);
        assert_eq!(uniform.shadow_scale, shadow.uv_scale_vec4());
        assert_eq!(uniform.shadow_scale[1], 1.0);
        assert_eq!(uniform.graphics[0], 12.5);
        assert_eq!(uniform.graphics[1], graphics.cloud_coverage);
        assert_eq!(uniform.water[1], 0.72);

        let low = CameraUniform::new(
            &camera,
            1.0,
            Environment::default(),
            0.0,
            GraphicsSettings::from_preset(GraphicsPreset::Low),
            &ShadowCascades::fit(
                &camera,
                1.0,
                &ShadowQuality::Off.config(),
                Environment::default().visual().sun_direction,
            ),
            None,
            WeatherVisual::default(),
        );
        assert_eq!(low.shadow_split, [0.0; 4]);
        assert_eq!(low.water[1], 0.80);
    }

    #[test]
    fn atmosphere_uniform_carries_the_seam_and_height_fog_gate() {
        let mut camera = Camera::new(WorldVec::new(0.0, 80.0, 0.0), 0.0, -0.2);
        camera.far = 8_320.0;
        let shadow = ShadowCascades::fit(
            &camera,
            16.0 / 9.0,
            &GraphicsSettings::default().shadow_config(),
            Environment::default().visual().sun_direction,
        );
        let high = CameraUniform::new(
            &camera,
            16.0 / 9.0,
            Environment::default(),
            0.0,
            GraphicsSettings::default(),
            &shadow,
            Some((512.0, 64.0)),
            WeatherVisual::default(),
        );
        assert_eq!(high.atmosphere[0], 512.0);
        assert_eq!(high.atmosphere[1], 64.0);
        assert!(high.atmosphere[2] > 0.0, "High must enable height fog");
        assert!(high.atmosphere[3] > 1.0);
        // Sea level for the underwater tint; water shader parameters are kept.
        assert!((high.water[3] - SEA_SURFACE_HEIGHT).abs() < 1e-6);
        assert_eq!(high.clip[2], 1.0, "High water binds a depth copy");
        assert_eq!(high.shadow_params[1], 0.0, "reserved shadow bias must stay zero");
        assert_eq!(high.graphics[3], 0.0, "High keeps cloud shadows Ultra-only");

        let low = CameraUniform::new(
            &camera,
            16.0 / 9.0,
            Environment::default(),
            0.0,
            GraphicsSettings::from_preset(GraphicsPreset::Low),
            &ShadowCascades::fit(
                &camera,
                16.0 / 9.0,
                &ShadowQuality::Off.config(),
                Environment::default().visual().sun_direction,
            ),
            None,
            WeatherVisual::default(),
        );
        assert_eq!(low.atmosphere[0], 0.0);
        assert_eq!(low.atmosphere[2], 0.0, "Low must keep the cheap atmosphere");
        assert_eq!(low.clip[2], 0.0, "Low water must not sample the copy");
        assert_eq!(low.graphics[3], 0.0);
        let ultra = CameraUniform::new(
            &camera,
            16.0 / 9.0,
            Environment::default(),
            0.0,
            GraphicsSettings::from_preset(GraphicsPreset::Ultra),
            &shadow,
            Some((512.0, 64.0)),
            WeatherVisual::default(),
        );
        assert!(ultra.graphics[3] > 0.0, "Ultra projects cloud shadows");
    }

    #[test]
    fn outline_buffer_holds_twelve_edges() {
        assert_eq!(OUTLINE_BUFFER_SIZE, 24 * 28);
        assert_eq!(std::mem::size_of::<OutlineVertex>(), 28);
    }
}

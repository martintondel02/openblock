//! Shared scene renderer: GpuMesh uploads, shadow cascades, HDR scene targets,
//! the water pass, bloom and the final post pass.
//!
//! `SceneRenderer` owns everything except the presentation surface (or the
//! offscreen readback texture) and the immediate-mode UI renderer. Both
//! [`crate::Renderer`] and [`crate::OffscreenRenderer`] delegate to it, which is
//! what guarantees that a preset renders identically in a window, in an F2
//! screenshot and in the headless benchmark.
//!
//! Per frame the renderer records:
//!
//! 1. one depth-only shadow pass per active sun cascade (opaque near sections,
//!    opaque far-LOD batches and dropped-item entities),
//! 2. the sky pass (gradient, clouds, sun/moon, stars) into a linear
//!    `Rgba16Float` target,
//! 3. the opaque world pass (sections, far batches, entities),
//! 4. the transparent water pass (back-to-front, depth-test only) and the
//!    targeted-block outline,
//! 5. the bloom threshold/downsample chain, then the ACES-fit post pass into
//!    the LDR surface or offscreen target.
//!
//! All pipelines are built once in [`SceneRenderer::new`]. A graphics quality
//! change only reallocates the shadow map and updates uniforms; a resize
//! reallocates the HDR/bloom/depth targets and their bind groups.

use std::collections::HashMap;

use ob_math::SectionPos;

use crate::entities::{EntitiesRenderer, EntityInstance};
use crate::far_terrain::{self, FarTerrainMesh, GpuFarTerrain};
use crate::frustum::{aabb_visible, aabb_within_distance, section_visible};
use crate::graphics::{GraphicsSettings, ShadowConfig};
use crate::lod::LodBatchKey;
use crate::pipeline;
use crate::shadow::{MAX_SHADOW_CASCADES, ShadowCascades};
use crate::textures::BlockTextureArray;
use crate::{BlockOutline, Camera, MeshData, OUTLINE_VERTEX_COUNT, RenderError, RenderStats, SceneParams, Vertex};

/// GPU resources for one section mesh.
///
/// The opaque and water vertex lists of [`MeshData`] are uploaded into separate
/// buffers: the opaque pass draws `buffer`, the translucent water pass draws
/// `water_buffer`, and the shadow pass only draws the opaque buffer.
#[derive(Debug)]
pub(crate) struct GpuMesh {
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) vertex_count: u32,
    pub(crate) water_buffer: Option<wgpu::Buffer>,
    pub(crate) water_vertex_count: u32,
    pub(crate) min: [f32; 3],
    pub(crate) max: [f32; 3],
}

impl GpuMesh {
    fn from_mesh(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        min: [f32; 3],
        max: [f32; 3],
        mesh: &MeshData,
    ) -> Option<Self> {
        // A section may contain only water (an ocean surface section). It still
        // needs a valid opaque buffer handle for the struct; its opaque draw is
        // skipped because `vertex_count == 0`.
        let (buffer, vertex_count) = upload_vertices(device, queue, label, &mesh.vertices)
            .unwrap_or_else(|| (empty_vertex_buffer(device, label), 0));
        let (water_buffer, water_vertex_count) = upload_vertices(device, queue, label, &mesh.water_vertices)
            .map_or((None, 0), |(buffer, count)| (Some(buffer), count));
        Some(Self {
            buffer,
            vertex_count,
            water_buffer,
            water_vertex_count,
            min,
            max,
        })
    }
}

/// GPU resources for one merged far-LOD batch.
#[derive(Debug)]
pub(crate) struct GpuLodBatch {
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) vertex_count: u32,
    pub(crate) water_buffer: Option<wgpu::Buffer>,
    pub(crate) water_vertex_count: u32,
    pub(crate) min: [f32; 3],
    pub(crate) max: [f32; 3],
}

impl GpuLodBatch {
    fn from_mesh(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        min: [f32; 3],
        max: [f32; 3],
        mesh: &MeshData,
    ) -> Option<Self> {
        // A far batch can be entirely water (an ocean shell); it still needs a
        // valid opaque buffer because the batch is uploaded as a unit. When one
        // list is empty its buffer is `None` and the corresponding pass skips it.
        let (buffer, vertex_count) = upload_vertices(device, queue, "lod-batch", &mesh.vertices)
            .unwrap_or_else(|| (empty_vertex_buffer(device, "lod-batch"), 0));
        let (water_buffer, water_vertex_count) =
            upload_vertices(device, queue, "lod-batch-water", &mesh.water_vertices)
                .map_or((None, 0), |(buffer, count)| (Some(buffer), count));
        if vertex_count == 0 && water_vertex_count == 0 {
            return None;
        }
        Some(Self {
            buffer,
            vertex_count,
            water_buffer,
            water_vertex_count,
            min,
            max,
        })
    }

    fn total_vertex_count(&self) -> u64 {
        u64::from(self.vertex_count) + u64::from(self.water_vertex_count)
    }
}

fn upload_vertices(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    vertices: &[Vertex],
) -> Option<(wgpu::Buffer, u32)> {
    if vertices.is_empty() {
        return None;
    }
    let size = std::mem::size_of_val(vertices) as wgpu::BufferAddress;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytemuck::cast_slice(vertices));
    Some((buffer, vertices.len() as u32))
}

/// A one-vertex placeholder so a water-only far batch still has a valid opaque
/// buffer handle. The batch's opaque draw is skipped when `vertex_count == 0`.
fn empty_vertex_buffer(device: &wgpu::Device, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        usage: wgpu::BufferUsages::VERTEX,
        mapped_at_creation: false,
    })
}

/// Shadow-map texture and per-cascade render views.
struct ShadowTargets {
    /// Kept alive for the views.
    _texture: wgpu::Texture,
    /// Complete depth array sampled by the world/water/sky shaders.
    view: wgpu::TextureView,
    /// One single-layer view per cascade, used as the shadow render target.
    layer_views: [Option<wgpu::TextureView>; MAX_SHADOW_CASCADES],
    /// Configuration the map was allocated for (read by the debug readback and
    /// to keep the allocated size and the reported size in one place).
    config: ShadowConfig,
}

impl ShadowTargets {
    fn new(device: &wgpu::Device, config: ShadowConfig) -> Self {
        let layers = config.cascades.max(1);
        let resolution = config.resolution.max(1);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(match config.quality {
                crate::graphics::ShadowQuality::Off => "sun-shadow-map-off",
                crate::graphics::ShadowQuality::Low => "sun-shadow-map-low",
                crate::graphics::ShadowQuality::High => "sun-shadow-map-high",
                crate::graphics::ShadowQuality::Ultra => "sun-shadow-map-ultra",
            }),
            size: wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: pipeline::SHADOW_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Always expose a D2Array view: with a single-layer fallback (shadows
        // off) the default view dimension would be D2 and group 0's depth-array
        // binding would be invalid.
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("sun-shadow-map-array"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let layer_views = std::array::from_fn(|index| {
            (index < config.cascades as usize).then(|| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("sun-shadow-cascade"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: index as u32,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
        });
        Self {
            _texture: texture,
            view,
            layer_views,
            config,
        }
    }
}

/// HDR colour, depth and bloom targets plus all size-dependent bind groups.
struct Targets {
    width: u32,
    height: u32,
    _hdr_texture: wgpu::Texture,
    hdr_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    _bloom_texture: wgpu::Texture,
    bloom_views: [wgpu::TextureView; pipeline::BLOOM_MIPS],
    post_bind_group: wgpu::BindGroup,
    /// `[threshold, downsample 1, downsample 2]` bind groups. The threshold pass
    /// reads the scene HDR texture; the two downsample passes read the previous
    /// bloom mip.
    bloom_bind_groups: [wgpu::BindGroup; pipeline::BLOOM_MIPS],
    shadow: ShadowTargets,
}

impl Targets {
    #[allow(clippy::too_many_arguments)]
    fn new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        graphics: GraphicsSettings,
        post_buffer: &wgpu::Buffer,
        post_sampler: &wgpu::Sampler,
        post_layout: &wgpu::BindGroupLayout,
        bloom_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let hdr_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("hdr-scene"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: pipeline::HDR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let hdr_view = hdr_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("hdr-scene-view"),
            ..Default::default()
        });
        let depth_view = pipeline::create_depth_view(device, width, height);
        let bloom_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bloom-chain"),
            size: wgpu::Extent3d {
                // At least 8x8 so the fixed three-mip chain always has three
                // valid levels, even for a 1x1 window or test target.
                width: (width / 2).max(8),
                height: (height / 2).max(8),
                depth_or_array_layers: 1,
            },
            mip_level_count: pipeline::BLOOM_MIPS as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: pipeline::BLOOM_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let bloom_views: [wgpu::TextureView; pipeline::BLOOM_MIPS] = std::array::from_fn(|level| {
            bloom_texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("bloom-mip"),
                base_mip_level: level as u32,
                mip_level_count: Some(1),
                ..Default::default()
            })
        });
        let post_bind_group =
            pipeline::create_post_bind_group(device, post_layout, post_buffer, &hdr_view, &bloom_views, post_sampler);
        let bloom_bind_groups = std::array::from_fn(|level| {
            let source = if level == 0 { &hdr_view } else { &bloom_views[level - 1] };
            pipeline::create_bloom_bind_group(device, bloom_layout, post_buffer, source, post_sampler)
        });
        Self {
            width,
            height,
            _hdr_texture: hdr_texture,
            hdr_view,
            depth_view,
            _bloom_texture: bloom_texture,
            bloom_views,
            post_bind_group,
            bloom_bind_groups,
            shadow: ShadowTargets::new(device, graphics.shadow_config()),
        }
    }
}

/// Instanced unit cube used by the far-cascade proxy casters (36 vertices, CCW
/// outside, coordinates 0 or 1 so instances can be an AABB min/max).
const PROXY_CUBE_VERTICES: [[f32; 3]; 36] = [
    // +Y
    [0.0, 1.0, 0.0],
    [1.0, 1.0, 0.0],
    [1.0, 1.0, 1.0],
    [0.0, 1.0, 0.0],
    [1.0, 1.0, 1.0],
    [0.0, 1.0, 1.0],
    // -Y
    [0.0, 0.0, 0.0],
    [0.0, 0.0, 1.0],
    [1.0, 0.0, 1.0],
    [0.0, 0.0, 0.0],
    [1.0, 0.0, 1.0],
    [1.0, 0.0, 0.0],
    // +X
    [1.0, 0.0, 0.0],
    [1.0, 0.0, 1.0],
    [1.0, 1.0, 1.0],
    [1.0, 0.0, 0.0],
    [1.0, 1.0, 1.0],
    [1.0, 1.0, 0.0],
    // -X
    [0.0, 0.0, 1.0],
    [0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 1.0, 0.0],
    [0.0, 1.0, 1.0],
    // +Z
    [1.0, 0.0, 1.0],
    [0.0, 0.0, 1.0],
    [0.0, 1.0, 1.0],
    [1.0, 0.0, 1.0],
    [0.0, 1.0, 1.0],
    [1.0, 1.0, 1.0],
    // -Z
    [0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
    [1.0, 1.0, 0.0],
    [0.0, 0.0, 0.0],
    [1.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
];

/// Per-instance AABB for the far shadow proxy pass (24 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ShadowProxyInstance {
    minimum: [f32; 3],
    maximum: [f32; 3],
}

/// Instanced far-cascade shadow proxies.
///
/// One draw per cascade replaces the per-section draws for AABBs beyond the
/// near-cascade detail limit. The instance buffer is rebuilt every frame from
/// the camera/sun culling result; it is tiny (24 bytes × a few hundred sections).
struct ShadowProxies {
    pipeline: wgpu::RenderPipeline,
    cube_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    capacity: usize,
    instances: Vec<ShadowProxyInstance>,
    bind_groups: [wgpu::BindGroup; MAX_SHADOW_CASCADES],
}

impl ShadowProxies {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, cascade_buffers: &[wgpu::Buffer; MAX_SHADOW_CASCADES]) -> Self {
        let layout = pipeline::create_shadow_proxy_bind_group_layout(device);
        let pipeline = pipeline::create_shadow_proxy_pipeline(device, &layout);
        let cube_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow-proxy-cube"),
            size: std::mem::size_of_val(&PROXY_CUBE_VERTICES) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&cube_buffer, 0, bytemuck::cast_slice(&PROXY_CUBE_VERTICES));
        let capacity = 256;
        let instance_buffer = create_proxy_instance_buffer(device, capacity);
        let bind_groups = std::array::from_fn(|index| {
            pipeline::create_shadow_cascade_bind_group(device, &layout, &cascade_buffers[index])
        });
        Self {
            pipeline,
            cube_buffer,
            instance_buffer,
            capacity,
            instances: Vec::new(),
            bind_groups,
        }
    }

    /// Rebuilds the instance buffer and returns the vertex-instance range for
    /// each cascade.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lists: &[Vec<[f32; 6]>; MAX_SHADOW_CASCADES],
    ) -> [std::ops::Range<u32>; MAX_SHADOW_CASCADES] {
        self.instances.clear();
        let mut ranges = std::array::from_fn(|_| 0u32..0u32);
        for (index, list) in lists.iter().enumerate() {
            let start = self.instances.len() as u32;
            self.instances.extend(list.iter().map(|bounds| ShadowProxyInstance {
                minimum: [bounds[0], bounds[1], bounds[2]],
                maximum: [bounds[3], bounds[4], bounds[5]],
            }));
            ranges[index] = start..self.instances.len() as u32;
        }
        if self.instances.len() > self.capacity {
            self.capacity = self.instances.len().next_power_of_two();
            self.instance_buffer = create_proxy_instance_buffer(device, self.capacity);
        }
        if !self.instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&self.instances));
        }
        ranges
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, cascade: usize, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_groups[cascade], &[]);
        pass.set_vertex_buffer(0, self.cube_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..PROXY_CUBE_VERTICES.len() as u32, range);
    }
}

fn create_proxy_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shadow-proxy-instances"),
        size: (capacity.max(1) * std::mem::size_of::<ShadowProxyInstance>()) as wgpu::BufferAddress,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Water draw key; water is sorted back-to-front across sections and batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WaterKey {
    Batch(LodBatchKey),
    Section(SectionPos),
}

/// The shared scene renderer.
pub(crate) struct SceneRenderer {
    pub(crate) graphics: GraphicsSettings,
    pub(crate) textures: BlockTextureArray,
    camera_buffer: wgpu::Buffer,
    post_buffer: wgpu::Buffer,
    outline_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    shadow_bind_group_layout: wgpu::BindGroupLayout,
    shadow_cascade_buffers: [wgpu::Buffer; MAX_SHADOW_CASCADES],
    shadow_bind_groups: [wgpu::BindGroup; MAX_SHADOW_CASCADES],
    shadow_proxies: ShadowProxies,
    shadow_sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    water_pipeline: wgpu::RenderPipeline,
    far_pipeline: wgpu::RenderPipeline,
    far_water_pipeline: wgpu::RenderPipeline,
    sky_pipeline: wgpu::RenderPipeline,
    outline_pipeline: wgpu::RenderPipeline,
    shadow_pipeline: wgpu::RenderPipeline,
    post_layout: wgpu::BindGroupLayout,
    post_pipeline: wgpu::RenderPipeline,
    post_pipeline_simple: wgpu::RenderPipeline,
    bloom_layout: wgpu::BindGroupLayout,
    bloom_threshold_pipeline: wgpu::RenderPipeline,
    bloom_down_pipeline: wgpu::RenderPipeline,
    post_sampler: wgpu::Sampler,
    targets: Targets,
    entities: EntitiesRenderer,
    meshes: HashMap<SectionPos, GpuMesh>,
    lod_batches: HashMap<LodBatchKey, GpuLodBatch>,
    far_terrain: Option<GpuFarTerrain>,
    stats: RenderStats,
}

impl SceneRenderer {
    /// Creates every pipeline and target for one output format.
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        output_format: wgpu::TextureFormat,
        source: &crate::TextureSource,
        graphics: GraphicsSettings,
    ) -> Result<Self, RenderError> {
        let graphics = graphics.normalised();
        let textures = BlockTextureArray::new_with_source(device, queue, source)?;
        let camera_buffer = pipeline::create_camera_buffer(device);
        let post_buffer = pipeline::create_post_buffer(device);
        let outline_buffer = pipeline::create_outline_buffer(device);
        let post_layout = pipeline::create_post_bind_group_layout(device);
        let bloom_layout = pipeline::create_bloom_bind_group_layout(device);
        let post_sampler = pipeline::create_post_sampler(device);
        let targets = Targets::new(
            device,
            width,
            height,
            graphics,
            &post_buffer,
            &post_sampler,
            &post_layout,
            &bloom_layout,
        );
        let bind_group_layout = pipeline::create_bind_group_layout(device);
        let shadow_bind_group_layout = pipeline::create_shadow_bind_group_layout(device);
        let shadow_sampler = pipeline::create_shadow_sampler(device);
        let bind_group = pipeline::create_bind_group(
            device,
            &bind_group_layout,
            &camera_buffer,
            &textures,
            &targets.shadow.view,
            &shadow_sampler,
        );
        let shadow_cascade_buffers = std::array::from_fn(|index| pipeline::create_shadow_cascade_buffer(device, index));
        let shadow_bind_groups = std::array::from_fn(|index| {
            pipeline::create_shadow_bind_group(
                device,
                &shadow_bind_group_layout,
                &camera_buffer,
                &textures,
                &shadow_cascade_buffers[index],
            )
        });
        let shadow_proxies = ShadowProxies::new(device, queue, &shadow_cascade_buffers);
        let world_pipeline = pipeline::create_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let water_pipeline = pipeline::create_water_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let far_pipeline = far_terrain::create_far_terrain_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let far_water_pipeline =
            far_terrain::create_far_water_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let sky_pipeline = pipeline::create_sky_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let outline_pipeline = pipeline::create_outline_pipeline(device, pipeline::HDR_FORMAT, &bind_group_layout);
        let shadow_pipeline = pipeline::create_shadow_pipeline(device, &shadow_bind_group_layout);
        let post_pipeline =
            pipeline::create_post_pipeline(device, output_format, &post_layout, "fs_post", "post-pipeline");
        let post_pipeline_simple = pipeline::create_post_pipeline(
            device,
            output_format,
            &post_layout,
            "fs_post_simple",
            "post-pipeline-simple",
        );
        let bloom_threshold_pipeline =
            pipeline::create_bloom_pipeline(device, &bloom_layout, "fs_bloom_threshold", "bloom-threshold-pipeline");
        let bloom_down_pipeline =
            pipeline::create_bloom_pipeline(device, &bloom_layout, "fs_bloom_down", "bloom-downsample-pipeline");
        let entities = EntitiesRenderer::new(
            device,
            queue,
            pipeline::HDR_FORMAT,
            &bind_group_layout,
            &shadow_bind_group_layout,
        );
        Ok(Self {
            graphics,
            textures,
            camera_buffer,
            post_buffer,
            outline_buffer,
            bind_group_layout,
            bind_group,
            shadow_bind_group_layout,
            shadow_cascade_buffers,
            shadow_bind_groups,
            shadow_proxies,
            shadow_sampler,
            pipeline: world_pipeline,
            water_pipeline,
            far_pipeline,
            far_water_pipeline,
            sky_pipeline,
            outline_pipeline,
            shadow_pipeline,
            post_layout,
            post_pipeline,
            post_pipeline_simple,
            bloom_layout,
            bloom_threshold_pipeline,
            bloom_down_pipeline,
            post_sampler,
            targets,
            entities,
            meshes: HashMap::new(),
            lod_batches: HashMap::new(),
            far_terrain: None,
            stats: RenderStats::default(),
        })
    }

    /// Current drawable size.
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.targets.width, self.targets.height)
    }

    /// Reallocates the HDR, depth and bloom targets after a resize.
    pub(crate) fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if (width, height) == (self.targets.width, self.targets.height) {
            return;
        }
        // A resize reallocates the HDR, depth and bloom targets, including their
        // bind groups. The shadow map is sized independently of the output and
        // is reallocated too; resizes are rare and correctness is worth the
        // simpler ownership.
        self.targets = Targets::new(
            device,
            width,
            height,
            self.graphics,
            &self.post_buffer,
            &self.post_sampler,
            &self.post_layout,
            &self.bloom_layout,
        );
        self.bind_group = pipeline::create_bind_group(
            device,
            &self.bind_group_layout,
            &self.camera_buffer,
            &self.textures,
            &self.targets.shadow.view,
            &self.shadow_sampler,
        );
        self.shadow_bind_groups = std::array::from_fn(|index| {
            pipeline::create_shadow_bind_group(
                device,
                &self.shadow_bind_group_layout,
                &self.camera_buffer,
                &self.textures,
                &self.shadow_cascade_buffers[index],
            )
        });
    }

    /// Applies a new visual quality, reallocating the shadow map when its
    /// resolution or cascade count changed. No shader is recompiled.
    pub(crate) fn set_graphics(&mut self, device: &wgpu::Device, graphics: GraphicsSettings) {
        let graphics = graphics.normalised();
        let previous = self.graphics.shadow_config();
        self.graphics = graphics;
        let next = graphics.shadow_config();
        if previous != next {
            self.targets.shadow = ShadowTargets::new(device, next);
            self.bind_group = pipeline::create_bind_group(
                device,
                &self.bind_group_layout,
                &self.camera_buffer,
                &self.textures,
                &self.targets.shadow.view,
                &self.shadow_sampler,
            );
        }
    }

    /// Replaces the block texture array and rebuilds the scene bind group.
    pub(crate) fn set_texture_source(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: crate::TextureSource,
    ) -> Result<(), RenderError> {
        if self.textures.source() == &source {
            return Ok(());
        }
        let textures = BlockTextureArray::new_with_source(device, queue, &source)?;
        self.bind_group = pipeline::create_bind_group(
            device,
            &self.bind_group_layout,
            &self.camera_buffer,
            &textures,
            &self.targets.shadow.view,
            &self.shadow_sampler,
        );
        self.shadow_bind_groups = std::array::from_fn(|index| {
            pipeline::create_shadow_bind_group(
                device,
                &self.shadow_bind_group_layout,
                &self.camera_buffer,
                &textures,
                &self.shadow_cascade_buffers[index],
            )
        });
        self.textures = textures;
        Ok(())
    }

    /// Uploads or replaces one section mesh.
    pub(crate) fn upload_mesh(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pos: SectionPos, mesh: MeshData) {
        if mesh.is_empty() {
            self.meshes.remove(&pos);
            return;
        }
        let (min, max) = crate::section_bounds(pos);
        if let Some(gpu) = GpuMesh::from_mesh(device, queue, "section-mesh", min, max, &mesh) {
            self.meshes.insert(pos, gpu);
        }
    }

    /// Removes a section mesh.
    pub(crate) fn remove_mesh(&mut self, pos: SectionPos) {
        self.meshes.remove(&pos);
    }

    /// Removes every section mesh.
    pub(crate) fn clear_meshes(&mut self) {
        self.meshes.clear();
    }

    /// Number of uploaded section meshes.
    pub(crate) fn mesh_count(&self) -> usize {
        self.meshes.len()
    }

    /// Uploads or replaces one merged far-LOD batch.
    pub(crate) fn upload_lod_batch(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: LodBatchKey,
        min: [f32; 3],
        max: [f32; 3],
        mesh: MeshData,
    ) {
        if mesh.is_empty() {
            self.lod_batches.remove(&key);
            return;
        }
        if let Some(gpu) = GpuLodBatch::from_mesh(device, queue, min, max, &mesh) {
            self.lod_batches.insert(key, gpu);
        }
    }

    /// Removes one far-LOD batch.
    pub(crate) fn remove_lod_batch(&mut self, key: LodBatchKey) {
        self.lod_batches.remove(&key);
    }

    /// Removes every far-LOD batch.
    pub(crate) fn clear_lod_batches(&mut self) {
        self.lod_batches.clear();
    }

    /// Number of uploaded far-LOD batches.
    pub(crate) fn lod_batch_count(&self) -> usize {
        self.lod_batches.len()
    }

    /// Total vertices held by uploaded far-LOD batches (opaque and water).
    pub(crate) fn lod_vertex_count(&self) -> u64 {
        self.lod_batches.values().map(GpuLodBatch::total_vertex_count).sum()
    }

    /// Uploads or clears the visual-only far-terrain heightfield.
    pub(crate) fn set_far_terrain(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, mesh: FarTerrainMesh) {
        self.far_terrain = GpuFarTerrain::from_mesh(device, queue, &mesh);
    }

    /// Removes the far-terrain mesh.
    pub(crate) fn clear_far_terrain(&mut self) {
        self.far_terrain = None;
    }

    /// World-space bounds of the current far mesh.
    pub(crate) fn far_terrain_bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        self.far_terrain.as_ref().map(|far| (far.min, far.max))
    }

    /// Replaces the dropped-item entities drawn with the world.
    pub(crate) fn set_entities(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, entities: &[EntityInstance]) {
        self.entities.set(device, queue, entities);
    }

    /// Statistics from the last recorded frame.
    pub(crate) fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Debug helper: reads back the first shadow cascade and reports
    /// `(min depth, max depth, texels written)`.
    ///
    /// A cleared map is all 1.0; casters write smaller values. Used by the GPU
    /// shadow regression test to distinguish "no casters" from "wrong sampling".
    #[cfg(test)]
    pub(crate) fn debug_shadow_depth(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<(f32, f32, usize)> {
        let resolution = self.targets.shadow.config.resolution;
        if resolution <= 1 || self.targets.shadow.config.cascades == 0 {
            return None;
        }
        let bytes_per_row = resolution * 4;
        let buffer_size = u64::from(bytes_per_row) * u64::from(resolution);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow-debug-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("shadow-debug"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.targets.shadow._texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: 0 },
                aspect: wgpu::TextureAspect::DepthOnly,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(resolution),
                },
            },
            wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let mapped: std::sync::Arc<std::sync::Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        {
            let mapped = std::sync::Arc::clone(&mapped);
            slice.map_async(wgpu::MapMode::Read, move |result| {
                if let Ok(mut slot) = mapped.lock() {
                    *slot = Some(result);
                }
            });
        }
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let mapped_result = pollster::block_on(std::future::poll_fn(|_| {
            let mut slot = mapped.lock().expect("shadow debug map state poisoned");
            match slot.take() {
                Some(result) => std::task::Poll::Ready(result),
                None => std::task::Poll::Pending,
            }
        }));
        if mapped_result.is_err() {
            return None;
        }
        let mut min = 1.0f32;
        let mut max = 0.0f32;
        let mut written = 0usize;
        {
            let data = slice.get_mapped_range().ok()?;
            for chunk in data.as_chunks::<4>().0 {
                let depth = f32::from_le_bytes(*chunk);
                min = min.min(depth);
                max = max.max(depth);
                if depth < 0.999_999 {
                    written += 1;
                }
            }
        }
        readback.unmap();
        Some((min, max, written))
    }

    /// Short description of the shadow settings for F3.
    pub(crate) fn shadow_summary(&self) -> String {
        // Report the allocated map rather than a freshly derived config; they
        // are kept in sync by `set_graphics`, and reading the field proves it.
        let config = self.targets.shadow.config;
        match config.quality {
            crate::graphics::ShadowQuality::Off => "off".to_string(),
            quality => format!(
                "{} {}² ×{} ({}×{} PCF near)",
                quality.name(),
                config.resolution,
                config.cascades,
                config.pcf_radius * 2 + 1,
                config.pcf_radius * 2 + 1
            ),
        }
    }

    /// Records the complete frame into `encoder` and returns its counters.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output_view: &wgpu::TextureView,
        camera: &Camera,
        scene: &SceneParams<'_>,
    ) -> RenderStats {
        let (width, height) = self.size();
        let aspect = width as f32 / height as f32;
        let visual = scene.environment.visual();
        let shadow = ShadowCascades::fit(camera, aspect, &self.graphics.shadow_config(), visual.sun_direction);
        let uniform = pipeline::CameraUniform::new(
            camera,
            aspect,
            scene.environment,
            scene.time_seconds,
            self.graphics,
            &shadow,
        );
        queue.write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
        let post = pipeline::PostUniform::new(width, height, self.graphics, scene.time_seconds);
        queue.write_buffer(&self.post_buffer, 0, bytemuck::bytes_of(&post));

        let outline_vertices = scene
            .outline
            .filter(BlockOutline::is_finite)
            .map(|outline| outline.vertices());
        if let Some(vertices) = &outline_vertices {
            queue.write_buffer(&self.outline_buffer, 0, bytemuck::cast_slice(vertices));
        }

        let frustum = camera.frustum_planes(aspect);
        let eye = [
            camera.position.x as f32,
            camera.position.y as f32,
            camera.position.z as f32,
        ];
        let draw_distance = camera.far + 24.0;

        let mut stats = RenderStats {
            shadow_cascades: shadow.count as u32,
            shadow_resolution: if shadow.is_enabled() { shadow.resolution } else { 0 },
            ..RenderStats::default()
        };

        // Deterministic submission order regardless of hash-map iteration.
        let mut order: Vec<SectionPos> = self.meshes.keys().copied().collect();
        order.sort_unstable();
        let mut lod_order: Vec<LodBatchKey> = self.lod_batches.keys().copied().collect();
        lod_order.sort_unstable();

        // --- 1. Sun shadow cascades -----------------------------------------
        self.record_shadows(device, queue, encoder, &shadow, &order, &lod_order, eye, &mut stats);

        // --- 2. Opaque world -----------------------------------------------
        // The HDR target is cleared to the horizon (the sky pass overwrites it
        // wherever the depth buffer is still clear) and depth is cleared here.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets.hdr_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(uniform.sky_clear()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth_view,
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
            for pos in &order {
                let Some(mesh) = self.meshes.get(pos) else {
                    continue;
                };
                if !section_visible(&frustum, *pos) || !aabb_within_distance(eye, mesh.min, mesh.max, draw_distance) {
                    stats.culled_sections += 1;
                    continue;
                }
                if mesh.vertex_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, mesh.buffer.slice(..));
                pass.draw(0..mesh.vertex_count, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(mesh.vertex_count);
                stats.sections += 1;
            }
            for key in &lod_order {
                let Some(batch) = self.lod_batches.get(key) else {
                    continue;
                };
                if !aabb_visible(&frustum, batch.min, batch.max)
                    || !aabb_within_distance(eye, batch.min, batch.max, draw_distance)
                {
                    stats.culled_lod_batches += 1;
                    continue;
                }
                if batch.vertex_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, batch.buffer.slice(..));
                pass.draw(0..batch.vertex_count, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(batch.vertex_count);
                stats.lod_batches += 1;
                stats.lod_vertices += u64::from(batch.vertex_count);
            }
            // Visual-only far terrain is drawn after the near world and far
            // batches, so early depth rejection removes every far fragment
            // hidden behind full-detail geometry. It is not part of the shadow
            // pass and never influences simulation or streaming.
            if let Some(far) = &self.far_terrain
                && far.index_count > 0
                && aabb_visible(&frustum, far.min, far.max)
                && aabb_within_distance(eye, far.min, far.max, draw_distance)
            {
                pass.set_pipeline(&self.far_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, far.vertex_buffer.slice(..));
                pass.set_index_buffer(far.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..far.index_count, 0, 0..1);
                stats.draw_calls += 1;
                stats.vertices += u64::from(far.vertex_count);
                stats.far_draw_calls += 1;
                stats.far_vertices += u64::from(far.vertex_count);
                stats.far_triangles += u64::from(far.index_count / 3);
            }
            self.entities.draw(&mut pass, &self.bind_group);
            stats.passes += 1;
        }

        // --- 3. Sky background fill ----------------------------------------
        // Depth `LessEqual` at z = 1.0: only the still-clear background pixels
        // run the expensive cloud/star shader, never terrain.
        {
            let mut sky = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sky"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets.hdr_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            sky.set_pipeline(&self.sky_pipeline);
            sky.set_bind_group(0, &self.bind_group, &[]);
            sky.draw(0..3, 0..1);
            stats.passes += 1;
        }

        // --- 4. Water and outline ------------------------------------------
        let water = self.collect_water(&frustum, &order, &lod_order, eye, draw_distance);
        let far_water = self.far_terrain.as_ref().filter(|far| {
            far.water_index_count > 0
                && aabb_visible(&frustum, far.min, far.max)
                && aabb_within_distance(eye, far.min, far.max, draw_distance)
        });
        if !water.is_empty() || far_water.is_some() || outline_vertices.is_some() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("water"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets.hdr_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.targets.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(far) = far_water {
                // The far water surface is farther than every near section or
                // batch, so drawing it before the back-to-front near water
                // keeps the transparent pass visually ordered without a sort.
                pass.set_pipeline(&self.far_water_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                if let (Some(vertex_buffer), Some(index_buffer)) = (&far.water_vertex_buffer, &far.water_index_buffer) {
                    pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..far.water_index_count, 0, 0..1);
                    stats.water_draw_calls += 1;
                    stats.water_vertices += u64::from(far.water_vertex_count);
                    stats.far_water_draw_calls += 1;
                    stats.far_water_vertices += u64::from(far.water_vertex_count);
                }
            }
            if !water.is_empty() {
                pass.set_pipeline(&self.water_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                for (_, key) in &water {
                    let (buffer, count, min, max) = match key {
                        WaterKey::Section(pos) => {
                            let Some(mesh) = self.meshes.get(pos) else {
                                continue;
                            };
                            let Some(buffer) = &mesh.water_buffer else {
                                continue;
                            };
                            (buffer, mesh.water_vertex_count, mesh.min, mesh.max)
                        }
                        WaterKey::Batch(key) => {
                            let Some(batch) = self.lod_batches.get(key) else {
                                continue;
                            };
                            let Some(buffer) = &batch.water_buffer else {
                                continue;
                            };
                            (buffer, batch.water_vertex_count, batch.min, batch.max)
                        }
                    };
                    let _ = (min, max);
                    if count == 0 {
                        continue;
                    }
                    pass.set_vertex_buffer(0, buffer.slice(..));
                    pass.draw(0..count, 0..1);
                    stats.water_draw_calls += 1;
                    stats.water_vertices += u64::from(count);
                }
            }
            if outline_vertices.is_some() {
                pass.set_pipeline(&self.outline_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.outline_buffer.slice(..));
                pass.draw(0..OUTLINE_VERTEX_COUNT as u32, 0..1);
            }
            stats.passes += 1;
        }

        // --- 5. Bloom and post --------------------------------------------
        if self.graphics.bloom {
            for level in 0..pipeline::BLOOM_MIPS {
                let (pipeline, bind_group) = if level == 0 {
                    (&self.bloom_threshold_pipeline, &self.targets.bloom_bind_groups[0])
                } else {
                    (&self.bloom_down_pipeline, &self.targets.bloom_bind_groups[level])
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("bloom"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.targets.bloom_views[level],
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, bind_group, &[]);
                pass.draw(0..3, 0..1);
                stats.bloom_passes += 1;
                stats.passes += 1;
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("post"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.graphics.bloom || self.graphics.fxaa {
                pass.set_pipeline(&self.post_pipeline);
            } else {
                pass.set_pipeline(&self.post_pipeline_simple);
            }
            pass.set_bind_group(0, &self.targets.post_bind_group, &[]);
            pass.draw(0..3, 0..1);
            stats.passes += 1;
        }

        let _ = device;
        self.stats = stats;
        stats
    }

    /// Records one depth-only pass per active shadow cascade.
    #[allow(clippy::too_many_arguments)]
    ///
    /// The nearest cascade draws full-detail section meshes so tree and terrain
    /// shadows stay crisp near the player. Further out (and for the whole far
    /// cascade) one instanced AABB per section replaces hundreds of per-section
    /// draws: at 64 m and beyond the blockier silhouette is hidden by PCF and fog,
    /// and the draw-count reduction is what keeps 512 blocks playable.
    fn record_shadows(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        shadow: &ShadowCascades,
        order: &[SectionPos],
        lod_order: &[LodBatchKey],
        eye: [f32; 3],
        stats: &mut RenderStats,
    ) {
        if !shadow.is_enabled() {
            return;
        }
        let mut real_sections: [Vec<SectionPos>; MAX_SHADOW_CASCADES] = std::array::from_fn(|_| Vec::new());
        let mut real_batches: [Vec<LodBatchKey>; MAX_SHADOW_CASCADES] = std::array::from_fn(|_| Vec::new());
        let mut proxy_bounds: [Vec<[f32; 6]>; MAX_SHADOW_CASCADES] = std::array::from_fn(|_| Vec::new());

        for cascade_index in 0..shadow.count {
            let cascade = shadow.cascades[cascade_index];
            let max_distance = cascade.split + 24.0;
            // Cascade 0 always draws real geometry. Later cascades switch to
            // proxies beyond a detail limit: 64 m for the middle cascade, and the
            // whole far cascade for three-cascade quality.
            let real_limit = match (shadow.count, cascade_index) {
                (_, 0) => f32::INFINITY,
                (3, 1) | (2, 1) => 64.0,
                _ => 0.0,
            };
            for pos in order {
                let Some(mesh) = self.meshes.get(pos) else {
                    continue;
                };
                if mesh.vertex_count == 0
                    || !aabb_within_distance(eye, mesh.min, mesh.max, max_distance)
                    || !shadow.visible_in(cascade_index, mesh.min, mesh.max)
                {
                    continue;
                }
                if aabb_within_distance(eye, mesh.min, mesh.max, real_limit) {
                    real_sections[cascade_index].push(*pos);
                } else {
                    proxy_bounds[cascade_index].push([
                        mesh.min[0],
                        mesh.min[1],
                        mesh.min[2],
                        mesh.max[0],
                        mesh.max[1],
                        mesh.max[2],
                    ]);
                }
            }
            for key in lod_order {
                let Some(batch) = self.lod_batches.get(key) else {
                    continue;
                };
                if batch.vertex_count == 0
                    || !aabb_within_distance(eye, batch.min, batch.max, max_distance)
                    || !shadow.visible_in(cascade_index, batch.min, batch.max)
                {
                    continue;
                }
                if aabb_within_distance(eye, batch.min, batch.max, real_limit) {
                    real_batches[cascade_index].push(*key);
                } else {
                    proxy_bounds[cascade_index].push([
                        batch.min[0],
                        batch.min[1],
                        batch.min[2],
                        batch.max[0],
                        batch.max[1],
                        batch.max[2],
                    ]);
                }
            }
        }
        let proxy_ranges = self.shadow_proxies.upload(device, queue, &proxy_bounds);

        for cascade_index in 0..shadow.count {
            let Some(layer_view) = self.targets.shadow.layer_views[cascade_index].as_ref() else {
                continue;
            };
            let cascade = shadow.cascades[cascade_index];
            // One 64-byte buffer and bind group per cascade: the passes are all
            // recorded before submission, so a single shared buffer would let
            // the last write leak into every pass.
            queue.write_buffer(
                &self.shadow_cascade_buffers[cascade_index],
                0,
                bytemuck::bytes_of(&cascade.view_proj.to_cols_array_2d()),
            );
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sun-shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: layer_view,
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
            // The far cascade renders into the lower-left quarter of its layer;
            // the shader scales its UVs by 0.5. Clearing still covers the whole
            // layer, which is harmless.
            pass.set_viewport(
                0.0,
                0.0,
                cascade.render_resolution as f32,
                cascade.render_resolution as f32,
                0.0,
                1.0,
            );
            pass.set_pipeline(&self.shadow_pipeline);
            pass.set_bind_group(0, &self.shadow_bind_groups[cascade_index], &[]);
            for pos in &real_sections[cascade_index] {
                let Some(mesh) = self.meshes.get(pos) else {
                    continue;
                };
                pass.set_vertex_buffer(0, mesh.buffer.slice(..));
                pass.draw(0..mesh.vertex_count, 0..1);
                stats.shadow_draw_calls += 1;
            }
            for key in &real_batches[cascade_index] {
                let Some(batch) = self.lod_batches.get(key) else {
                    continue;
                };
                pass.set_vertex_buffer(0, batch.buffer.slice(..));
                pass.draw(0..batch.vertex_count, 0..1);
                stats.shadow_draw_calls += 1;
            }
            let proxy_range = proxy_ranges[cascade_index].clone();
            if !proxy_range.is_empty() {
                self.shadow_proxies.draw(&mut pass, cascade_index, proxy_range);
                stats.shadow_draw_calls += 1;
            }
            self.entities
                .draw_shadow(&mut pass, &self.shadow_bind_groups[cascade_index]);
            stats.passes += 1;
        }
    }

    /// Collects every visible water buffer, sorted far-to-near.
    fn collect_water(
        &self,
        frustum: &[[f32; 4]; crate::FRUSTUM_PLANE_COUNT],
        order: &[SectionPos],
        lod_order: &[LodBatchKey],
        eye: [f32; 3],
        draw_distance: f32,
    ) -> Vec<(f32, WaterKey)> {
        let mut water = Vec::new();
        let centre_distance_sq = |min: [f32; 3], max: [f32; 3]| {
            let cx = (min[0] + max[0]) * 0.5 - eye[0];
            let cy = (min[1] + max[1]) * 0.5 - eye[1];
            let cz = (min[2] + max[2]) * 0.5 - eye[2];
            cx * cx + cy * cy + cz * cz
        };
        for pos in order {
            let Some(mesh) = self.meshes.get(pos) else {
                continue;
            };
            if mesh.water_vertex_count == 0
                || !section_visible(frustum, *pos)
                || !aabb_within_distance(eye, mesh.min, mesh.max, draw_distance)
            {
                continue;
            }
            water.push((centre_distance_sq(mesh.min, mesh.max), WaterKey::Section(*pos)));
        }
        for key in lod_order {
            let Some(batch) = self.lod_batches.get(key) else {
                continue;
            };
            if batch.water_vertex_count == 0
                || !aabb_visible(frustum, batch.min, batch.max)
                || !aabb_within_distance(eye, batch.min, batch.max, draw_distance)
            {
                continue;
            }
            water.push((centre_distance_sq(batch.min, batch.max), WaterKey::Batch(*key)));
        }
        // Back-to-front for stable alpha compositing; keys break ties so the
        // submitted command stream is deterministic.
        water.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        water
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::GraphicsPreset;

    #[test]
    fn gpu_mesh_vertex_totals_include_water() {
        // Without a device there is nothing to upload, but the empty-state and
        // buffer bookkeeping rules are testable through the helpers' contract.
        let graphics = GraphicsSettings::from_preset(GraphicsPreset::Low);
        assert_eq!(graphics.shadow_config().cascades, 0);
        let mesh = MeshData::default();
        assert!(mesh.is_empty());
        assert_eq!(mesh.vertex_count(), 0);
        assert_eq!(mesh.water_vertex_count(), 0);
    }

    #[test]
    fn water_keys_are_ordered_deterministically() {
        let a = WaterKey::Section(SectionPos::new(0, 0, 0));
        let b = WaterKey::Section(SectionPos::new(1, 0, 0));
        let c = WaterKey::Batch(LodBatchKey::default());
        assert!(a < b);
        assert_ne!(a, c);
        let mut keys = [b, c, a];
        keys.sort();
        assert_eq!(keys, [c, a, b]);
    }
}

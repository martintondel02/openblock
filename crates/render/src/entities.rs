//! Dropped-item entities: small textured or flat-tinted cubes in the world.
//!
//! The client describes each dropped item with an [`EntityInstance`] (base-centre
//! position, edge length, yaw, texture layer, tint). [`EntitiesRenderer`] packs a
//! slice of them into one instanced vertex buffer and draws a unit cube per
//! instance after the world geometry. Block items sample the shared block texture
//! array; non-block items (apple, stick, tools) use a flat sRGB tint.
//!
//! The pipeline reuses the camera and block-texture bind group (`group(0)`,
//! bindings 0..2) built by the render pipeline module, so no extra uniform or texture
//! binding is needed. It depth-tests with `LessEqual`, writes depth, disables
//! culling and blending, and leaves the world/sky/UI pipelines untouched.
//!
//! Instances are drawn in the order they were supplied. An empty slice issues no
//! draw call; the internal instance buffer grows on demand and persists between
//! frames.

use bytemuck::{Pod, Zeroable};
use ob_content::{ItemId, Registry};

use crate::pipeline::DEPTH_FORMAT;

/// Number of vertices in the shared unit-cube mesh (six triangles per face).
pub const CUBE_VERTEX_COUNT: u32 = 36;

/// Initial per-instance vertex-buffer capacity.
const INITIAL_INSTANCE_CAPACITY: usize = 64;

/// One dropped item/entity drawn as a small cube in the world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntityInstance {
    /// Feet/base centre in world space (metres).
    pub position: [f32; 3],
    /// Cube edge length in metres (e.g. 0.25).
    pub scale: f32,
    /// Yaw spin in radians.
    pub yaw: f32,
    /// Texture-array layer for block items; ignored when `textured == false`.
    pub layer: u32,
    /// Flat or multiplicative tint (sRGB 0..=1). For untextured items this is
    /// the cube colour; for textured blocks use `[1, 1, 1]`.
    pub tint: [f32; 3],
    /// Whether to sample the block texture array or draw a flat tinted cube.
    pub textured: bool,
}

impl EntityInstance {
    /// Scale used by [`EntityInstance::default`] (a quarter-metre cube).
    pub const DEFAULT_SCALE: f32 = 0.25;

    /// A textured block-item cube at `position` with edge length `scale`.
    pub const fn block(position: [f32; 3], scale: f32, layer: u32) -> Self {
        Self {
            position,
            scale,
            yaw: 0.0,
            layer,
            tint: [1.0, 1.0, 1.0],
            textured: true,
        }
    }

    /// An untextured non-block item cube with an sRGB tint.
    pub const fn flat(position: [f32; 3], scale: f32, tint: [f32; 3]) -> Self {
        Self {
            position,
            scale,
            yaw: 0.0,
            layer: 0,
            tint,
            textured: false,
        }
    }
}

impl Default for EntityInstance {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            scale: Self::DEFAULT_SCALE,
            yaw: 0.0,
            layer: 0,
            tint: [1.0, 1.0, 1.0],
            textured: true,
        }
    }
}

/// Texture-array layer for a dropped item, or `None` for non-block items.
///
/// Block items use their side texture layer from the content registry, so
/// planks/cobblestone map to layers 14/15 and a crafting table uses its front
/// (side) layer 17. Apple, sticks and tools have no block and return `None`.
pub fn entity_layer(registry: &Registry, item: ItemId) -> Option<u32> {
    let block = registry.item(item).block()?;
    Some(u32::from(registry.block(block).textures.side.0))
}

/// Dense per-item texture layers indexed by [`ItemId`].
///
/// `None` entries are flat-tinted items. The vector length equals
/// [`Registry::item_len`] and follows the same id order as [`Registry::items`].
pub fn entity_layers(registry: &Registry) -> Vec<Option<u32>> {
    (0..registry.item_len())
        .map(|index| entity_layer(registry, ItemId(index as u16)))
        .collect()
}

/// Per-instance GPU vertex data (40 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct EntityRaw {
    position: [f32; 3],
    scale: f32,
    yaw: f32,
    layer: u32,
    tint: [f32; 3],
    textured: u32,
}

// Keep the instance layout and the shader attribute offsets in sync.
const _: () = assert!(std::mem::size_of::<EntityRaw>() == 40);

impl From<EntityInstance> for EntityRaw {
    fn from(entity: EntityInstance) -> Self {
        Self {
            position: entity.position,
            scale: entity.scale,
            yaw: entity.yaw,
            layer: entity.layer,
            tint: entity.tint,
            textured: u32::from(entity.textured),
        }
    }
}

/// One unit-cube vertex: base-centred position, per-face UV and baked shade.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CubeVertex {
    position: [f32; 3],
    uv: [f32; 2],
    shade: f32,
}

const _: () = assert!(std::mem::size_of::<CubeVertex>() == 24);

const fn cube_vertex(position: [f32; 3], uv: [f32; 2], shade: f32) -> CubeVertex {
    CubeVertex { position, uv, shade }
}

// The cube spans x/z -0.5..=0.5 and y 0..=1 so `position` is the base centre.
// Face shades fake a light from above/right; the pipeline has no lighting.
const CUBE_VERTICES: [CubeVertex; CUBE_VERTEX_COUNT as usize] = [
    // +Y (top)
    cube_vertex([-0.5, 1.0, -0.5], [0.0, 0.0], 1.00),
    cube_vertex([0.5, 1.0, -0.5], [1.0, 0.0], 1.00),
    cube_vertex([0.5, 1.0, 0.5], [1.0, 1.0], 1.00),
    cube_vertex([-0.5, 1.0, -0.5], [0.0, 0.0], 1.00),
    cube_vertex([0.5, 1.0, 0.5], [1.0, 1.0], 1.00),
    cube_vertex([-0.5, 1.0, 0.5], [0.0, 1.0], 1.00),
    // -Y (bottom)
    cube_vertex([-0.5, 0.0, -0.5], [0.0, 0.0], 0.45),
    cube_vertex([-0.5, 0.0, 0.5], [0.0, 1.0], 0.45),
    cube_vertex([0.5, 0.0, 0.5], [1.0, 1.0], 0.45),
    cube_vertex([-0.5, 0.0, -0.5], [0.0, 0.0], 0.45),
    cube_vertex([0.5, 0.0, 0.5], [1.0, 1.0], 0.45),
    cube_vertex([0.5, 0.0, -0.5], [1.0, 0.0], 0.45),
    // +X
    cube_vertex([0.5, 0.0, -0.5], [0.0, 0.0], 0.84),
    cube_vertex([0.5, 0.0, 0.5], [1.0, 0.0], 0.84),
    cube_vertex([0.5, 1.0, 0.5], [1.0, 1.0], 0.84),
    cube_vertex([0.5, 0.0, -0.5], [0.0, 0.0], 0.84),
    cube_vertex([0.5, 1.0, 0.5], [1.0, 1.0], 0.84),
    cube_vertex([0.5, 1.0, -0.5], [0.0, 1.0], 0.84),
    // -X
    cube_vertex([-0.5, 0.0, 0.5], [0.0, 0.0], 0.66),
    cube_vertex([-0.5, 0.0, -0.5], [1.0, 0.0], 0.66),
    cube_vertex([-0.5, 1.0, -0.5], [1.0, 1.0], 0.66),
    cube_vertex([-0.5, 0.0, 0.5], [0.0, 0.0], 0.66),
    cube_vertex([-0.5, 1.0, -0.5], [1.0, 1.0], 0.66),
    cube_vertex([-0.5, 1.0, 0.5], [0.0, 1.0], 0.66),
    // +Z
    cube_vertex([0.5, 0.0, 0.5], [0.0, 0.0], 0.76),
    cube_vertex([-0.5, 0.0, 0.5], [1.0, 0.0], 0.76),
    cube_vertex([-0.5, 1.0, 0.5], [1.0, 1.0], 0.76),
    cube_vertex([0.5, 0.0, 0.5], [0.0, 0.0], 0.76),
    cube_vertex([-0.5, 1.0, 0.5], [1.0, 1.0], 0.76),
    cube_vertex([0.5, 1.0, 0.5], [0.0, 1.0], 0.76),
    // -Z
    cube_vertex([-0.5, 0.0, -0.5], [0.0, 0.0], 0.92),
    cube_vertex([0.5, 0.0, -0.5], [1.0, 0.0], 0.92),
    cube_vertex([0.5, 1.0, -0.5], [1.0, 1.0], 0.92),
    cube_vertex([-0.5, 0.0, -0.5], [0.0, 0.0], 0.92),
    cube_vertex([0.5, 1.0, -0.5], [1.0, 1.0], 0.92),
    cube_vertex([-0.5, 1.0, -0.5], [0.0, 1.0], 0.92),
];

/// The entity pipeline and its persistent vertex/instance buffers.
pub struct EntitiesRenderer {
    pipeline: wgpu::RenderPipeline,
    /// Depth-only variant used by the sun shadow cascades.
    shadow_pipeline: wgpu::RenderPipeline,
    cube_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
    instances: Vec<EntityInstance>,
    gpu_instances: Vec<EntityRaw>,
}

impl EntitiesRenderer {
    /// Creates the entity pipeline, the unit-cube buffer and an empty instance
    /// buffer. `bind_group_layout` is the shared camera/texture layout.
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
        bind_group_layout: &wgpu::BindGroupLayout,
        shadow_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("entity-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("entity.wgsl").into()),
        });

        let cube_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("entity-cube-vertices"),
            size: std::mem::size_of_val(&CUBE_VERTICES) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&cube_buffer, 0, bytemuck::cast_slice(&CUBE_VERTICES));

        let instance_buffer = create_instance_buffer(device, INITIAL_INSTANCE_CAPACITY);

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("entity-pipeline-layout"),
            bind_group_layouts: &[Some(bind_group_layout)],
            immediate_size: 0,
        });
        let shadow_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("entity-shadow-pipeline-layout"),
            bind_group_layouts: &[Some(shadow_bind_group_layout)],
            immediate_size: 0,
        });
        // Keep these offsets in sync with `CubeVertex`, `EntityRaw` and entity.wgsl.
        let cube_attributes = [
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
                format: wgpu::VertexFormat::Float32,
                offset: 20,
                shader_location: 2,
            },
        ];
        let instance_attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 12,
                shader_location: 4,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 16,
                shader_location: 5,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 20,
                shader_location: 6,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 24,
                shader_location: 7,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 36,
                shader_location: 8,
            },
        ];
        let vertex_buffers = [
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<CubeVertex>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &cube_attributes,
            }),
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<EntityRaw>() as wgpu::BufferAddress,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &instance_attributes,
            }),
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("entity-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_entity"),
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
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_entity"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // Dropped-item cubes are opaque; no sorted transparency pass.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let shadow_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("entity-shadow-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("entity_shadow.wgsl").into()),
        });
        let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("entity-shadow-pipeline"),
            layout: Some(&shadow_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shadow_shader,
                entry_point: Some("vs_entity_shadow"),
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
                format: crate::pipeline::SHADOW_FORMAT,
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
                module: &shadow_shader,
                entry_point: Some("fs_entity_shadow"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            shadow_pipeline,
            cube_buffer,
            instance_buffer,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instances: Vec::new(),
            gpu_instances: Vec::new(),
        }
    }

    /// Replaces the drawn entities, keeping the submitted order.
    pub(crate) fn set(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, entities: &[EntityInstance]) {
        self.instances.clear();
        self.instances.extend_from_slice(entities);
        self.gpu_instances.clear();
        self.gpu_instances.extend(entities.iter().copied().map(EntityRaw::from));

        if self.gpu_instances.len() > self.instance_capacity {
            self.instance_capacity = self.gpu_instances.len().next_power_of_two();
            self.instance_buffer = create_instance_buffer(device, self.instance_capacity);
        }
        if !self.gpu_instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&self.gpu_instances));
        }
    }

    /// The instances most recently supplied (input order).
    pub fn instances(&self) -> &[EntityInstance] {
        &self.instances
    }

    /// Number of instances that would be drawn.
    pub fn instance_count(&self) -> usize {
        self.gpu_instances.len()
    }

    /// Draws every instance into an already-bound world pass.
    ///
    /// Does nothing for an empty entity list (no pipeline switch, no draw call).
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, bind_group: &wgpu::BindGroup) {
        let instance_count = self.gpu_instances.len() as u32;
        if instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_vertex_buffer(0, self.cube_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..CUBE_VERTEX_COUNT, 0..instance_count);
    }

    /// Draws every instance into a shadow-cascade depth pass.
    ///
    /// The shadow pipeline shares the unit-cube and instance buffers; textured
    /// items discard transparent texels so dropped leaves do not cast a solid
    /// cube shadow. An empty list issues no draw call.
    pub(crate) fn draw_shadow(&self, pass: &mut wgpu::RenderPass<'_>, bind_group: &wgpu::BindGroup) {
        let instance_count = self.gpu_instances.len() as u32;
        if instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.shadow_pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_vertex_buffer(0, self.cube_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..CUBE_VERTEX_COUNT, 0..instance_count);
    }
}

/// Creates an instance vertex buffer with room for `capacity` entries.
fn create_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("entity-instances"),
        size: (capacity.max(1) * std::mem::size_of::<EntityRaw>()) as wgpu::BufferAddress,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_content::textures;

    #[test]
    fn instance_defaults_and_constructors_are_grounded() {
        let default = EntityInstance::default();
        assert_eq!(default.position, [0.0; 3]);
        assert_eq!(default.scale, EntityInstance::DEFAULT_SCALE);
        assert_eq!(default.yaw, 0.0);
        assert_eq!(default.layer, 0);
        assert_eq!(default.tint, [1.0; 3]);
        assert!(default.textured);

        let block = EntityInstance::block([1.0, 2.0, 3.0], 0.4, 14);
        assert_eq!(block.position, [1.0, 2.0, 3.0]);
        assert_eq!(block.scale, 0.4);
        assert_eq!(block.yaw, 0.0);
        assert_eq!(block.layer, 14);
        assert_eq!(block.tint, [1.0; 3]);
        assert!(block.textured);

        let flat = EntityInstance::flat([0.0, 65.0, 0.0], 0.25, [0.8, 0.2, 0.1]);
        assert!(!flat.textured);
        assert_eq!(flat.layer, 0);
        assert_eq!(flat.tint, [0.8, 0.2, 0.1]);
    }

    #[test]
    fn packed_instances_preserve_order_and_flags() {
        let entities = [
            EntityInstance::block([1.0, 2.0, 3.0], 0.5, 14),
            EntityInstance::flat([4.0, 5.0, 6.0], 0.25, [1.0, 0.0, 0.0]),
        ];
        let packed: Vec<EntityRaw> = entities.iter().copied().map(EntityRaw::from).collect();
        assert_eq!(packed.len(), entities.len());
        assert_eq!(packed[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(packed[0].scale, 0.5);
        assert_eq!(packed[0].layer, 14);
        assert_eq!(packed[0].textured, 1);
        assert_eq!(packed[1].position, [4.0, 5.0, 6.0]);
        assert_eq!(packed[1].layer, 0);
        assert_eq!(packed[1].tint, [1.0, 0.0, 0.0]);
        assert_eq!(packed[1].textured, 0);
        assert_eq!(std::mem::size_of::<EntityRaw>(), 40);
        assert_eq!(std::mem::align_of::<EntityRaw>(), 4);
        assert_eq!(std::mem::offset_of!(EntityRaw, position), 0);
        assert_eq!(std::mem::offset_of!(EntityRaw, scale), 12);
        assert_eq!(std::mem::offset_of!(EntityRaw, yaw), 16);
        assert_eq!(std::mem::offset_of!(EntityRaw, layer), 20);
        assert_eq!(std::mem::offset_of!(EntityRaw, tint), 24);
        assert_eq!(std::mem::offset_of!(EntityRaw, textured), 36);
    }

    #[test]
    fn cube_vertices_are_a_unit_cube_with_face_uvs() {
        assert_eq!(CUBE_VERTICES.len(), CUBE_VERTEX_COUNT as usize);
        for vertex in CUBE_VERTICES {
            assert!((-0.5..=0.5).contains(&vertex.position[0]));
            assert!((0.0..=1.0).contains(&vertex.position[1]));
            assert!((-0.5..=0.5).contains(&vertex.position[2]));
            assert!((0.0..=1.0).contains(&vertex.uv[0]));
            assert!((0.0..=1.0).contains(&vertex.uv[1]));
            assert!((0.0..=1.0).contains(&vertex.shade));
        }
        let top = CUBE_VERTICES[0];
        let bottom = CUBE_VERTICES[6];
        assert_eq!(top.position[1], 1.0);
        assert_eq!(bottom.position[1], 0.0);
        assert!(top.shade > bottom.shade);
    }

    #[test]
    fn entity_layer_maps_block_items_and_skips_non_blocks() {
        let registry = Registry::base();
        let layer = |name: &str| {
            let item = registry.item_id(name).unwrap_or_else(|| panic!("missing item {name}"));
            entity_layer(&registry, item)
        };
        assert_eq!(layer("openblock:planks"), Some(u32::from(textures::PLANKS.0)));
        assert_eq!(layer("openblock:cobblestone"), Some(u32::from(textures::COBBLESTONE.0)));
        assert_eq!(
            layer("openblock:crafting_table"),
            Some(u32::from(textures::CRAFTING_TABLE_SIDE.0))
        );
        assert_eq!(
            layer("openblock:crafting_table"),
            Some(
                textures::NAMES
                    .iter()
                    .position(|name| *name == "crafting_table_side")
                    .unwrap() as u32
            )
        );
        // Dropped block item cubes always show the side texture; the +Z front
        // texture is a placed-block feature only.
        assert_eq!(layer("openblock:furnace"), Some(u32::from(textures::FURNACE_SIDE.0)));
        assert_eq!(layer("openblock:chest"), Some(u32::from(textures::CHEST_SIDE.0)));
        for name in [
            "openblock:apple",
            "openblock:stick",
            "openblock:wooden_pickaxe",
            "openblock:stone_sword",
        ] {
            assert_eq!(layer(name), None, "{name} is not a block item");
        }

        let all = entity_layers(&registry);
        assert_eq!(all.len(), registry.item_len());
        let planks = registry.item_id("openblock:planks").unwrap();
        assert_eq!(all[planks.0 as usize], Some(u32::from(textures::PLANKS.0)));
        let apple = registry.item_id("openblock:apple").unwrap();
        assert_eq!(all[apple.0 as usize], None);
    }
}

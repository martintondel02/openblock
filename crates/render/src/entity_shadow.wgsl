//! Depth-only entity shadow casters.
//!
//! Separate from `entity.wgsl` because it binds the per-cascade light matrix at
//! group 0 binding 3, which the main entity pipeline does not have.

struct ShadowCamera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> shadow_camera: ShadowCamera;

@group(0) @binding(1)
var blocks: texture_2d_array<f32>;

@group(0) @binding(2)
var block_sampler: sampler;

@group(0) @binding(3)
var<uniform> cascade: ShadowCamera;

struct EntityVertex {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) shade: f32,
};

struct EntityInstance {
    @location(3) position: vec3<f32>,
    @location(4) scale: f32,
    @location(5) yaw: f32,
    @location(6) layer: u32,
    @location(7) tint: vec3<f32>,
    @location(8) textured: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    @location(2) @interpolate(flat) textured: u32,
};

@vertex
fn vs_entity_shadow(vertex: EntityVertex, instance: EntityInstance) -> VertexOutput {
    let spin_cos = cos(instance.yaw);
    let spin_sin = sin(instance.yaw);
    let spun = vec3<f32>(
        vertex.position.x * spin_cos + vertex.position.z * spin_sin,
        vertex.position.y,
        vertex.position.z * spin_cos - vertex.position.x * spin_sin,
    );
    let world_position = instance.position + spun * instance.scale;
    var output: VertexOutput;
    output.clip_position = cascade.view_proj * vec4<f32>(world_position, 1.0);
    output.uv = vertex.uv;
    output.layer = instance.layer;
    output.textured = instance.textured;
    return output;
}

@fragment
fn fs_entity_shadow(input: VertexOutput) {
    if input.textured != 0u {
        let texel = textureSample(blocks, block_sampler, input.uv, i32(input.layer));
        if texel.a < 0.5 {
            discard;
        }
    }
}

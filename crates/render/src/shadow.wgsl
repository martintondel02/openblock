//! Depth-only sun-shadow caster pass.
//!
//! This module is separate from `shader.wgsl` because the shadow pass needs one
//! extra uniform (the current cascade's light view-projection) and must not bind
//! the shadow depth array it is writing, while the main world shader binds the
//! shadow array instead. Keeping the bindings disjoint avoids both a feedback
//! loop and a wgpu usage conflict.

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

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: u32,
    @location(3) light: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
};

@vertex
fn vs_shadow(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = cascade.view_proj * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.layer = input.layer;
    return output;
}

@fragment
fn fs_shadow_alpha(input: VertexOutput) {
    // Leaves and other alpha-cut-out materials cast shaped shadows; every
    // opaque texel keeps the depth. Water never reaches this pass because the
    // mesher puts fluid faces in a separate vertex list.
    let texel = textureSample(blocks, block_sampler, input.uv, input.layer);
    if (texel.a < 0.5) {
        discard;
    }
}

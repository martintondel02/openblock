//! Textured chunk geometry shader for the v0.0.1 forward pass.
//!
//! Vertices carry the world position, tile-space UVs, a block-texture array
//! layer and a baked light factor (face shading times per-corner ambient
//! occlusion). The fragment stage samples the nearest-filtered array and
//! writes opaque texels.

struct Camera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(0) @binding(1)
var blocks: texture_2d_array<f32>;

@group(0) @binding(2)
var block_sampler: sampler;

// Vertex inputs carry no interpolation attribute in WGSL; `layer` is a flat
// varying on the vertex output / fragment input below, as integers must be.
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
    @location(2) light: f32,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.layer = input.layer;
    output.light = input.light;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let albedo = textureSample(blocks, block_sampler, input.uv, input.layer).rgb;
    return vec4<f32>(albedo * input.light, 1.0);
}

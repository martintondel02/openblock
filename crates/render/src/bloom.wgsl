//! Bloom threshold and downsample passes.
//!
//! The HDR scene is first reduced into a half-resolution mip with a soft
//! threshold, then copied into two smaller mips. The post pass samples all
//! three mips with a bilinear sampler, which turns the chain into a cheap,
//! wide glow without a separate blur pass.

struct PostUniform {
    // x: exposure, y: bloom strength, z: saturation, w: FXAA strength.
    params: vec4<f32>,
    // x: 1 / source width, y: 1 / source height, z: bloom threshold, w: time.
    texels: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> post: PostUniform;

@group(0) @binding(1)
var source: texture_2d<f32>;

@group(0) @binding(2)
var source_sampler: sampler;

struct BloomOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_post(@builtin(vertex_index) vertex_index: u32) -> BloomOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[vertex_index];
    var output: BloomOutput;
    output.clip_position = vec4<f32>(position, 0.0, 1.0);
    output.uv = vec2<f32>(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);
    return output;
}

fn luminance(colour: vec3<f32>) -> f32 {
    return dot(colour, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn box_sample(uv: vec2<f32>) -> vec3<f32> {
    // Four bilinear taps cover a 2x2 source block; the destinations are always
    // half the source size, so this is an exact box filter. The source
    // dimensions come from the texture, not the full-resolution post uniform,
    // so the same pipeline serves every mip level.
    let dimensions = textureDimensions(source);
    let step_uv = 1.0 / vec2<f32>(f32(dimensions.x), f32(dimensions.y));
    let offset = vec2<f32>(step_uv.x * 0.5, step_uv.y * 0.5);
    return (textureSampleLevel(source, source_sampler, uv + vec2<f32>(-offset.x, -offset.y), 0.0).rgb
        + textureSampleLevel(source, source_sampler, uv + vec2<f32>(offset.x, -offset.y), 0.0).rgb
        + textureSampleLevel(source, source_sampler, uv + vec2<f32>(-offset.x, offset.y), 0.0).rgb
        + textureSampleLevel(source, source_sampler, uv + vec2<f32>(offset.x, offset.y), 0.0).rgb) * 0.25;
}

@fragment
fn fs_bloom_threshold(input: BloomOutput) -> @location(0) vec4<f32> {
    let colour = box_sample(input.uv);
    let luma = luminance(colour);
    // Soft knee: nothing below the threshold, a smooth ramp just above it, so
    // bright sky gradients do not produce a hard bloom line.
    let threshold = post.texels.z;
    let amount = smoothstep(threshold, threshold + 0.5, luma);
    let contribution = colour * amount;
    // Keep the extracted highlights roughly energy-normalised for the quarter
    // resolution destination.
    return vec4<f32>(min(contribution, vec3<f32>(8.0)), 1.0);
}

@fragment
fn fs_bloom_down(input: BloomOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(box_sample(input.uv), 1.0);
}

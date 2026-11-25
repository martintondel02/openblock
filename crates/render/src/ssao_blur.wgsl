//! Depth-aware blur for the half-resolution SSAO target.
//!
//! Runs as a second full-screen pass after `fs_ssao`; five bilinear taps are
//! weighted by their eye-depth distance from the centre so AO never bleeds
//! across a silhouette (which would draw a halo around terrain edges). Sky
//! pixels stay exactly 1.0.

struct SsaoUniform {
    // x: 1 / depth width, y: 1 / depth height, z: projection m22, w: m32.
    params: vec4<f32>,
    // x: tan(fov_y / 2), y: aspect, z: world radius per metre of depth,
    // w: radius ceiling in metres.
    view: vec4<f32>,
    // x/y: depth target size, z/w: half-resolution AO target size.
    screen: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> ssao: SsaoUniform;

@group(0) @binding(1)
var raw_ao: texture_2d<f32>;

@group(0) @binding(2)
var blur_depth: texture_depth_2d;

@group(0) @binding(3)
var linear_sampler: sampler;

fn eye_depth(ndc_depth: f32) -> f32 {
    let near = ssao.params.z;
    let far = ssao.params.w;
    return (near * far) / max(far - ndc_depth * (far - near), 1e-4);
}

struct FullscreenOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_ssao(@builtin(vertex_index) vertex_index: u32) -> FullscreenOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[vertex_index];
    var output: FullscreenOutput;
    output.clip_position = vec4<f32>(position, 0.0, 1.0);
    output.uv = vec2<f32>(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);
    return output;
}

@fragment
fn fs_ssao_blur(input: FullscreenOutput) -> @location(0) vec4<f32> {
    let centre_pixel = vec2<i32>(
        clamp(input.uv * ssao.screen.xy, vec2<f32>(0.0), ssao.screen.xy - vec2<f32>(1.0)),
    );
    let centre_depth = textureLoad(blur_depth, centre_pixel, 0);
    if centre_depth >= 1.0 {
        return vec4<f32>(1.0);
    }
    let centre_eye = eye_depth(centre_depth);
    let texel = vec2<f32>(1.0) / max(ssao.screen.zw, vec2<f32>(1.0));
    var sum = 0.0;
    var weight_sum = 0.0;
    // Five taps (centre + four neighbours) with a depth-similarity weight so a
    // silhouette does not bleed its AO across the edge.
    for (var index = 0; index < 5; index = index + 1) {
        let offset = array<vec2<f32>, 5>(
            vec2<f32>(0.0, 0.0),
            vec2<f32>(texel.x, 0.0),
            vec2<f32>(-texel.x, 0.0),
            vec2<f32>(0.0, texel.y),
            vec2<f32>(0.0, -texel.y),
        )[index];
        let uv = input.uv + offset;
        let sample_depth = textureLoad(
            blur_depth,
            vec2<i32>(
                clamp(uv * ssao.screen.xy, vec2<f32>(0.0), ssao.screen.xy - vec2<f32>(1.0)),
            ),
            0,
        );
        var weight = 0.0;
        if sample_depth < 1.0 {
            let eye = eye_depth(sample_depth);
            let tolerance = max(0.1, centre_eye * 0.02);
            weight = 1.0 / (1.0 + abs(eye - centre_eye) / tolerance);
        }
        if index == 0 {
            weight = 1.0;
        }
        sum = sum + textureSampleLevel(raw_ao, linear_sampler, uv, 0.0).r * weight;
        weight_sum = weight_sum + weight;
    }
    return vec4<f32>(sum / max(weight_sum, 1.0));
}

//! Final HDR post-processing pass.
//!
//! The scene (world, sky, water, entities) is rendered into a linear
//! `Rgba16Float` target. This pass adds the three bloom mip levels, applies
//! exposure, an ACES-fitted tonemap curve, optional FXAA-lite edge blending,
//! saturation and a subtle vignette, and writes linear values to the sRGB
//! output target (the hardware performs the linear→sRGB encode on store).
//!
//! Two fragment entries share one bind group:
//!
//! * `fs_post` — bloom and FXAA-aware, used by the default High/Ultra presets.
//! * `fs_post_simple` — no bloom or FXAA sampling, used when both are disabled
//!   (the Low preset), so those pixels do not pay for effects they cannot see.
//!
//! Both pipelines are compiled once at start-up; choosing between them is a
//! pointer switch, never a shader compilation.

struct PostUniform {
    // x: exposure, y: bloom strength, z: saturation, w: FXAA strength.
    params: vec4<f32>,
    // x: 1 / width, y: 1 / height, z: bloom threshold, w: scene time.
    texels: vec4<f32>,
    // x: SSAO strength (0 disables the fetch), y: underwater amount,
    // zw: reserved.
    effects: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> post: PostUniform;

@group(0) @binding(1)
var scene: texture_2d<f32>;

@group(0) @binding(2)
var bloom0: texture_2d<f32>;
@group(0) @binding(3)
var bloom1: texture_2d<f32>;
@group(0) @binding(4)
var bloom2: texture_2d<f32>;
@group(0) @binding(5)
var post_sampler: sampler;
// Blurred half-resolution SSAO (or a white 1×1 fallback when disabled).
@group(0) @binding(6)
var ao_texture: texture_2d<f32>;

struct PostOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle, same oversized-triangle trick as the sky pass.
@vertex
fn vs_post(@builtin(vertex_index) vertex_index: u32) -> PostOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[vertex_index];
    var output: PostOutput;
    output.clip_position = vec4<f32>(position, 0.0, 1.0);
    // Clip-space +Y points up while texture V grows down.
    output.uv = vec2<f32>(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);
    return output;
}

// Narkowicz's ACES fit. Monotonic, maps 0 to 0 and approaches 1 in the
// highlights, with a gentle S-curve through the midtones.
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn tone(colour: vec3<f32>) -> vec3<f32> {
    return aces_fitted(max(colour, vec3<f32>(0.0)) * post.params.x);
}

fn luminance(colour: vec3<f32>) -> f32 {
    return dot(colour, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn vignette(uv: vec2<f32>) -> f32 {
    // Very subtle: the centre keeps full brightness, the corners lose 6%.
    let offset = uv - vec2<f32>(0.5);
    let radius = length(offset * vec2<f32>(1.0, 0.9));
    return 1.0 - 0.06 * smoothstep(0.30, 0.80, radius);
}

fn bloom_contribution(uv: vec2<f32>) -> vec3<f32> {
    // Two mips already give a wide, soft glow; the third mip is rendered (the
    // chain keeps its documented threshold + 3-mip structure) but contributes so
    // little that skipping its full-resolution fetch is free quality-wise.
    return textureSampleLevel(bloom0, post_sampler, uv, 0.0).rgb * 0.55
        + textureSampleLevel(bloom1, post_sampler, uv, 0.0).rgb * 0.45;
}

/// Tonemapped scene colour without bloom, used for FXAA neighbour luma.
fn toned_neighbour(uv: vec2<f32>) -> vec3<f32> {
    return tone(textureSampleLevel(scene, post_sampler, uv, 0.0).rgb);
}

fn grade(toned: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    let grey = luminance(toned);
    let saturated = mix(vec3<f32>(grey), toned, post.params.z);
    return saturated * vignette(uv);
}

// Ambient-occlusion multiplier fetched at half resolution and blurred. The
// pass writes 1.0 on sky and beyond its distance fade, so the multiply is a
// no-op there; `effects.x` is zero on Low/Medium and skips the fetch entirely.
fn apply_ao(hdr: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    if post.effects.x <= 0.0 {
        return hdr;
    }
    let ao = clamp(textureSampleLevel(ao_texture, post_sampler, uv, 0.0).r, 0.0, 1.0);
    return hdr * mix(1.0, ao, post.effects.x);
}

// Cheap underwater grade: absorb warm light, lift a deep blue-green veil and
// desaturate the highlights. Applied after tonemapping so the bloom halo is
// tinted with the rest of the frame.
fn underwater_grade(toned: vec3<f32>) -> vec3<f32> {
    let amount = clamp(post.effects.y, 0.0, 1.0);
    if amount <= 0.0 {
        return toned;
    }
    let absorbed = toned * vec3<f32>(0.42, 0.78, 0.92);
    let veil = vec3<f32>(0.012, 0.058, 0.082) * (0.35 + 0.65 * luminance(toned) * 4.0);
    return mix(toned, absorbed + veil, amount * 0.8);
}

@fragment
fn fs_post(input: PostOutput) -> @location(0) vec4<f32> {
    var hdr = textureSampleLevel(scene, post_sampler, input.uv, 0.0).rgb;
    hdr = apply_ao(hdr, input.uv);
    hdr = hdr + bloom_contribution(input.uv) * post.params.y;
    var toned = tone(hdr);

    let fxaa = post.params.w;
    if (fxaa > 0.0) {
        // Two probes decide whether this pixel is on an edge. Flat sky, HUD-free
        // terrain interiors and other smooth areas (most of a 512-block frame)
        // stop here instead of paying for four neighbour fetches. Real edges then
        // fetch the remaining two directions for the luma-range test and blend.
        let step_uv = post.texels.xy;
        let centre_luma = luminance(toned);
        let left = toned_neighbour(input.uv - vec2<f32>(step_uv.x, 0.0));
        // One probe catches the overwhelming majority of edges; the three
        // remaining directions are fetched only when the left neighbour differs.
        if (abs(centre_luma - luminance(left)) > 0.010) {
            let right = toned_neighbour(input.uv + vec2<f32>(step_uv.x, 0.0));
            let down = toned_neighbour(input.uv - vec2<f32>(0.0, step_uv.y));
            let up = toned_neighbour(input.uv + vec2<f32>(0.0, step_uv.y));
            let low = min(
                min(centre_luma, luminance(left)),
                min(luminance(right), min(luminance(down), luminance(up))),
            );
            let high = max(
                max(centre_luma, luminance(left)),
                max(luminance(right), max(luminance(down), luminance(up))),
            );
            let amount = smoothstep(0.055, 0.22, high - low) * fxaa;
            let average = (left + right + down + up) * 0.25;
            toned = mix(toned, average, amount * 0.65);
        }
    }
    return vec4<f32>(clamp(underwater_grade(grade(toned, input.uv)), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

// No bloom, FXAA or SSAO work at all: one HDR fetch, ACES, saturation and
// vignette (plus the cheap underwater tint when the camera is submerged).
@fragment
fn fs_post_simple(input: PostOutput) -> @location(0) vec4<f32> {
    let hdr = textureSampleLevel(scene, post_sampler, input.uv, 0.0).rgb;
    let toned = tone(hdr);
    let graded = underwater_grade(grade(toned, input.uv));
    return vec4<f32>(clamp(graded, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

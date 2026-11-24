//! Textured chunk geometry and the screen-space sky gradient.
//!
//! Vertices carry the world position, tile-space UVs, a block-texture array
//! layer and a baked light factor (face shading times per-corner ambient
//! occlusion). The fragment stage samples the nearest-filtered array, applies
//! distance fog, and writes opaque texels. A second pair of entry points
//! (`vs_sky`/`fs_sky`) draws the background as one full-screen triangle before
//! the world pass; the sky and the fog share one palette so distant chunk edges
//! dissolve into the sky instead of showing a hard boundary.

struct Camera {
    view_proj: mat4x4<f32>,
    // xyz: eye position in world space; w: unused.
    camera_pos: vec4<f32>,
    // x: fog start distance, y: fog end distance, zw: unused.
    fog: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(0) @binding(1)
var blocks: texture_2d_array<f32>;

@group(0) @binding(2)
var block_sampler: sampler;

// Sky palette shared by the sky background and distance fog.
//
// The colours are authored as sRGB and stored here in linear RGB: both render
// targets are sRGB formats, so the hardware converts this linear output to the
// authored display values. The horizon colour is also the fog colour, which
// makes fully fogged geometry disappear into the horizon line at screen
// centre.
struct SkyPalette {
    horizon: vec3<f32>,      // sRGB (0.68, 0.80, 0.93)
    zenith: vec3<f32>,       // sRGB (0.30, 0.52, 0.86)
    warm_horizon: vec3<f32>, // subtle additive warm tint below the horizon
}

const SKY: SkyPalette = SkyPalette(
    vec3<f32>(0.4196, 0.6038, 0.8481),
    vec3<f32>(0.0732, 0.2330, 0.7106),
    vec3<f32>(0.0550, 0.0260, 0.0060),
);

// Vertical sky gradient in screen space. `ndc_y` is -1 at the bottom edge and
// +1 at the top. The geometric horizon sits at screen centre; above it the sky
// darkens towards the zenith, below it a very subtle warm haze brightens the
// lower edge.
fn sky_colour(ndc_y: f32) -> vec3<f32> {
    let t = clamp(ndc_y * 0.5 + 0.5, 0.0, 1.0);
    let above_horizon = smoothstep(0.5, 1.0, t);
    var colour = mix(SKY.horizon, SKY.zenith, above_horizon);
    let below_horizon = 1.0 - smoothstep(0.0, 0.5, t);
    colour += SKY.warm_horizon * below_horizon;
    return colour;
}

// Fog resolves to the horizon colour so the sky stays visible through it.
fn fog_colour() -> vec3<f32> {
    return SKY.horizon;
}

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
    @location(3) world_position: vec3<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.layer = input.layer;
    output.light = input.light;
    output.world_position = input.position;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(blocks, block_sampler, input.uv, input.layer);
    // Alpha cut-out: leaf layers punch small transparent holes, everything else
    // is fully opaque. The chunk pipeline has no blending, so this stays a cheap
    // discard rather than a sorted transparent pass.
    if texel.a < 0.5 {
        discard;
    }
    let albedo = texel.rgb;
    let lit = albedo * input.light;
    // Smooth distance fog: zero at or before `fog_start`, fully sky-coloured at
    // `fog_end`, with a gentle S-curve in between.
    let view_distance = distance(input.world_position, camera.camera_pos.xyz);
    let amount = smoothstep(camera.fog.x, camera.fog.y, view_distance);
    return vec4<f32>(mix(lit, fog_colour(), amount), 1.0);
}

// Full-screen triangle generated from the vertex index: no vertex buffer and no
// per-frame sky resources. The triangle is deliberately larger than the clip
// volume so the rasteriser covers every pixel with three vertices.
struct SkyOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) ndc_y: f32,
};

@vertex
fn vs_sky(@builtin(vertex_index) vertex_index: u32) -> SkyOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let position = positions[vertex_index];
    var output: SkyOutput;
    output.clip_position = vec4<f32>(position, 0.0, 1.0);
    output.ndc_y = position.y;
    return output;
}

@fragment
fn fs_sky(input: SkyOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(sky_colour(input.ndc_y), 1.0);
}

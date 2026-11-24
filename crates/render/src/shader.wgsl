//! Textured chunk geometry, the screen-space sky gradient, the sun/moon discs
//! and the targeted-block outline.
//!
//! Vertices carry the world position, tile-space UVs, a block-texture array
//! layer and a baked light factor (face shading times per-corner ambient
//! occlusion). The main fragment stage samples the nearest-filtered array,
//! scales the baked light by the time-of-day world light, applies distance fog
//! and writes opaque texels.
//!
//! `vs_sky`/`fs_sky` draw the background as one full-screen triangle before the
//! world pass. The sky uses the current zenith/horizon colours from the camera
//! uniform, the same horizon colour is the fog colour, and the sun, moon and a
//! procedural star field are composited on top using a world-space view ray
//! reconstructed from the camera basis.
//!
//! `vs_outline`/`fs_outline` draw a 24-vertex line-list cube after the world
//! geometry with `LessEqual` depth testing and no depth writes.

struct Camera {
    view_proj: mat4x4<f32>,
    // xyz: eye position in world space; w: unused.
    camera_pos: vec4<f32>,
    // x: fog start distance, y: fog end distance, zw: unused.
    fog: vec4<f32>,
    // xyz: unit vector towards the sun; w: sun-disc intensity.
    sun: vec4<f32>,
    // rgb: linear sun-disc colour; w: star visibility (0 day .. 1 night).
    sun_colour: vec4<f32>,
    // rgb: linear zenith colour; w: ambient light multiplier.
    zenith: vec4<f32>,
    // rgb: linear horizon/fog colour; w: daylight block-light multiplier.
    horizon: vec4<f32>,
    // xyz: unit camera forward; w: tan(fov_y / 2).
    basis_forward: vec4<f32>,
    // xyz: unit camera right; w: viewport aspect ratio.
    basis_right: vec4<f32>,
    // xyz: unit camera up; w: star-field rotation in radians.
    basis_up: vec4<f32>,
    // rgb: daylight * light tint applied to baked block light; w: moon visibility.
    world_light: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(0) @binding(1)
var blocks: texture_2d_array<f32>;

@group(0) @binding(2)
var block_sampler: sampler;

// Subtle additive warm haze below the (screen-space) horizon, scaled by the
// ambient light so the night sky stays dark.
const WARM_HORIZON: vec3<f32> = vec3<f32>(0.0550, 0.0260, 0.0060);

// Generic Hermite ramp. Unlike the builtin `smoothstep`, the edges may be
// reversed (used to fade stars out as the sun rises).
fn ramp(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Vertical sky gradient in screen space. `ndc_y` is -1 at the bottom edge and
// +1 at the top. The geometric horizon sits at screen centre; above it the sky
// darkens towards the zenith, below it a very subtle warm haze brightens the
// lower edge.
fn sky_colour(ndc_y: f32) -> vec3<f32> {
    let t = clamp(ndc_y * 0.5 + 0.5, 0.0, 1.0);
    let above_horizon = ramp(0.5, 1.0, t);
    var colour = mix(camera.horizon.rgb, camera.zenith.rgb, above_horizon);
    let below_horizon = 1.0 - ramp(0.0, 0.5, t);
    colour += WARM_HORIZON * below_horizon * camera.zenith.w;
    return colour;
}

// Builds the world-space view direction for a normalised device coordinate.
fn view_ray(ndc: vec2<f32>) -> vec3<f32> {
    let forward = camera.basis_forward.xyz;
    let right = camera.basis_right.xyz;
    let up = camera.basis_up.xyz;
    let tangent = camera.basis_forward.w;
    let aspect = camera.basis_right.w;
    return normalize(forward + right * (ndc.x * tangent * aspect) + up * (ndc.y * tangent));
}

// Antialiased disc coverage for a direction and a light direction.
fn disc_coverage(view_dir: vec3<f32>, light_dir: vec3<f32>, angular_radius: f32) -> f32 {
    let cos_angle = clamp(dot(view_dir, light_dir), -1.0, 1.0);
    let inner = cos(angular_radius);
    let outer = cos(angular_radius * 1.7);
    return ramp(outer, inner, cos_angle);
}

// Sun disc plus a soft halo. Hidden once the sun is below the horizon.
fn sun_contribution(view_dir: vec3<f32>) -> vec3<f32> {
    let sun_dir = normalize(camera.sun.xyz);
    let above_horizon = ramp(-0.04, 0.03, sun_dir.y);
    if (above_horizon <= 0.0 || camera.sun.w <= 0.0) {
        return vec3<f32>(0.0);
    }
    let core = disc_coverage(view_dir, sun_dir, 0.016);
    let halo = pow(max(dot(view_dir, sun_dir), 0.0), 220.0) * 0.12;
    return camera.sun_colour.rgb * ((core * 1.6 + halo) * camera.sun.w * above_horizon);
}

// Moon disc opposite the sun, visible only at night. `world_light.w` carries
// the moon visibility computed on the CPU.
fn moon_contribution(view_dir: vec3<f32>) -> vec3<f32> {
    let moon_dir = -camera.sun.xyz;
    let above_horizon = ramp(0.02, 0.15, moon_dir.y);
    let visible = above_horizon * camera.world_light.w;
    if (visible <= 0.0) {
        return vec3<f32>(0.0);
    }
    let core = disc_coverage(view_dir, moon_dir, 0.014);
    let halo = pow(max(dot(view_dir, moon_dir), 0.0), 300.0) * 0.03;
    let tint = vec3<f32>(0.72, 0.80, 0.95);
    return tint * ((core * 1.1 + halo) * visible);
}

// Deterministic 2D hash in 0..1, stable across GPUs because it only uses
// fract/multiply/add (no precision-sensitive trigonometry).
fn hash21(p: vec2<f32>) -> f32 {
    var state = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    state = state + vec3<f32>(dot(state, state.yzx + 33.33));
    return fract((state.x + state.y) * state.z);
}

// Procedural star points.
//
// The sky is divided into a roughly square grid in azimuth/altitude; a hash
// selects a sparse subset of cells and jitters one point inside each. The grid
// rotates around the vertical axis once per day (`basis_up.w`), so the star
// field is deterministic for a given environment and never flickers per frame.
// Stars are only drawn above the world horizon and are scaled by the CPU star
// visibility (stored in `sun_colour.w`).
fn star_contribution(view_dir: vec3<f32>) -> vec3<f32> {
    if (view_dir.y <= 0.0 || camera.sun_colour.w <= 0.0) {
        return vec3<f32>(0.0);
    }
    let angle = camera.basis_up.w;
    let rotation_sin = sin(angle);
    let rotation_cos = cos(angle);
    let rotated = vec3<f32>(
        rotation_cos * view_dir.x - rotation_sin * view_dir.z,
        view_dir.y,
        rotation_sin * view_dir.x + rotation_cos * view_dir.z,
    );
    if (rotated.y <= 0.0) {
        return vec3<f32>(0.0);
    }

    let azimuth = atan2(rotated.z, rotated.x) * 0.15915494;
    let altitude = asin(clamp(rotated.y, -1.0, 1.0)) * 0.63661977;
    // A dense grid with small points: at a 70-degree FOV one cell covers about
    // 1.4 degrees, so a 0.10-cell radius lands at roughly one to two pixels.
    let grid = vec2<f32>(azimuth * 260.0, altitude * 66.0);
    let cell = floor(grid);
    let local = fract(grid);
    if (hash21(cell) > 0.14) {
        return vec3<f32>(0.0);
    }

    let jitter = vec2<f32>(
        hash21(cell + vec2<f32>(17.0, 3.0)),
        hash21(cell + vec2<f32>(43.0, 91.0)),
    );
    let point = jitter * 0.7 + vec2<f32>(0.15);
    let core = ramp(0.10, 0.0, distance(local, point));
    if (core <= 0.0) {
        return vec3<f32>(0.0);
    }
    let brightness = 0.45 + 0.55 * hash21(cell + vec2<f32>(131.0, 7.0));
    let horizon_fade = ramp(0.0, 0.06, view_dir.y);
    // A slight blue-white variation keeps the field from looking like a grid
    // of identical dots.
    let star_tint = mix(
        vec3<f32>(0.82, 0.88, 1.0),
        vec3<f32>(1.0, 0.96, 0.86),
        hash21(cell + vec2<f32>(211.0, 37.0)),
    );
    return star_tint * (core * brightness * horizon_fade * camera.sun_colour.w);
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
    // Baked face light times the time-of-day multiplier and light tint. At noon
    // `world_light.rgb` is exactly (1,1,1), preserving the daylight look.
    let lit = albedo * input.light * camera.world_light.rgb;
    // Smooth distance fog: zero at or before `fog_start`, fully sky-coloured at
    // `fog_end`, with a gentle S-curve in between. The fog colour is the current
    // horizon colour, the same value the sky gradient uses.
    let view_distance = distance(input.world_position, camera.camera_pos.xyz);
    let amount = ramp(camera.fog.x, camera.fog.y, view_distance);
    return vec4<f32>(mix(lit, camera.horizon.rgb, amount), 1.0);
}

// Full-screen triangle generated from the vertex index: no vertex buffer and no
// per-frame sky resources. The triangle is deliberately larger than the clip
// volume so the rasteriser covers every pixel with three vertices.
struct SkyOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) ndc: vec2<f32>,
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
    output.ndc = position;
    return output;
}

@fragment
fn fs_sky(input: SkyOutput) -> @location(0) vec4<f32> {
    let ray = view_ray(input.ndc);
    var colour = sky_colour(input.ndc.y);
    colour += sun_contribution(ray);
    colour += moon_contribution(ray);
    colour += star_contribution(ray);
    return vec4<f32>(colour, 1.0);
}

// Targeted-block outline: world position plus a linear RGB colour. Drawn as a
// line list with depth test but no depth writes, so it can be occluded by
// terrain that is nearer than the selected box.
struct OutlineInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
};

struct OutlineOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_outline(input: OutlineInput) -> OutlineOutput {
    var output: OutlineOutput;
    var clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    // Pull the line a fraction towards the camera in depth. Line primitives
    // cannot use a depth bias, and a uniform box inflation cancels in view
    // depth along the far top edge of an obliquely viewed block, which would
    // hide that edge behind the block it is outlining. 2.5e-5 in NDC depth is
    // about 4 cm at 9 m: enough to win that pixel-level depth comparison, far
    // too little to leak through the terrain or a pillar metres in front of
    // the outline.
    clip_position.z -= 0.000025 * clip_position.w;
    output.clip_position = clip_position;
    output.color = input.color;
    return output;
}

@fragment
fn fs_outline(input: OutlineOutput) -> @location(0) vec4<f32> {
    return input.color;
}

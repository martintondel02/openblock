//! Visual-only far terrain: opaque heightfield plus a translucent water
//! surface.
//!
//! The client builds the geometry; this shader only shades it. The vertex
//! carries a world-space position, a linear albedo (already tinted for biome,
//! slope, curvature and subtle noise) and the heightfield normal. The fragment
//! stage applies the same sun/ambient model as the near world but deliberately
//! samples no shadow map: far terrain neither casts nor receives sun shadows.
//! Distance fog uses the camera fog bounds, so the far mesh dissolves into the
//! same horizon colour as the sky and the streamed near world.

// Prefix of the shared `Camera` uniform (see `pipeline.rs`); only the fields
// needed by the far pass are declared. Keep the offsets identical to the
// prefix of the full struct.
struct FarCamera {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,
    fog: vec4<f32>,
    sun: vec4<f32>,
    sun_colour: vec4<f32>,
    zenith: vec4<f32>,
    horizon: vec4<f32>,
    basis_forward: vec4<f32>,
    basis_right: vec4<f32>,
    basis_up: vec4<f32>,
    world_light: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: FarCamera;

struct FarVertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec3<f32>,
    @location(2) normal: vec3<f32>,
};

struct FarVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) world_position: vec3<f32>,
    @location(2) normal: vec3<f32>,
};

@vertex
fn vs_far(input: FarVertexInput) -> FarVertexOutput {
    var output: FarVertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.color = input.color;
    output.world_position = input.position;
    output.normal = input.normal;
    return output;
}

fn ramp(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Shared near/far distance fog. The CPU computes one fog range from the
// camera's far plane, so the streamed near world and the far heightfield never
// disagree about the horizon.
fn apply_far_fog(color: vec3<f32>, world_position: vec3<f32>) -> vec3<f32> {
    let view_distance = distance(world_position, camera.camera_pos.xyz);
    let amount = ramp(camera.fog.x, camera.fog.y, view_distance);
    return mix(color, camera.horizon.rgb, amount);
}

@fragment
fn fs_far(input: FarVertexOutput) -> @location(0) vec4<f32> {
    let sun_direction = normalize(camera.sun.xyz);
    let sun_up = smoothstep(0.0, 0.08, sun_direction.y);
    let daylight = clamp(camera.horizon.w, 0.0, 1.0);
    let normal = normalize(input.normal);
    let direct = max(dot(normal, sun_direction), 0.0) * sun_up * daylight;
    // The near terrain bakes face brightness and AO into its sky light. The
    // far mesh has neither shadows nor block light, so use the same nominal
    // range: full sky on sun-facing ground, ~78% ambient on slopes facing away.
    let light = camera.world_light.rgb * (0.78 + 0.22 * direct);
    let lit = input.color * light;
    return vec4<f32>(apply_far_fog(lit, input.world_position), 1.0);
}

@fragment
fn fs_far_water(input: FarVertexOutput) -> @location(0) vec4<f32> {
    let normal = normalize(input.normal);
    let view = normalize(camera.camera_pos.xyz - input.world_position);
    let fresnel = pow(1.0 - clamp(dot(normal, view), 0.0, 1.0), 5.0);
    let sun_direction = normalize(camera.sun.xyz);
    let sun_up = smoothstep(0.0, 0.08, sun_direction.y);
    let daylight = clamp(camera.horizon.w, 0.0, 1.0);

    let deep = input.color * (camera.world_light.rgb * 1.05);
    let reflected = mix(camera.horizon.rgb, camera.zenith.rgb, 0.45);
    var color = mix(deep, reflected, fresnel * 0.55);

    let half_direction = normalize(view + sun_direction);
    let specular = pow(max(dot(normal, half_direction), 0.0), 96.0) * sun_up * daylight;
    color = color + camera.sun_colour.rgb * (specular * 0.22);
    let alpha = clamp(0.72 + fresnel * 0.22, 0.0, 1.0);
    return vec4<f32>(apply_far_fog(color, input.world_position), alpha);
}

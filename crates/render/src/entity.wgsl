//! Dropped-item entity cubes.
//!
//! One instanced unit cube (base centre at the origin, edge length one) is
//! transformed by the per-instance position/scale/yaw. Textured instances
//! sample the shared block texture array through the same bind group as the
//! world pass; flat instances use their sRGB tint converted to linear. Faces
//! carry a baked shade so the small cubes read as three-dimensional, and the
//! world fog/world-light values are reused so entities sit in the scene.

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

fn ramp(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// sRGB (as documented on `EntityInstance::tint`) to the linear values the
// sRGB target expects.
fn srgb_to_linear(colour: vec3<f32>) -> vec3<f32> {
    let low = colour / 12.92;
    let high = pow((colour + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, colour <= vec3<f32>(0.04045));
}

struct EntityVertex {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) shade: f32,
};

struct EntityInstance {
    // Base-centre position in world space.
    @location(3) position: vec3<f32>,
    @location(4) scale: f32,
    @location(5) yaw: f32,
    @location(6) layer: u32,
    @location(7) tint: vec3<f32>,
    // 0 = flat tinted cube, 1 = block texture array.
    @location(8) textured: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) shade: f32,
    @location(2) tint: vec3<f32>,
    @location(3) @interpolate(flat) layer: u32,
    @location(4) @interpolate(flat) textured: u32,
    @location(5) world_position: vec3<f32>,
};

@vertex
fn vs_entity(vertex: EntityVertex, instance: EntityInstance) -> VertexOutput {
    let spin_cos = cos(instance.yaw);
    let spin_sin = sin(instance.yaw);
    let spun = vec3<f32>(
        vertex.position.x * spin_cos + vertex.position.z * spin_sin,
        vertex.position.y,
        vertex.position.z * spin_cos - vertex.position.x * spin_sin,
    );
    let world_position = instance.position + spun * instance.scale;

    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(world_position, 1.0);
    output.uv = vertex.uv;
    output.shade = vertex.shade;
    output.tint = instance.tint;
    output.layer = instance.layer;
    output.textured = instance.textured;
    output.world_position = world_position;
    return output;
}

@fragment
fn fs_entity(input: VertexOutput) -> @location(0) vec4<f32> {
    let tint = srgb_to_linear(input.tint);
    var colour: vec3<f32>;
    if input.textured != 0u {
        let texel = textureSample(blocks, block_sampler, input.uv, i32(input.layer));
        if texel.a < 0.5 {
            discard;
        }
        colour = texel.rgb * tint;
    } else {
        colour = tint;
    }

    let lit = colour * input.shade * camera.world_light.rgb;
    let view_distance = distance(input.world_position, camera.camera_pos.xyz);
    let amount = ramp(camera.fog.x, camera.fog.y, view_distance);
    return vec4<f32>(mix(lit, camera.horizon.rgb, amount), 1.0);
}

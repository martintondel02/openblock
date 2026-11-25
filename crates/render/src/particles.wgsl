//! Instanced weather, fire and effect particles.
//!
//! One draw call draws six vertices per instance. The quad is expanded in the
//! vertex stage from the camera basis (so it always faces the viewer), rotated
//! by the per-instance angle, and shaped procedurally in the fragment stage:
//! rain streaks, snowflakes, smoke puffs, embers, debris shards, XP sparks and
//! hearts all share the same pipeline and no texture binding.
//!
//! Particles are alpha-blended after the opaque and water passes with depth
//! testing but no depth writes. Distance fog fades the alpha to zero so an
//! explosion on the horizon does not pop out of the haze, and a soft near fade
//! keeps a drop from covering the camera.

// Prefix of the shared camera uniform (the full struct is 560 bytes; a uniform
// binding may be larger than the struct the shader declares).
struct Camera {
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
};

@group(0) @binding(0)
var<uniform> camera: Camera;

struct Instance {
    // Centre in world metres.
    @location(0) position: vec3<f32>,
    // Half width / half height in metres.
    @location(1) size: vec2<f32>,
    // Linear RGBA with the age fade already folded in.
    @location(2) color: vec4<f32>,
    // Billboard rotation in radians.
    @location(3) rotation: f32,
    // 0 rain, 1 snow, 2 smoke, 3 ember, 4 debris, 5 xp, 6 heart.
    @location(4) kind: u32,
    // Emissive strength 0..1.
    @location(5) glow: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) glow: f32,
    @location(4) view_distance: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    // Two triangles, counter-clockwise in the camera plane.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let corner = corners[vertex_index % 6u];
    let angle = instance.rotation;
    let c = cos(angle);
    let s = sin(angle);
    let right = camera.basis_right.xyz * c + camera.basis_up.xyz * s;
    let up = -camera.basis_right.xyz * s + camera.basis_up.xyz * c;
    let world = instance.position + right * (corner.x * instance.size.x) + up * (corner.y * instance.size.y);

    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(world, 1.0);
    output.uv = corner * 0.5 + 0.5;
    output.color = instance.color;
    output.kind = instance.kind;
    output.glow = instance.glow;
    output.view_distance = distance(world, camera.camera_pos.xyz);
    return output;
}

// Soft radial falloff with a controllable hard core.
fn radial(p: vec2<f32>, core: f32) -> f32 {
    let r = length(p);
    return smoothstep(1.0, core, r);
}

// Procedural silhouette for one kind. `p` is in `-1..1` quad space.
fn shape_alpha(kind: u32, p: vec2<f32>, uv: vec2<f32>) -> f32 {
    switch kind {
        // Rain: a thin streak with tapered ends.
        case 0u: {
            let taper = smoothstep(0.0, 0.28, uv.y) * smoothstep(1.0, 0.72, uv.y);
            let thin = pow(max(1.0 - abs(p.x), 0.0), 2.4);
            return thin * taper;
        }
        // Snow: a soft round flake with a faint six-point sparkle.
        case 1u: {
            let r = length(p);
            let core = smoothstep(1.0, 0.15, r);
            let angle = atan2(p.y, p.x);
            let spokes = 0.5 + 0.5 * cos(angle * 6.0);
            return core * (0.82 + 0.18 * spokes * smoothstep(0.55, 1.0, r));
        }
        // Smoke: a soft puff.
        case 2u: {
            return radial(p, 0.05) * 0.62;
        }
        // Ember: a hot core with a halo.
        case 3u: {
            let core = smoothstep(1.0, 0.0, length(p));
            return core * core;
        }
        // Debris: a rotating rounded shard.
        case 4u: {
            let box = max(abs(p.x), abs(p.y));
            return 1.0 - smoothstep(0.72, 1.0, box);
        }
        // XP: a diamond spark.
        case 5u: {
            let diamond = abs(p.x) + abs(p.y);
            return smoothstep(1.0, 0.25, diamond);
        }
        // Heart: implicit heart curve.
        case 6u: {
            let q = vec2<f32>(p.x * 1.15, -p.y * 1.15 + 0.15);
            let a = q.x * q.x + q.y * q.y - 0.55;
            let heart = a * a * a - q.x * q.x * q.y * q.y * q.y * 0.85;
            return smoothstep(0.06, -0.02, heart);
        }
        default: {
            return radial(p, 0.0);
        }
    }
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let p = input.uv * 2.0 - 1.0;
    var alpha = shape_alpha(input.kind, p, input.uv) * input.color.a;
    if (alpha <= 0.003) {
        discard;
    }
    // Fade into the aerial haze rather than drawing a crisp sprite over it.
    let fog = smoothstep(camera.fog.x, camera.fog.y, input.view_distance);
    alpha = alpha * (1.0 - fog);
    // A drop or ember directly in front of the eye should not fill the screen.
    alpha = alpha * smoothstep(0.10, 0.55, input.view_distance);
    if (alpha <= 0.002) {
        discard;
    }
    // Emissive kinds get a little extra headroom for the HDR bloom pass.
    let emissive = 1.0 + input.glow * 1.5;
    return vec4<f32>(input.color.rgb * emissive, alpha);
}

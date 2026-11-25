//! Visual-only far terrain: opaque heightfield plus a translucent water
//! surface.
//!
//! The client builds the geometry; this shader only shades it. The vertex
//! carries a world-space position, a linear albedo (already tinted for biome,
//! slope, curvature and subtle noise) and the heightfield normal. The fragment
//! stage applies the same sun/ambient model, height fog, cloud-shadow
//! modulation and near/far seam blend as the near world, but deliberately
//! samples no shadow map: far terrain neither casts nor receives sun shadows.
//! Distance fog uses the camera fog bounds, so the far mesh dissolves into the
//! same horizon colour as the sky and the streamed near world.
//!
//! The near world fades towards the same seam colour at the streamed-world
//! edge (`atmosphere.x`), and this shader starts there and reaches its own
//! shading after `atmosphere.y` blocks. That two-sided cross-fade is what makes
//! the change from textured blocks to the coarse palette a soft aerial band
//! rather than a hard style step.

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
    // Shadow-cascade matrices and splits (unused by this pass) are declared so
    // the following fields land at the same offsets as the full `Camera`.
    shadow_view_proj: array<mat4x4<f32>, 3>,
    shadow_split: vec4<f32>,
    shadow_texel: vec4<f32>,
    shadow_params: vec4<f32>,
    // x: scene time, y: cloud coverage, z: cloud quality, w: cloud-shadow strength.
    graphics: vec4<f32>,
    // x: wave strength, y: surface alpha, z: specular, w: sea-surface height.
    water: vec4<f32>,
    shadow_scale: vec4<f32>,
    // x: seam radius, y: seam band, z: height-fog density, w: height-fog scale.
    atmosphere: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: FarCamera;

@group(0) @binding(6)
var cloud_density: texture_2d<f32>;
@group(0) @binding(7)
var cloud_density_sampler: sampler;

// Cloud plane height and wind matching `shader.wgsl`.
const CLOUD_HEIGHT: f32 = 190.0;
const CLOUD_WIND: vec2<f32> = vec2<f32>(0.82, 0.34);

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
    // x: height-haze amount, y: seam cross-fade amount, z: main distance-fog
    // amount, w: view distance in blocks. Fog is smooth at the 8–128 m far-LOD
    // cell size, so evaluating it per vertex and interpolating removes the
    // per-fragment square roots and exponentials from the whole far mesh.
    @location(3) atmosphere: vec4<f32>,
};

// Fog factors for one far vertex; mirrors the near shader's atmosphere terms.
fn far_vertex_atmosphere(world_position: vec3<f32>) -> vec4<f32> {
    let view_distance = distance(world_position, camera.camera_pos.xyz);
    let seam = camera.atmosphere.x;
    var seam_amount = 0.0;
    if seam > 0.0 {
        let band = max(camera.atmosphere.y, 1.0);
        let dx = world_position.x - camera.camera_pos.x;
        let dz = world_position.z - camera.camera_pos.z;
        let plane_squared = dx * dx + dz * dz;
        let outer = seam + band;
        let inner = max(seam - band, 0.0);
        if plane_squared < outer * outer && plane_squared > inner * inner {
            let fade = clamp((outer * outer - plane_squared) / max(outer * outer - seam * seam, 1.0), 0.0, 1.0);
            seam_amount = fade * fade * (3.0 - 2.0 * fade) * 0.22;
        }
    }
    var haze = 0.0;
    let density = camera.atmosphere.z;
    if density > 0.0 && view_distance > 96.0 {
        let scale = max(camera.atmosphere.w, 1.0);
        let height = 1.0 / (1.0 + max(world_position.y - camera.water.w, 0.0) / scale);
        let optical_depth = density * view_distance * height;
        haze = optical_depth / (1.0 + optical_depth);
    }
    let fog = ramp(camera.fog.x, camera.fog.y, view_distance);
    return vec4<f32>(haze, seam_amount, fog, view_distance);
}

// Applies the pre-multiplied vertex fog factors: haze, seam, distance fog.
fn apply_far_atmosphere(color: vec3<f32>, factors: vec4<f32>) -> vec3<f32> {
    var result = mix(color, far_haze_colour(), factors.x);
    result = mix(result, far_seam_colour(), factors.y);
    return mix(result, camera.horizon.rgb, factors.z);
}

@vertex
fn vs_far(input: FarVertexInput) -> FarVertexOutput {
    var output: FarVertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.color = input.color;
    output.world_position = input.position;
    output.normal = input.normal;
    output.atmosphere = far_vertex_atmosphere(input.position);
    return output;
}

fn ramp(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn far_haze_colour() -> vec3<f32> {
    return mix(camera.horizon.rgb, camera.zenith.rgb, 0.16);
}

fn far_seam_colour() -> vec3<f32> {
    return mix(far_haze_colour(), camera.horizon.rgb, 0.25);
}

// Height fog and the far half of the seam cross-fade. Exactly the same terms
// the near shader evaluates, so both sides agree at the seam radius.
// Cloud shadows projected from the shared world-space cloud plane. The far
// pass uses the same two-octave field as the near pass (cheaper than the sky's
// domain-warped noise) so a cloud shadow crossing the seam is continuous.
fn far_cloud_shadow(world_position: vec3<f32>, view_distance: f32) -> f32 {
    var strength = camera.graphics.w;
    if strength <= 0.0 || view_distance > 1536.0 {
        return 1.0;
    }
    strength = strength * (1.0 - smoothstep(1024.0, 1536.0, view_distance));
    let sun_direction = normalize(camera.sun.xyz);
    if sun_direction.y <= 0.03 {
        return 1.0;
    }
    let travel = (CLOUD_HEIGHT - world_position.y) / sun_direction.y;
    if travel <= 0.0 || travel > 20000.0 {
        return 1.0;
    }
    let world = world_position.xz + sun_direction.xz * travel;
    let wind = CLOUD_WIND * (camera.graphics.x * 0.004);
    let uv = world * 0.0055 + wind;
    let density = textureSampleLevel(cloud_density, cloud_density_sampler, uv, 0.0).r;
    let coverage = clamp(camera.graphics.y, 0.0, 1.0);
    let threshold = mix(0.62, 0.30, coverage);
    let occluded = smoothstep(threshold, threshold + 0.16, density);
    return 1.0 - occluded * strength;
}

@fragment
fn fs_far(input: FarVertexOutput) -> @location(0) vec4<f32> {
    let sun_direction = normalize(camera.sun.xyz);
    let sun_up = smoothstep(0.0, 0.08, sun_direction.y);
    let daylight = clamp(camera.horizon.w, 0.0, 1.0);
    let normal = normalize(input.normal);
    let direct = max(dot(normal, sun_direction), 0.0) * sun_up * daylight;
    // Match the near world's baked sky light range: 1.0 on a sun-facing top
    // face, ~0.82 on a slope facing away (the near face table shades sides at
    // 0.72-0.85 and the top at 1.0). Height/slope tint is baked into the albedo.
    let light = camera.world_light.rgb * (0.82 + 0.18 * direct);
    var lit = input.color * light;
    // Cloud shadows move across both tiers with the same field, keeping the
    // 512-chunk vista coherent when a cloud passes in front of the sun.
    lit = lit * far_cloud_shadow(input.world_position, input.atmosphere.w);
    return vec4<f32>(apply_far_atmosphere(lit, input.atmosphere), 1.0);
}

// Small helper so the water shade above stays readable: the cloud shadow only
// matters while the sun is up.
fn cloud_shadow_far(world_position: vec3<f32>, sun_up: f32, view_distance: f32) -> f32 {
    if sun_up <= 0.0 {
        return 1.0;
    }
    return far_cloud_shadow(world_position, view_distance);
}

@fragment
fn fs_far_water(input: FarVertexOutput) -> @location(0) vec4<f32> {
    let normal = normalize(input.normal);
    let world = input.world_position;
    let view = normalize(camera.camera_pos.xyz - world);
    let time = camera.graphics.x;
    let wave = camera.water.x;
    // Animated ripples give the distant water a moving highlight and break the
    // flat colour band, without a depth copy or extra texture.
    let p = world.xz;
    let ripple = vec2<f32>(
        sin(p.x * 0.031 + time * 0.9) + 0.45 * sin(p.y * 0.057 - time * 1.3),
        cos(p.y * 0.034 + time * 0.8) + 0.45 * cos(p.x * 0.049 + time * 1.1),
    );
    let perturbed = normalize(normal + vec3<f32>(ripple.x * 0.05 * wave, 0.0, ripple.y * 0.05 * wave));
    let fresnel = 0.02 + 0.98 * pow(1.0 - clamp(dot(perturbed, view), 0.0, 1.0), 5.0);
    let sun_direction = normalize(camera.sun.xyz);
    let sun_up = smoothstep(0.0, 0.08, sun_direction.y);
    let daylight = clamp(camera.horizon.w, 0.0, 1.0);
    let half_direction = normalize(view + sun_direction);
    let ndh = max(dot(perturbed, half_direction), 0.0);
    let glint = pow(ndh, 180.0) + 0.2 * pow(ndh, 36.0);

    // The baked water albedo darkens with depth, so its luminance is a cheap
    // depth proxy: shallow margins stay translucent and show the bed through,
    // deep ocean tends towards the opaque absorbed blue.
    let luminance = dot(input.color, vec3<f32>(0.2126, 0.7152, 0.0722));
    let deepness = clamp(1.0 - luminance * 2.4, 0.0, 1.0);
    let deep = input.color * (camera.world_light.rgb * 1.05);
    let reflected = mix(camera.horizon.rgb, camera.zenith.rgb, smoothstep(0.0, 0.35, reflect(-view, perturbed).y));
    var color = mix(deep, reflected, fresnel * 0.60);
    color = color * cloud_shadow_far(world, sun_up, input.atmosphere.w);
    color = color + camera.sun_colour.rgb * (glint * 0.20 * sun_up * daylight);

    let base_alpha = mix(0.42, camera.water.y, deepness);
    let alpha = clamp(mix(base_alpha, 0.94, fresnel * 0.7), 0.05, 0.97);
    return vec4<f32>(apply_far_atmosphere(color, input.atmosphere), alpha);
}

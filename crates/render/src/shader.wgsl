//! Textured chunk geometry, transparent water, the screen-space sky (gradient,
//! animated clouds, sun/moon, stars), the shadow-depth pass and the
//! targeted-block outline.
//!
//! Vertices carry the world position, tile-space UVs, a block-texture array
//! layer and a baked light factor (face shading times per-corner ambient
//! occlusion). The opqaue fragment stage samples the array, attenuates direct
//! sunlight with the fitted cascade shadow map, scales the baked light by the
//! time-of-day world light, applies distance fog and writes into the linear
//! HDR target. Water has a second fragment entry (`fs_water`) with animated
//! normals, Fresnel, a sun highlight and alpha blending.
//!
//! Shadows use three fitted cascades stored in a depth texture array.
//! `sun_light_factor` selects and blends cascades and applies a normal offset so
//! the baked ambient/AO term is untouched: only the direct-sun contribution is
//! attenuated. The `fs_shadow_alpha` entry renders the casters into the depth
//! map and discards transparent leaf texels.
//!
//! `vs_sky`/`fs_sky` draw the background as one full-screen triangle. The sky
//! uses the current zenith/horizon colours, a world-space cloud layer, a subtle
//! horizon haze, the sun, the moon and a procedural star field, all reconstructed
//! from the camera basis and environment uniform.

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
    // World-to-shadow-clip matrices for the three cascades.
    shadow_view_proj: array<mat4x4<f32>, 3>,
    // x/y/z: far distance of cascades 0..2; w: active cascade count.
    shadow_split: vec4<f32>,
    // x/y/z: world-space texel size of cascades 0..2; w: 1 / map resolution.
    shadow_texel: vec4<f32>,
    // x: normal offset in texels, y: reserved depth bias, z: PCF half-width,
    // w: shadow strength.
    shadow_params: vec4<f32>,
    // x: scene time in seconds, y: cloud coverage, z: cloud quality factor.
    graphics: vec4<f32>,
    // x: wave strength, y: surface alpha, z: sun-specular strength.
    water: vec4<f32>,
    // x/y/z: UV scale of the shadow cascade when sampling (0.5 for the
    // half-resolution far cascade).
    shadow_scale: vec4<f32>,
    // x: near/far seam radius in blocks (0 disables the seam fade),
    // y: seam band half-width, z: height-fog density, w: height-fog scale.
    atmosphere: vec4<f32>,
    // x: near plane, y: far plane, z: fresh depth copy bound, w: reserved.
    clip: vec4<f32>,
    // x: weather intensity 0..1, y: storm weight 0..1, zw: reserved.
    weather: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(0) @binding(1)
var blocks: texture_2d_array<f32>;

@group(0) @binding(2)
var block_sampler: sampler;

@group(0) @binding(3)
var shadow_map: texture_depth_2d_array;

@group(0) @binding(4)
var shadow_sampler: sampler_comparison;

// Depth-only copy of the scene depth buffer, refreshed before the water pass
// on the High/Ultra water path. `camera.clip.z` is 1 only when this frame's
// copy is valid.
@group(0) @binding(5)
var scene_depth: texture_depth_2d;

// Baked tiling cloud density and its linear/repeat sampler (see
// `cloud_shadow.rs`). One fetch replaces four octaves of per-fragment value
// noise for the ground cloud shadows.
@group(0) @binding(6)
var cloud_density: texture_2d<f32>;
@group(0) @binding(7)
var cloud_density_sampler: sampler;

// Subtle additive warm haze below the (screen-space) horizon, scaled by the
// ambient light so the night sky stays dark.
const WARM_HORIZON: vec3<f32> = vec3<f32>(0.0550, 0.0260, 0.0060);

// Catalogue layer of the `openblock:fire` cross block (`ob_content::textures::FIRE`).
// Kept as a local constant so this shader stays a pure GPU module.
const FIRE_LAYER: u32 = 38u;

// Cloud plane height above sea level, in metres. Clouds are composited from a
// world-space plane so they parallax correctly as the camera moves.
const CLOUD_HEIGHT: f32 = 190.0;
// Storm clouds sit lower and thicker. The 0.58 factor at full intensity brings
// the layer from 190 m down to about 110 m, enough to feel oppressive without
// putting the player inside the cloud deck on a mountain.
fn cloud_height() -> f32 {
    let storm = clamp(camera.weather.x + camera.weather.y, 0.0, 1.0);
    return CLOUD_HEIGHT * mix(1.0, 0.58, storm);
}
// Wind direction and speed of the cloud layer (metres per second).
const CLOUD_WIND: vec2<f32> = vec2<f32>(0.82, 0.34);

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
    // A wider atmospheric glow around the sun, strongest at dawn and dusk.
    let glow = pow(max(dot(view_dir, sun_dir), 0.0), 10.0) * 0.05;
    return camera.sun_colour.rgb * ((core * 1.6 + halo + glow) * camera.sun.w * above_horizon);
}

// Moon disc opposite the sun, visible only at night. `world_light.w` carries
// the moon visibility computed on the CPU.
fn moon_contribution(view_dir: vec3<f32>) -> vec3<f32> {
    let moon_dir = -normalize(camera.sun.xyz);
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

// Smooth value noise over a 2D grid, used for the cloud layer.
fn value_noise(p: vec2<f32>) -> f32 {
    let cell = floor(p);
    let local = fract(p);
    let blend = local * local * (3.0 - 2.0 * local);
    let a = hash21(cell);
    let b = hash21(cell + vec2<f32>(1.0, 0.0));
    let c = hash21(cell + vec2<f32>(0.0, 1.0));
    let d = hash21(cell + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, blend.x), mix(c, d, blend.x), blend.y);
}

// Fractal cloud noise. `quality` is 0..1; Low quality uses two octaves.
fn cloud_noise(p: vec2<f32>, quality: f32) -> f32 {
    let octaves = select(2, 3, quality > 0.7);
    var sum = 0.0;
    var amplitude = 0.55;
    var frequency = 1.0;
    var total = 0.0;
    for (var index = 0; index < 4; index = index + 1) {
        if (index >= octaves) {
            break;
        }
        sum = sum + value_noise(p * frequency) * amplitude;
        total = total + amplitude;
        amplitude = amplitude * 0.5;
        frequency = frequency * 2.03;
    }
    return sum / max(total, 0.001);
}

// Animated cloud layer composited over the sky. Returns premultiplied-ish colour
// and coverage: the caller linearly interpolates by alpha.
fn cloud_contribution(view_dir: vec3<f32>) -> vec4<f32> {
    let quality = camera.graphics.z;
    if (quality <= 0.0 || view_dir.y <= 0.005) {
        return vec4<f32>(0.0);
    }
    let plane_height = cloud_height() - camera.camera_pos.y;
    let distance_to_plane = plane_height / view_dir.y;
    if (distance_to_plane <= 0.0 || distance_to_plane > 30000.0) {
        return vec4<f32>(0.0);
    }
    let world = camera.camera_pos.xz + view_dir.xz * distance_to_plane;
    let storm = clamp(camera.weather.x + camera.weather.y, 0.0, 1.0);
    let wind = CLOUD_WIND * (camera.graphics.x * 0.004) * (1.0 + storm * 1.6);
    var uv = world * 0.0055 + wind;
    // Domain warp breaks the regular noise grid without another texture.
    // One cheap value-noise sample drives a diagonal domain warp. A full fbm
    // warp looked marginally better but cost two extra octave sets across the
    // whole sky; the single sample still breaks the regular grid.
    let warp = value_noise(uv * 2.9 + vec2<f32>(3.1, 7.7)) - 0.5;
    uv = uv + vec2<f32>(warp, warp * 0.6) * 0.05;
    let density = cloud_noise(uv, quality);
    let coverage = clamp(camera.graphics.y, 0.0, 1.0);
    let threshold = mix(0.60, 0.26, coverage) - storm * 0.06;
    let shaped = ramp(threshold, threshold + 0.16, density);
    if (shaped <= 0.0) {
        return vec4<f32>(0.0);
    }

    let daylight = clamp(camera.horizon.w, 0.0, 1.0);
    let sun_dir = normalize(camera.sun.xyz);
    let sun_up = ramp(-0.04, 0.08, sun_dir.y);
    // Sample the density a little towards the sun: thinner density means the
    // edge is lit, thicker means the cloud body shades itself.
    let toward_sun = uv + normalize(sun_dir.xz + vec2<f32>(0.001)) * 0.035;
    let sun_side = value_noise(toward_sun * 1.6);
    let edge = clamp(shaped - ramp(threshold, threshold + 0.16, sun_side) + 0.35, 0.0, 1.0);
    let sun_amount = pow(max(dot(view_dir, sun_dir), 0.0), 6.0);

    var cloud_body = mix(vec3<f32>(0.16, 0.19, 0.26), vec3<f32>(0.92, 0.94, 0.98), daylight);
    // Storm bases are grey and self-shaded; the lit rim survives only as a
    // faint warm edge. This is what sells "heavy overcast" rather than "night".
    cloud_body = mix(cloud_body, vec3<f32>(0.20, 0.22, 0.25), storm * 0.72);
    let warm = mix(camera.horizon.rgb, camera.sun_colour.rgb, 0.55);
    // Thicker parts of a cloud shade themselves a little; combined with the
    // sun-facing rim this gives the layer some volume instead of a flat white.
    let lit = cloud_body * (0.35 + 0.65 * daylight) * mix(1.0, 0.80, density);
    let rim = warm * (edge * sun_up * (0.35 + 0.8 * sun_amount)) * (1.0 - storm * 0.75);
    let night = mix(vec3<f32>(0.05, 0.07, 0.11), vec3<f32>(0.10, 0.13, 0.20), sun_up);
    let colour = mix(night, lit + rim, daylight);

    let horizon_fade = ramp(0.0, 0.10, view_dir.y);
    let distance_fade = 1.0 - ramp(12000.0, 26000.0, distance_to_plane);
    let alpha = shaped * quality * horizon_fade * distance_fade * (0.94 + storm * 0.05);
    return vec4<f32>(colour, alpha);
}

// ---------------------------------------------------------------------------
// Shadow sampling
// ---------------------------------------------------------------------------

// PCF comparison lookup in one cascade. `world_position` should already be the
// fragment's world position; `normal` is the geometric face normal.
fn cascade_visibility(world_position: vec3<f32>, normal: vec3<f32>, cascade: i32) -> f32 {
    let texel = camera.shadow_texel[cascade];
    let offset_position = world_position + normal * (texel * camera.shadow_params.x);
    let clip = camera.shadow_view_proj[cascade] * vec4<f32>(offset_position, 1.0);
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * camera.shadow_scale[cascade];
    if (uv.x <= 0.0 || uv.y <= 0.0 || uv.x >= 1.0 || uv.y >= 1.0) {
        return 1.0;
    }
    let depth = ndc.z - camera.shadow_params.y;
    // Near-cascade shadows use the full 3×3 kernel (5×5 at Ultra). The far
    // cascades use one bilinear comparison tap: the linear comparison sampler
    // already filters a 2×2 footprint, and at 32 m and beyond the extra eight
    // taps cost several milliseconds across a 1080p frame for a barely visible
    // softening. This keeps PCF where it matters without paying for it in the
    // distance.
    if (cascade > 0) {
        return textureSampleCompareLevel(shadow_map, shadow_sampler, uv, cascade, depth);
    }
    var radius = i32(camera.shadow_params.z + 0.5);
    // Ultra's 5×5 kernel is reserved for truly close contact shadows; beyond
    // 16 m a 3×3 kernel is visually indistinguishable and noticeably cheaper
    // across a 1080p frame.
    if (radius > 1 && distance(world_position, camera.camera_pos.xyz) > 16.0) {
        radius = 1;
    }
    let texel_uv = camera.shadow_texel.w;
    var sum = 0.0;
    var taps = 0.0;
    // The loop bounds are constant; the uniform PCF radius masks the taps so a
    // single pipeline serves 3×3 and 5×5 filtering without a shader variant.
    for (var y = -2; y <= 2; y = y + 1) {
        if (abs(y) > radius) {
            continue;
        }
        for (var x = -2; x <= 2; x = x + 1) {
            if (abs(x) > radius) {
                continue;
            }
            let offset = vec2<f32>(f32(x), f32(y)) * texel_uv;
            sum = sum + textureSampleCompareLevel(shadow_map, shadow_sampler, uv + offset, cascade, depth);
            taps = taps + 1.0;
        }
    }
    return sum / max(taps, 1.0);
}

// Sun visibility for a fragment: selects the first cascade containing it and
// cross-fades into the next cascade over the last 15% of the range.
fn sun_visibility(world_position: vec3<f32>, normal: vec3<f32>, view_distance: f32) -> f32 {
    let count = i32(camera.shadow_split.w + 0.5);
    if (count <= 0) {
        return 1.0;
    }
    var visibility = 1.0;
    var found = false;
    for (var cascade = 0; cascade < 3; cascade = cascade + 1) {
        if (cascade >= count) {
            break;
        }
        if (found) {
            break;
        }
        if (view_distance <= camera.shadow_split[cascade]) {
            found = true;
            visibility = cascade_visibility(world_position, normal, cascade);
            if (cascade + 1 < count) {
                let split = camera.shadow_split[cascade];
                let blend_start = split * 0.85;
                if (view_distance > blend_start) {
                    let next = cascade_visibility(world_position, normal, cascade + 1);
                    let blend = smoothstep(blend_start, split, view_distance);
                    visibility = mix(visibility, next, blend);
                }
            }
        }
    }
    return visibility;
}

// Multiplier applied to the baked light for direct sun occlusion.
//
// The baked light already contains face brightness and AO; only the direct-sun
// share is attenuated. `smoothstep(ndl)` means faces turned away from the sun
// keep their ambient/AO shading unchanged, and the daylight factor keeps the
// effect subtle at night. The shadowed factor bottoms out at 45% so terrain in
// shadow stays readable under ambient sky light.
fn sun_light_factor(world_position: vec3<f32>, normal: vec3<f32>, view_distance: f32) -> f32 {
    if (camera.shadow_split.w < 0.5) {
        return 1.0;
    }
    let sun_dir = normalize(camera.sun.xyz);
    let sun_up = smoothstep(0.0, 0.08, sun_dir.y);
    if (sun_up <= 0.0) {
        return 1.0;
    }
    let ndl = max(dot(normal, sun_dir), 0.0);
    let direct = smoothstep(0.03, 0.25, ndl) * sun_up * clamp(camera.horizon.w, 0.0, 1.0);
    if (direct <= 0.0) {
        return 1.0;
    }
    let visibility = sun_visibility(world_position, normal, view_distance);
    let amount = (1.0 - visibility) * direct * camera.shadow_params.w;
    return 1.0 - amount * 0.55;
}

// Geometric face normal from screen-space derivatives. The mesher emits large
// flat greedy quads and no normal attribute, so the derivative normal is both
// exact for a quad and cheaper than widening the vertex format. It is flipped
// towards the camera so the sign convention matches the outward face.
fn face_normal(world_position: vec3<f32>) -> vec3<f32> {
    var normal = cross(dpdx(world_position), dpdy(world_position));
    let length_squared = dot(normal, normal);
    if (length_squared < 1e-12) {
        return vec3<f32>(0.0, 1.0, 0.0);
    }
    normal = normal * inverseSqrt(length_squared);
    if (dot(normal, camera.camera_pos.xyz - world_position) < 0.0) {
        normal = -normal;
    }
    return normal;
}

// Distance fog shared by the opaque, water and entity passes.
fn apply_fog(colour: vec3<f32>, view_distance: f32) -> vec3<f32> {
    let amount = ramp(camera.fog.x, camera.fog.y, view_distance);
    return mix(colour, camera.horizon.rgb, amount);
}

// Colour the aerial perspective approaches: a slightly blue horizon haze, not
// a flat grey wall. Both near and far terrain use it, so the seam matches.
fn haze_colour() -> vec3<f32> {
    return mix(camera.horizon.rgb, camera.zenith.rgb, 0.16);
}

// Colour the near/far seam fades towards. It sits between the haze and the
// horizon so the coarse heightfield and the textured near world meet in the
// same soft band instead of showing a hard style/luminance step.
fn seam_colour() -> vec3<f32> {
    // Close to the terrain's aerial colour rather than the bright horizon, so
    // the cross-fade reads as distance rather than a pale ring.
    return mix(haze_colour(), camera.horizon.rgb, 0.25);
}

// Cheap height fog: one exponential height term and one optical-depth term.
// Gated by `atmosphere.z` (High/Ultra), so Low and Medium pay one uniform test.
fn height_fog_amount(world_position: vec3<f32>, distance_to_eye: f32) -> f32 {
    let density = camera.atmosphere.z;
    if density <= 0.0 || distance_to_eye < 96.0 {
        return 0.0;
    }
    // Rational height falloff and optical depth instead of two exponentials:
    // the curve is visually indistinguishable at this density and measured
    // noticeably cheaper across a 1080p frame.
    let scale = max(camera.atmosphere.w, 1.0);
    let height = 1.0 / (1.0 + max(world_position.y - camera.water.w, 0.0) / scale);
    let optical_depth = density * distance_to_eye * height;
    return optical_depth / (1.0 + optical_depth);
}

// Aerial perspective plus the near/far seam cross-fade.
//
// The near world fades towards `seam_colour` over `seam - band .. seam`; the
// far heightfield starts at the same colour at `seam` and reaches its own
// shading by `seam + band`. Both sides therefore evaluate the *same* colour at
// the streamed-world edge, which masks the change from textured blocks to the
// coarse palette without a depth discontinuity or extra geometry.
fn apply_atmosphere(colour: vec3<f32>, world_position: vec3<f32>, far_side: bool, view_distance: f32) -> vec3<f32> {
    let seam = camera.atmosphere.x;
    let density = camera.atmosphere.z;
    // One branch for the cheap path: Low/Medium upload zeroes for both fields,
    // so their fragments never run the aerial-perspective or seam math.
    if seam <= 0.0 && (density <= 0.0 || view_distance < 96.0) {
        return colour;
    }
    var result = colour;
    let haziness = height_fog_amount(world_position, view_distance);
    if haziness > 0.0 {
        result = mix(result, haze_colour(), haziness);
    }
    if seam > 0.0 {
        // Squared horizontal distance: no square root for a radial band, which
        // measured ~1 ms at 1080p when computed with `distance()`.
        let band = max(camera.atmosphere.y, 1.0);
        let dx = world_position.x - camera.camera_pos.x;
        let dz = world_position.z - camera.camera_pos.z;
        let plane_squared = dx * dx + dz * dz;
        let seam_squared = seam * seam;
        let band_squared = band * band;
        // Skip the ramp and mix for the overwhelming majority of fragments that
        // are nowhere near the cross-fade band.
        let close_to_band = plane_squared > seam_squared - 2.0 * seam * band + band_squared
            && plane_squared < seam_squared + 2.0 * seam * band + band_squared;
        if close_to_band {
            var fade: f32;
            if far_side {
                let outer = seam + band;
                fade = clamp((outer * outer - plane_squared) / max(outer * outer - seam_squared, 1.0), 0.0, 1.0);
            } else {
                let inner = max(seam - band, 0.0);
                fade = clamp((plane_squared - inner * inner) / max(seam_squared - inner * inner, 1.0), 0.0, 1.0);
            }
            fade = fade * fade * (3.0 - 2.0 * fade);
            result = mix(result, seam_colour(), fade * 0.22);
        }
    }
    return result;
}

// Moving cloud shadows projected on the ground.
//
// The fragment is projected along the sun direction onto the same world-space
// cloud plane the sky pass shades, so a cloud crossing the sun darkens the
// terrain underneath it. One value-noise pair (two octaves of the same field)
// is enough at ground scale and much cheaper than the sky pass's four-octave
// domain-warped noise. `camera.graphics.w` is zero on Low/Medium and when the
// cloud layer is off, making the whole function a single uniform test.
fn cloud_shadow(world_position: vec3<f32>, view_distance: f32) -> f32 {
    var strength = camera.graphics.w;
    // Cloud shadows fade out into the haze; skipping the fetch for the far
    // heightfield keeps the 512-chunk vista affordable.
    if strength <= 0.0 || view_distance > 1536.0 {
        return 1.0;
    }
    strength = strength * (1.0 - smoothstep(1024.0, 1536.0, view_distance));
    let sun_dir = normalize(camera.sun.xyz);
    if sun_dir.y <= 0.03 {
        return 1.0;
    }
    let travel = (cloud_height() - world_position.y) / sun_dir.y;
    if travel <= 0.0 || travel > 20000.0 {
        return 1.0;
    }
    let world = world_position.xz + sun_dir.xz * travel;
    let wind = CLOUD_WIND * (camera.graphics.x * 0.004);
    let uv = world * 0.0055 + wind;
    // One bilinear fetch of the baked value-noise field, sharing the sky's
    // coverage threshold. At ground scale this is visually indistinguishable
    // from evaluating the noise per fragment, which measured ~4.5 ms at 1080p.
    let density = textureSampleLevel(cloud_density, cloud_density_sampler, uv, 0.0).r;
    let coverage = clamp(camera.graphics.y, 0.0, 1.0);
    let threshold = mix(0.62, 0.30, coverage);
    let occluded = smoothstep(threshold, threshold + 0.16, density);
    return 1.0 - occluded * strength;
}

// Vertex inputs carry no interpolation attribute in WGSL; `layer` is a flat
// varying on the vertex output / fragment input below, as integers must be.
struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: u32,
    // Packed light from `Vertex`: sky in the low byte, block light in the next
    // byte. Both already include face brightness, per-corner AO and quad tint.
    @location(3) light: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    // Split varyings interpolate smoothly across the triangle; the packed u32
    // input must never be interpolated itself (sky/block would carry into each
    // other's byte).
    @location(2) sky: f32,
    @location(3) block: f32,
    @location(4) world_position: vec3<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.layer = input.layer;
    output.sky = f32(input.light & 0xFFu) / 255.0;
    output.block = f32((input.light >> 8u) & 0xFFu) / 255.0;
    output.world_position = input.position;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Fire is an animated emissive cut-out. The one fire layer is sampled at a
    // wobbling UV and the alpha threshold pulses, so the ragged silhouette
    // dances without an animated texture atlas. The result is HDR (>1) so the
    // bloom pass gives it a warm halo in rain and at night; it uses no scene
    // light, so it reads the same under an overcast sky.
    if (input.layer == FIRE_LAYER) {
        let time = camera.graphics.x;
        let wobble = vec2<f32>(
            sin(time * 3.1 + input.world_position.x * 2.3) * 0.025,
            sin(time * 5.3 + input.world_position.z * 1.7) * 0.018,
        );
        let flame = textureSampleLevel(blocks, block_sampler, input.uv + wobble, input.layer, 0.0);
        let pulse = 0.80 + 0.20 * sin(time * 11.0 + input.world_position.x * 4.1 + input.world_position.z * 2.9)
            + 0.06 * sin(time * 27.0 + input.world_position.y * 7.0);
        if flame.a * pulse < 0.24 {
            discard;
        }
        // Keep most of the flame below the bloom threshold; only the hot core
        // reaches HDR so the fire reads orange instead of clipping to white.
        let core = 0.6 + 1.1 * pulse;
        let colour = flame.rgb * (1.0 + 0.35 * pulse) + vec3<f32>(0.34, 0.09, 0.01) * core;
        return vec4<f32>(colour, 1.0);
    }
    let texel = textureSample(blocks, block_sampler, input.uv, input.layer);
    // Alpha cut-out: leaf layers punch small transparent holes, everything else
    // is fully opaque. The chunk pipeline has no blending, so this stays a cheap
    // discard rather than a sorted transparent pass.
    if texel.a < 0.5 {
        discard;
    }
    let albedo = texel.rgb;
    let view_distance = distance(input.world_position, camera.camera_pos.xyz);
    let normal = face_normal(input.world_position);
    // Baked face light times the time-of-day multiplier and light tint. At noon
    // `world_light.rgb` is exactly (1,1,1), preserving the daylight look. The
    // shadow term removes only the direct-sun part on sun-facing geometry.
    let shadow = sun_light_factor(input.world_position, normal, view_distance);
    // Sky light follows the time of day and the tinted sun/moon palette; block
    // light is independent of the clock so a torch reads the same in a cave at
    // noon and at midnight. Torch light is slightly warm; the tint is scaled by
    // strength so a 1-level glow stays nearly white instead of orange.
    let warm = mix(vec3<f32>(1.0), vec3<f32>(1.0, 0.82, 0.58), input.block * 0.85);
    let sky_light = input.sky * camera.world_light.rgb * shadow;
    let block_light = input.block * warm;
    let lit = albedo * (sky_light * cloud_shadow(input.world_position, view_distance) + block_light);
    return vec4<f32>(apply_fog(apply_atmosphere(lit, input.world_position, false, view_distance), view_distance), 1.0);
}

// ---------------------------------------------------------------------------
// Transparent water
// ---------------------------------------------------------------------------

// Water needs the fragment's pixel coordinate to fetch the scene-depth copy
// (foam/depth response). The builtin `position` is available to a fragment
// entry point independently of the vertex output, so only the water entry
// declares it here.
struct WaterFragmentInput {
    @builtin(position) frag_coord: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    @location(2) sky: f32,
    @location(3) block: f32,
    @location(4) world_position: vec3<f32>,
};

// Eye-space distance from the stored NDC depth.
//
// For the right-handed zero-to-one perspective used by the renderer,
// `ndc = m32 / eye_depth - m22`; inverting is the expression below. The near
// and far planes come from the camera uniform, so the water pass needs no
// inverse projection matrix and no extra render target.
fn linear_eye_depth(depth: f32) -> f32 {
    let near = camera.clip.x;
    let far = camera.clip.y;
    return (near * far) / max(far - depth * (far - near), 1e-4);
}

@fragment
fn fs_water(input: WaterFragmentInput) -> @location(0) vec4<f32> {
    let texel = textureSample(blocks, block_sampler, input.uv, input.layer);
    if texel.a < 0.5 {
        discard;
    }
    let world_position = input.world_position;
    let view_distance = distance(world_position, camera.camera_pos.xyz);
    let base_normal = face_normal(world_position);
    let time = camera.graphics.x;
    let wave = camera.water.x;
    let underwater = camera.camera_pos.y < camera.water.w;
    // Two travelling wave bands (plus one finer diagonal band) perturb the
    // geometric normal. The scale is deliberately larger than the ripple so
    // the highlights drift rather than shimmer at 1080p.
    let p = world_position.xz;
    let ripple = vec2<f32>(
        sin(p.x * 0.55 + time * 1.6)
            + 0.5 * sin(p.y * 1.05 - time * 2.2)
            + 0.35 * sin((p.x + p.y) * 2.7 + time * 3.1),
        cos(p.y * 0.62 + time * 1.25)
            + 0.5 * cos(p.x * 0.95 + time * 1.9)
            + 0.35 * cos((p.x - p.y) * 3.3 - time * 2.6),
    );
    let normal = normalize(base_normal + vec3<f32>(ripple.x * 0.075 * wave, 0.0, ripple.y * 0.075 * wave));
    let view = normalize(camera.camera_pos.xyz - world_position);
    let fresnel = 0.02 + 0.98 * pow(1.0 - clamp(dot(normal, view), 0.0, 1.0), 5.0);

    let shadow = sun_light_factor(world_position, normal, view_distance);
    let sun_dir = normalize(camera.sun.xyz);
    let half_dir = normalize(view + sun_dir);
    let sun_up = smoothstep(0.0, 0.08, sun_dir.y);
    let ndh = max(dot(normal, half_dir), 0.0);
    // A tight glint plus a broader sheen reads as sun sparkle on waves instead
    // of one uniform bright spot.
    let glint = pow(ndh, 220.0) + 0.22 * pow(ndh, 42.0);
    let specular = glint * camera.water.z * sun_up * shadow * clamp(camera.sun.w, 0.0, 2.0);

    // Water thickness from the scene-depth copy (High/Ultra water only). The
    // scene depth is eye-space distance, so subtracting the surface's own eye
    // depth gives the water column under this fragment in blocks.
    let depth_available = camera.clip.z > 0.5;
    var thickness = 0.0;
    if depth_available {
        let scene_depth = textureLoad(scene_depth, vec2<i32>(input.frag_coord.xy), 0);
        let scene_eye = linear_eye_depth(scene_depth);
        let surface_eye = dot(camera.camera_pos.xyz - world_position, camera.basis_forward.xyz);
        thickness = clamp(scene_eye - surface_eye, 0.0, 12.0);
    }
    let depth_response = smoothstep(0.15, 3.0, thickness);

    // Shallow water lets a warm sandy bottom show through; deep water takes
    // the blue-green absorption tint. The depth response also drives opacity.
    let body_deep = texel.rgb * vec3<f32>(0.34, 0.65, 0.82) * (camera.world_light.rgb + input.sky * 0.15);
    let body_shallow = texel.rgb * vec3<f32>(1.02, 0.94, 0.72) * (camera.world_light.rgb * max(input.sky, 0.4));
    let mixed_body = select(body_deep, mix(body_shallow, body_deep, depth_response), depth_available);

    // Reflect the sky about the perturbed normal (cheap environment
    // reflection): the horizon near glancing angles, the zenith when the
    // reflection points up.
    let reflected = reflect(-view, normal);
    let sky_reflection = mix(camera.horizon.rgb, camera.zenith.rgb, smoothstep(0.0, 0.45, reflected.y));
    var colour = mix(mixed_body, sky_reflection, fresnel * 0.62);
    // Water receives the same cascade shadows as opaque terrain; the direct-sun
    // share is what makes water bright, ambient reflection remains.
    colour = colour * (0.58 + 0.42 * shadow);
    colour = colour + camera.sun_colour.rgb * specular * 0.58;
    colour = colour * cloud_shadow(world_position, view_distance);

    // Shoreline foam: a soft animated band where the column is very thin. It
    // fades out with distance and with thickness, so there is no hard edge at a
    // block boundary and no halo where the depth copy is unavailable.
    var alpha = camera.water.y * texel.a;
    if depth_available {
        let foam_edge = 1.0 - smoothstep(0.0, 0.5, thickness);
        let foam_noise = 0.5 + 0.5 * sin(p.x * 2.1 + time * 1.3) * cos(p.y * 1.7 - time * 1.1);
        let foam = foam_edge * (0.45 + 0.55 * foam_noise) * (1.0 - smoothstep(40.0, 120.0, view_distance));
        let foam_colour = mix(camera.world_light.rgb * 0.86, camera.sun_colour.rgb * 0.25, 0.3);
        colour = mix(colour, foam_colour, foam * 0.5);
        let shallow_alpha = camera.water.y * 0.5;
        alpha = mix(shallow_alpha, camera.water.y, depth_response) * texel.a;
        alpha = mix(alpha, 0.90, foam * 0.6);
    }
    alpha = clamp(mix(alpha, 0.92, fresnel * 0.62), 0.04, 0.96);
    if underwater {
        // Seen from below the surface is a dim, saturated blue sheet rather than
        // a sky mirror. The full-screen underwater tint is applied in post.
        colour = mix(colour, vec3<f32>(0.03, 0.12, 0.18) * (0.5 + 0.5 * camera.world_light.r), 0.45);
        alpha = mix(alpha, 0.85, 0.3);
    }
    colour = apply_fog(apply_atmosphere(colour, world_position, false, view_distance), view_distance);
    return vec4<f32>(colour, alpha);
}

// ---------------------------------------------------------------------------
// Shadow depth pass
// ---------------------------------------------------------------------------

@vertex
fn vs_shadow(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = camera.view_proj * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    output.layer = input.layer;
    output.sky = f32(input.light & 0xFFu) / 255.0;
    output.block = f32((input.light >> 8u) & 0xFFu) / 255.0;
    output.world_position = input.position;
    return output;
}

@fragment
fn fs_shadow_alpha(input: VertexOutput) {
    let texel = textureSample(blocks, block_sampler, input.uv, input.layer);
    if (texel.a < 0.5) {
        discard;
    }
}

// ---------------------------------------------------------------------------
// Sky
// ---------------------------------------------------------------------------

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
    // Depth 1.0 with `LessEqual` and no depth writes: the sky pass runs after the
    // opaque world and only fills pixels where the depth buffer is still clear.
    // This avoids shading clouds and stars behind terrain.
    output.clip_position = vec4<f32>(position, 1.0, 1.0);
    output.ndc = position;
    return output;
}

@fragment
fn fs_sky(input: SkyOutput) -> @location(0) vec4<f32> {
    let ray = view_ray(input.ndc);
    var colour = sky_colour(input.ndc.y);
    // A soft band just above the horizon; zero exactly at the horizon so the
    // fog colour and the horizon row stay identical.
    let haze_band = smoothstep(0.0, 0.12, ray.y) * (1.0 - smoothstep(0.12, 0.45, ray.y));
    colour += camera.horizon.rgb * (haze_band * 0.06 * camera.zenith.w);
    colour += sun_contribution(ray);
    colour += moon_contribution(ray);
    colour += star_contribution(ray);
    // Clouds occlude stars and the discs, exactly as a real cloud deck would.
    let clouds = cloud_contribution(ray);
    colour = mix(colour, clouds.rgb, clamp(clouds.a, 0.0, 1.0));
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

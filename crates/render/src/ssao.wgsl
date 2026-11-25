//! Half-resolution screen-space ambient occlusion reconstructed from depth.
//!
//! There is no normal buffer in the shipping renderer, so the view-space normal
//! is derived from the reconstructed positions of the pixel and its right/up
//! neighbours. Six hemisphere taps (rotated by a per-pixel hash) compare each
//! sample against the stored depth; the raw result is then depth-aware blurred
//! by `fs_ssao_blur` and multiplied into the HDR scene by the post pass.
//!
//! Guards keep the effect free of halos and cheap where it does not belong:
//! sky pixels (depth 1.0) output 1.0, the effect fades out between 96 and 192 m
//! (the far heightfield and the surface-shell LOD stay untouched), and the
//! output is clamped so a fully occluded crevice never goes black. The whole
//! pass is skipped on Low/Medium via `GraphicsSettings::ssao_enabled`.

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
var depth_texture: texture_depth_2d;

// Eye-space distance from the stored NDC depth. Same projection algebra as
// the water shader: the standard right-handed zero-to-one inverse.
fn eye_depth(ndc_depth: f32) -> f32 {
    let near = ssao.params.z;
    let far = ssao.params.w;
    return (near * far) / max(far - ndc_depth * (far - near), 1e-4);
}

fn view_position_from_depth(uv: vec2<f32>, depth: f32) -> vec3<f32> {
    let z = eye_depth(depth);
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    return vec3<f32>(ndc.x * ssao.view.x * ssao.view.y * z, ndc.y * ssao.view.x * z, -z);
}

fn view_position(uv: vec2<f32>) -> vec3<f32> {
    let pixel = vec2<i32>(clamp(uv * ssao.screen.xy, vec2<f32>(0.0), ssao.screen.xy - vec2<f32>(1.0)));
    return view_position_from_depth(uv, textureLoad(depth_texture, pixel, 0));
}

fn project_view(p: vec3<f32>) -> vec2<f32> {
    let z = max(-p.z, 1e-4);
    let ndc = vec2<f32>(p.x / (ssao.view.x * ssao.view.y * z), p.y / (ssao.view.x * z));
    return vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

fn hash21(p: vec2<f32>) -> f32 {
    var state = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    state = state + vec3<f32>(dot(state, state.yzx + 33.33));
    return fract((state.x + state.y) * state.z);
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
fn fs_ssao(input: FullscreenOutput) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(clamp(input.uv * ssao.screen.xy, vec2<f32>(0.0), ssao.screen.xy - vec2<f32>(1.0)));
    let centre_depth = textureLoad(depth_texture, pixel, 0);
    // Sky and any cleared pixel keep a neutral AO.
    if centre_depth >= 1.0 {
        return vec4<f32>(1.0);
    }
    let centre = view_position(input.uv);
    let centre_distance = length(centre);
    let fade = 1.0 - smoothstep(96.0, 192.0, centre_distance);
    if fade <= 0.0 {
        return vec4<f32>(1.0);
    }

    // View-space normal from the depth-reconstructed right/up neighbours. The
    // half-resolution target samples the full-resolution depth at one point per
    // output pixel, so the derivative spans two pixels; both the projection and
    // the orientation guard are still exact for a flat surface.
    let right = view_position(input.uv + vec2<f32>(ssao.params.x * 2.0, 0.0));
    let up = view_position(input.uv + vec2<f32>(0.0, ssao.params.y * 2.0));
    var normal = cross(right - centre, up - centre);
    let length_squared = dot(normal, normal);
    if length_squared < 1e-10 {
        return vec4<f32>(1.0);
    }
    normal = normal * inverseSqrt(length_squared);
    if dot(normal, -centre) < 0.0 {
        normal = -normal;
    }

    // Tangent frame around the normal. The helper axis must never be parallel
    // to the normal: flat ground has an exactly-up normal, where the naive
    // `cross(normal, up)` degenerates to NaN and every tap is discarded.
    var helper = vec3<f32>(0.0, 1.0, 0.0);
    if abs(normal.y) > 0.9 {
        helper = vec3<f32>(1.0, 0.0, 0.0);
    }
    let tangent = normalize(cross(normal, helper));
    let bitangent = cross(normal, tangent);

    let radius = clamp(centre_distance * ssao.view.z, 0.6, ssao.view.w);
    let rotation = hash21(input.uv * ssao.screen.zw) * 6.2831853;
    let cos_rotation = cos(rotation);
    let sin_rotation = sin(rotation);

    // Six taps: a spiral in the tangent plane with a raised centre, weighted
    // slightly towards the normal so flat ground gets a uniform, stable term.
    var occlusion = 0.0;
    var accepted = 0.0;
    for (var index = 0; index < 6; index = index + 1) {
        let fi = f32(index);
        let angle = fi * 2.3999632;
        let ring = 0.35 + 0.65 * (fi / 6.0);
        let offset = vec2<f32>(cos(angle) * ring, sin(angle) * ring);
        // Rotate the tap pattern per pixel to turn banding into high-frequency
        // noise the blur then removes.
        let rotated = vec2<f32>(
            offset.x * cos_rotation - offset.y * sin_rotation,
            offset.x * sin_rotation + offset.y * cos_rotation,
        );
        let sample_point = centre
            + (tangent * rotated.x + bitangent * rotated.y) * radius
            + normal * (radius * 0.45);
        let sample_uv = project_view(sample_point);
        if sample_uv.x <= 0.0 || sample_uv.y <= 0.0 || sample_uv.x >= 1.0 || sample_uv.y >= 1.0 {
            continue;
        }
        let sample_depth = textureLoad(
            depth_texture,
            vec2<i32>(clamp(sample_uv * ssao.screen.xy, vec2<f32>(0.0), ssao.screen.xy - vec2<f32>(1.0))),
            0,
        );
        // Never treat the sky behind an edge as an occluder: it would draw a
        // dark halo around every silhouette.
        if sample_depth >= 1.0 {
            accepted = accepted + 1.0;
            continue;
        }
        // Reuse the depth already fetched for this tap instead of sampling it
        // a second time through `view_position`; that halves the tap loads.
        let stored_depth = -view_position_from_depth(sample_uv, sample_depth).z;
        let difference = -sample_point.z - stored_depth;
        let range = radius * 2.5;
        if difference > 0.01 && difference < range {
            occlusion = occlusion + (1.0 - clamp(difference / range, 0.0, 1.0));
        }
        accepted = accepted + 1.0;
    }

    let normalized = occlusion / max(accepted, 1.0);
    let ao = clamp(1.0 - normalized * 1.1 * fade, 0.30, 1.0);
    return vec4<f32>(ao);
}


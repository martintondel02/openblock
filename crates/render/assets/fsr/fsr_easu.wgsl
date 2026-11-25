//! AMD FidelityFX Super Resolution 1.0 — EASU (Edge Adaptive Spatial
//! Upsampling) pass, WGSL port.
//!
//! Adapted from `ffx-fsr/ffx_fsr1.h` in
//! <https://github.com/GPUOpen-Effects/FidelityFX-FSR> (v1.20210629).
//! The original is Copyright (c) 2021 Advanced Micro Devices, Inc. and is
//! distributed under the MIT license; see `LICENSE-FSR.txt` in this directory.
//!
//! FSR1 expects a *perceptual* (sRGB- or gamma-encoded) input and produces a
//! perceptual image. The input texture is usually `Rgba8UnormSrgb`, so the
//! hardware decodes it to linear when gathering; `encode_srgb` puts each
//! gathered channel back into perceptual space before the EASU filter runs.
//! When the input texture is a non-sRGB `*Unorm` format its contents are
//! assumed to already be perceptual and no conversion is applied.
//!
//! The 12-tap kernel, the four `textureGather` calls and the constants
//! (`con0..con3`) follow the non-packed 32-bit reference implementation.
//! Approximate reciprocal/square-root intrinsics are replaced by exact
//! operations with explicit zero guards, which is a numerical-quality
//! improvement over the reference and preserves its output for finite input.

// SPDX-FileCopyrightText: 2021 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: MIT

struct FsrEasuUniform {
    // Output pixel position -> input pixel position:
    //   pp = ip * con0.xy + con0.zw
    con0: vec4<f32>,
    // Input texel size, then the offset from the upper-left of the 'F' tap to
    // the first gather center.
    con1: vec4<f32>,
    // Offsets from the first gather center to gathers 1 and 2.
    con2: vec4<f32>,
    // Offset from the first gather center to gather 3.
    con3: vec4<f32>,
    // x: 1.0 when the input texture format is sRGB and must be re-encoded to
    // perceptual space after sampling; yzw reserved.
    params: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> easu: FsrEasuUniform;

@group(0) @binding(1)
var input_tex: texture_2d<f32>;

@group(0) @binding(2)
var input_sampler: sampler;

struct FsrVertexOutput {
    @builtin(position) position: vec4<f32>,
};

// Full-screen triangle. EASU uses the fragment position, not interpolated
// UVs, because the reference algorithm is defined on integer output pixels.
@vertex
fn vs_fsr(@builtin(vertex_index) vertex_index: u32) -> FsrVertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var output: FsrVertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    return output;
}

fn srgb_encode_channel(value: f32) -> f32 {
    let x = clamp(value, 0.0, 1.0);
    let linear = x * 12.92;
    let curved = 1.055 * pow(x, 1.0 / 2.4) - 0.055;
    return select(curved, linear, x <= 0.0031308);
}

// The four gathered values are four samples of one channel, so every
// component needs the same transfer function.
fn srgb_encode_vec4(value: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(
        srgb_encode_channel(value.x),
        srgb_encode_channel(value.y),
        srgb_encode_channel(value.z),
        srgb_encode_channel(value.w),
    );
}

// The gather component must be a const-expression in WGSL, so the three
// channel reads are separate helpers into the same code path.
fn encode_gather(raw: vec4<f32>) -> vec4<f32> {
    if (easu.params.x > 0.5) {
        return srgb_encode_vec4(raw);
    }
    return raw;
}

fn gather_r(coord: vec2<f32>) -> vec4<f32> {
    return encode_gather(textureGather(0u, input_tex, input_sampler, coord));
}

fn gather_g(coord: vec2<f32>) -> vec4<f32> {
    return encode_gather(textureGather(1u, input_tex, input_sampler, coord));
}

fn gather_b(coord: vec2<f32>) -> vec4<f32> {
    return encode_gather(textureGather(2u, input_tex, input_sampler, coord));
}

// Exact reciprocal with a zero guard. The reference uses a hardware
// approximate reciprocal; returning 0 for a zero denominator keeps the flat
// (zero gradient) case finite instead of producing 0 * inf = NaN.
fn safe_rcp(value: f32) -> f32 {
    if (value > 1.0e-20) {
        return 1.0 / value;
    }
    return 0.0;
}

struct EasuDirection {
    dir: vec2<f32>,
    len: f32,
};

// Accumulates direction and length from one of the four bilinear quadrants.
fn easu_set(weight: f32, l_a: f32, l_b: f32, l_c: f32, l_d: f32, l_e: f32) -> EasuDirection {
    // Direction is the '+' difference, with the length taken from the
    // magnitude of the absolute average on both sides of the center tap.
    let dc = l_d - l_c;
    let cb = l_c - l_b;
    let len_x = safe_rcp(max(abs(dc), abs(cb)));
    let dir_x = l_d - l_b;
    var len_x_term = clamp(abs(dir_x) * len_x, 0.0, 1.0);
    len_x_term *= len_x_term;

    let ec = l_e - l_c;
    let ca = l_c - l_a;
    let len_y = safe_rcp(max(abs(ec), abs(ca)));
    let dir_y = l_e - l_a;
    var len_y_term = clamp(abs(dir_y) * len_y, 0.0, 1.0);
    len_y_term *= len_y_term;

    return EasuDirection(
        vec2<f32>(dir_x * weight, dir_y * weight),
        (len_x_term + len_y_term) * weight,
    );
}

struct EasuTap {
    colour: vec3<f32>,
    weight: f32,
};

// One anisotropically stretched Lanczos-2 tap. The window is clipped so taps
// outside the 2x2 nearest-texel neighborhood cannot dominate.
fn easu_tap(off: vec2<f32>, dir: vec2<f32>, len: vec2<f32>, lob: f32, clp: f32, colour: vec3<f32>) -> EasuTap {
    // Rotate the offset by the gradient direction.
    var v = vec2<f32>(
        off.x * dir.x + off.y * dir.y,
        off.x * (-dir.y) + off.y * dir.x,
    );
    v *= len;
    let d2 = min(v.x * v.x + v.y * v.y, clp);

    // Approximation of lanczos2 without sin, rcp or sqrt from the reference:
    //   (25/16 * (2/5 * x^2 - 1)^2 - (25/16 - 1)) * (1/4 * x^2 - 1)^2
    var w_b = (2.0 / 5.0) * d2 - 1.0;
    var w_a = lob * d2 - 1.0;
    w_b *= w_b;
    w_a *= w_a;
    w_b = (25.0 / 16.0) * w_b - (25.0 / 16.0 - 1.0);
    let w = w_b * w_a;
    return EasuTap(colour * w, w);
}

fn luma_times_two(r: vec4<f32>, g: vec4<f32>, b: vec4<f32>) -> vec4<f32> {
    return b * 0.5 + (r * 0.5 + g);
}

fn min3(a: vec3<f32>, b: vec3<f32>, c: vec3<f32>) -> vec3<f32> {
    return min(a, min(b, c));
}

fn max3(a: vec3<f32>, b: vec3<f32>, c: vec3<f32>) -> vec3<f32> {
    return max(a, max(b, c));
}

@fragment
fn fs_easu(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // Integer output pixel position, matching the reference's `ip`.
    let ip = floor(position.xy);

    // Position of the 'f' tap in input pixel space.
    let pp = ip * easu.con0.xy + easu.con0.zw;
    let fp = floor(pp);
    let ppf = pp - fp;

    // 12-tap kernel (16 fetched, 4 unused):
    //     b c
    //   e f g h
    //   i j k l
    //     n o
    // The four gathers are 2x2 groups. WGSL's textureGather ordering is
    // x=(u min,v max), y=(u max,v max), z=(u max,v min), w=(u min,v min),
    // which is the order the reference shader expects.
    let p0 = fp * easu.con1.xy + easu.con1.zw;
    let p1 = p0 + easu.con2.xy;
    let p2 = p0 + easu.con2.zw;
    let p3 = p0 + easu.con3.xy;

    let bczz_r = gather_r(p0);
    let bczz_g = gather_g(p0);
    let bczz_b = gather_b(p0);
    let ijfe_r = gather_r(p1);
    let ijfe_g = gather_g(p1);
    let ijfe_b = gather_b(p1);
    let klhg_r = gather_r(p2);
    let klhg_g = gather_g(p2);
    let klhg_b = gather_b(p2);
    let zzon_r = gather_r(p3);
    let zzon_g = gather_g(p3);
    let zzon_b = gather_b(p3);

    let bczz_l = luma_times_two(bczz_r, bczz_g, bczz_b);
    let ijfe_l = luma_times_two(ijfe_r, ijfe_g, ijfe_b);
    let klhg_l = luma_times_two(klhg_r, klhg_g, klhg_b);
    let zzon_l = luma_times_two(zzon_r, zzon_g, zzon_b);

    let b_l = bczz_l.x;
    let c_l = bczz_l.y;
    let i_l = ijfe_l.x;
    let j_l = ijfe_l.y;
    let f_l = ijfe_l.z;
    let e_l = ijfe_l.w;
    let k_l = klhg_l.x;
    let l_l = klhg_l.y;
    let h_l = klhg_l.z;
    let g_l = klhg_l.w;
    let o_l = zzon_l.z;
    let n_l = zzon_l.w;

    // Bilinear weights for the four quadrants around 'f'.
    let w_s = (1.0 - ppf.x) * (1.0 - ppf.y);
    let w_t = ppf.x * (1.0 - ppf.y);
    let w_u = (1.0 - ppf.x) * ppf.y;
    let w_v = ppf.x * ppf.y;

    let dir_s = easu_set(w_s, b_l, e_l, f_l, g_l, j_l);
    let dir_t = easu_set(w_t, c_l, f_l, g_l, h_l, k_l);
    let dir_u = easu_set(w_u, f_l, i_l, j_l, k_l, n_l);
    let dir_v = easu_set(w_v, g_l, j_l, k_l, l_l, o_l);

    let dir = dir_s.dir + dir_t.dir + dir_u.dir + dir_v.dir;
    let edge_len = dir_s.len + dir_t.len + dir_u.len + dir_v.len;

    // Normalize the gradient and clean up the near-zero case.
    let zero = (dir.x * dir.x + dir.y * dir.y) < (1.0 / 32768.0);
    let dir_scale = select(inverseSqrt(dir.x * dir.x + dir.y * dir.y), 1.0, zero);
    let direction = select(dir * dir_scale, vec2<f32>(1.0, 0.0), zero);

    // Transform the edge length from {0..2} to {0..1} and shape it with a
    // square. Then stretch the kernel on diagonals.
    var len = edge_len * 0.5;
    len *= len;
    let stretch = (direction.x * direction.x + direction.y * direction.y)
        * safe_rcp(max(abs(direction.x), abs(direction.y)));
    let len2 = vec2<f32>(
        1.0 + (stretch - 1.0) * len,
        1.0 - 0.5 * len,
    );
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;

    let b = vec3<f32>(bczz_r.x, bczz_g.x, bczz_b.x);
    let c = vec3<f32>(bczz_r.y, bczz_g.y, bczz_b.y);
    let i = vec3<f32>(ijfe_r.x, ijfe_g.x, ijfe_b.x);
    let j = vec3<f32>(ijfe_r.y, ijfe_g.y, ijfe_b.y);
    let f = vec3<f32>(ijfe_r.z, ijfe_g.z, ijfe_b.z);
    let e = vec3<f32>(ijfe_r.w, ijfe_g.w, ijfe_b.w);
    let k = vec3<f32>(klhg_r.x, klhg_g.x, klhg_b.x);
    let l = vec3<f32>(klhg_r.y, klhg_g.y, klhg_b.y);
    let h = vec3<f32>(klhg_r.z, klhg_g.z, klhg_b.z);
    let g = vec3<f32>(klhg_r.w, klhg_g.w, klhg_b.w);
    let o = vec3<f32>(zzon_r.z, zzon_g.z, zzon_b.z);
    let n = vec3<f32>(zzon_r.w, zzon_g.w, zzon_b.w);

    // The 2x2 nearest neighbors clamp the result, removing the negative-lobe
    // ringing while keeping the edge-adaptive reconstruction.
    let min4 = min(min3(f, g, j), k);
    let max4 = max(max3(f, g, j), k);

    let tap_b = easu_tap(vec2<f32>(0.0, -1.0) - ppf, direction, len2, lob, clp, b);
    let tap_c = easu_tap(vec2<f32>(1.0, -1.0) - ppf, direction, len2, lob, clp, c);
    let tap_i = easu_tap(vec2<f32>(-1.0, 1.0) - ppf, direction, len2, lob, clp, i);
    let tap_j = easu_tap(vec2<f32>(0.0, 1.0) - ppf, direction, len2, lob, clp, j);
    let tap_f = easu_tap(vec2<f32>(0.0, 0.0) - ppf, direction, len2, lob, clp, f);
    let tap_e = easu_tap(vec2<f32>(-1.0, 0.0) - ppf, direction, len2, lob, clp, e);
    let tap_k = easu_tap(vec2<f32>(1.0, 1.0) - ppf, direction, len2, lob, clp, k);
    let tap_l = easu_tap(vec2<f32>(2.0, 1.0) - ppf, direction, len2, lob, clp, l);
    let tap_h = easu_tap(vec2<f32>(2.0, 0.0) - ppf, direction, len2, lob, clp, h);
    let tap_g = easu_tap(vec2<f32>(1.0, 0.0) - ppf, direction, len2, lob, clp, g);
    let tap_o = easu_tap(vec2<f32>(1.0, 2.0) - ppf, direction, len2, lob, clp, o);
    let tap_n = easu_tap(vec2<f32>(0.0, 2.0) - ppf, direction, len2, lob, clp, n);

    let accumulated = tap_b.colour + tap_c.colour + tap_i.colour + tap_j.colour
        + tap_f.colour + tap_e.colour + tap_k.colour + tap_l.colour
        + tap_h.colour + tap_g.colour + tap_o.colour + tap_n.colour;
    let total_weight = tap_b.weight + tap_c.weight + tap_i.weight + tap_j.weight
        + tap_f.weight + tap_e.weight + tap_k.weight + tap_l.weight
        + tap_h.weight + tap_g.weight + tap_o.weight + tap_n.weight;

    let resolved = clamp(accumulated * safe_rcp(total_weight), min4, max4);
    return vec4<f32>(resolved, 1.0);
}

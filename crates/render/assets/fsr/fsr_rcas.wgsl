//! AMD FidelityFX Super Resolution 1.0 — RCAS (Robust Contrast Adaptive
//! Sharpening) pass, WGSL port.
//!
//! Adapted from `ffx-fsr/ffx_fsr1.h` in
//! <https://github.com/GPUOpen-Effects/FidelityFX-FSR> (v1.20210629).
//! The original is Copyright (c) 2021 Advanced Micro Devices, Inc. and is
//! distributed under the MIT license; see `LICENSE-FSR.txt` in this directory.
//!
//! RCAS runs on the output-resolution perceptual image written by the EASU
//! pass (a non-sRGB `Rgba8Unorm` intermediate owned by `FsrUpscaler`). It uses
//! only texture loads and no sampler, exactly like the reference.
//!
//! The destination is normally an sRGB surface/offscreen texture: the final
//! contrast-adaptive value is decoded back to linear and the hardware performs
//! the sRGB encode on store. For a non-sRGB destination the perceptual value is
//! written directly. `FsrUpscaler` sets `params.y` accordingly.

// SPDX-FileCopyrightText: 2021 Advanced Micro Devices, Inc.
// SPDX-License-Identifier: MIT

struct FsrRcasUniform {
    // x: exp2(-sharpness_stops), the reference RCAS lobe scale.
    // y: 1.0 when the destination is an sRGB format (decode before store).
    // zw: reserved.
    params: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> rcas: FsrRcasUniform;

@group(0) @binding(1)
var source: texture_2d<f32>;

struct FsrVertexOutput {
    @builtin(position) position: vec4<f32>,
};

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

fn srgb_decode_channel(value: f32) -> f32 {
    let x = clamp(value, 0.0, 1.0);
    let linear = x / 12.92;
    let curved = pow((x + 0.055) / 1.055, 2.4);
    return select(curved, linear, x <= 0.04045);
}

fn srgb_decode_vec3(value: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        srgb_decode_channel(value.x),
        srgb_decode_channel(value.y),
        srgb_decode_channel(value.z),
    );
}

fn load_rgb(coord: vec2<i32>, dimensions: vec2<i32>) -> vec3<f32> {
    let clamped = clamp(coord, vec2<i32>(0), dimensions - vec2<i32>(1));
    return textureLoad(source, clamped, 0).rgb;
}

// Exact division with a zero guard. The reference uses fast reciprocal
// approximations; guarding only the exact-zero denominator keeps the flat
// black/white cases finite (the reference's 0 * inf would be NaN).
fn safe_ratio(numerator: f32, denominator: f32) -> f32 {
    if (abs(denominator) > 1.0e-20) {
        return numerator / denominator;
    }
    return 0.0;
}

fn luma_times_two(colour: vec3<f32>) -> f32 {
    return colour.b * 0.5 + (colour.r * 0.5 + colour.g);
}

@fragment
fn fs_rcas(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    // RCAS limit: the point at which sharpening starts to look unnatural.
    let rcas_limit = 0.25 - (1.0 / 16.0);

    let dimensions = vec2<i32>(textureDimensions(source));
    let ip = vec2<i32>(floor(position.xy));

    // Minimal 3x3 neighborhood:
    //     b
    //   d e f
    //     h
    let b = load_rgb(ip + vec2<i32>(0, -1), dimensions);
    let d = load_rgb(ip + vec2<i32>(-1, 0), dimensions);
    let e = load_rgb(ip, dimensions);
    let f = load_rgb(ip + vec2<i32>(1, 0), dimensions);
    let h = load_rgb(ip + vec2<i32>(0, 1), dimensions);

    let b_l = luma_times_two(b);
    let d_l = luma_times_two(d);
    let e_l = luma_times_two(e);
    let f_l = luma_times_two(f);
    let h_l = luma_times_two(h);

    // Noise detection (the reference's FSR_RCAS_DENOISE path; always enabled
    // here because it costs two instructions and avoids sharpening grain).
    let neighborhood_centre = 0.25 * b_l + 0.25 * d_l + 0.25 * f_l + 0.25 * h_l;
    let ring_low = min(min(min(b_l, d_l), min(e_l, f_l)), h_l);
    let ring_high = max(max(max(b_l, d_l), max(e_l, f_l)), h_l);
    var noise = clamp(safe_ratio(abs(neighborhood_centre - e_l), ring_high - ring_low), 0.0, 1.0);
    noise = -0.5 * noise + 1.0;

    // Min/max of the four-neighbor ring, then the contrast-adaptive lobe.
    let mn4_r = min(min(b.r, d.r), min(f.r, h.r));
    let mn4_g = min(min(b.g, d.g), min(f.g, h.g));
    let mn4_b = min(min(b.b, d.b), min(f.b, h.b));
    let mx4_r = max(max(b.r, d.r), max(f.r, h.r));
    let mx4_g = max(max(b.g, d.g), max(f.g, h.g));
    let mx4_b = max(max(b.b, d.b), max(f.b, h.b));

    // Immediate constants for the peak range from the reference.
    let peak_white = 1.0;
    let peak_black = -4.0;

    let hit_min_r = safe_ratio(min(mn4_r, e.r), 4.0 * mx4_r);
    let hit_min_g = safe_ratio(min(mn4_g, e.g), 4.0 * mx4_g);
    let hit_min_b = safe_ratio(min(mn4_b, e.b), 4.0 * mx4_b);
    let hit_max_r = safe_ratio(peak_white - max(mx4_r, e.r), 4.0 * mn4_r + peak_black);
    let hit_max_g = safe_ratio(peak_white - max(mx4_g, e.g), 4.0 * mn4_g + peak_black);
    let hit_max_b = safe_ratio(peak_white - max(mx4_b, e.b), 4.0 * mn4_b + peak_black);

    let lobe_r = max(-hit_min_r, hit_max_r);
    let lobe_g = max(-hit_min_g, hit_max_g);
    let lobe_b = max(-hit_min_b, hit_max_b);
    let lobe = clamp(max(lobe_r, max(lobe_g, lobe_b)), -rcas_limit, 0.0) * rcas.params.x;

    // Apply noise removal.
    let denoised_lobe = lobe * noise;

    // Resolve.
    let reciprocal = 1.0 / (4.0 * denoised_lobe + 1.0);
    var resolved = (b + d + h + f) * denoised_lobe + e;
    resolved *= reciprocal;
    resolved = clamp(resolved, vec3<f32>(0.0), vec3<f32>(1.0));

    if (rcas.params.y > 0.5) {
        return vec4<f32>(srgb_decode_vec3(resolved), 1.0);
    }
    return vec4<f32>(resolved, 1.0);
}

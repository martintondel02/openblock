//! Low-detail shadow casters for the far cascade.
//!
//! Rendering every full-detail section into the far cascade costs thousands of
//! draw calls for shadows that are a few pixels wide. Beyond the near cascade we
//! instead rasterise one instanced unit cube per section/far-batch AABB. The
//! silhouette is blockier than the real geometry, but the cascade covers 64 m
//! and beyond, where PCF and distance fog hide the difference; the near cascade
//! keeps full-detail tree and terrain shadows.
//!
//! One instanced draw per cascade replaces hundreds of per-section draws, which
//! is the single biggest win for the 512-block frame budget.

struct Cascade {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> cascade: Cascade;

struct ProxyInput {
    // Unit-cube corner in 0..1.
    @location(0) position: vec3<f32>,
    // World-space AABB minimum.
    @location(1) minimum: vec3<f32>,
    // World-space AABB maximum.
    @location(2) maximum: vec3<f32>,
};

@vertex
fn vs_proxy(input: ProxyInput) -> @builtin(position) vec4<f32> {
    let world = mix(input.minimum, input.maximum, input.position);
    return cascade.view_proj * vec4<f32>(world, 1.0);
}

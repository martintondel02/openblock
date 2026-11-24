# ADR-0003 — wgpu-first renderer

> Status: accepted · Date: 2026-11-24 · Decisions: DEC-011 … DEC-018

## Context

The renderer must deliver 512 blocks of view at 120–144 FPS on mid-range
hardware, modern visual effects, a shader-pack ecosystem, and support for
Windows, Linux and macOS at 1.0 with Android later. Graphics API candidates:

- **wgpu 30** — safe Rust WebGPU implementation over Vulkan/DX12/Metal, with a
  future WebGPU path.
- **ash 0.38** — raw Vulkan bindings; maximum control, Vulkan only.
- **vulkano 0.35** — safe Vulkan wrapper; mature but Vulkan only.
- **glow** — OpenGL/GLES bindings; outdated feature set.

The renderer's performance-critical operations are: culling millions of blocks
worth of section geometry, submitting thousands of section draws, streaming
geometry and textures with low latency, and shading a few thousand quads per
frame at high resolution. Most of these are about batching, resource layout and
frame pacing, not exotic API features.

## Decision

Build on **wgpu 30**, request an explicit, adapter-clamped limit/feature set,
and design for the lowest common denominator:

- Use **multi-draw-indirect** where available and a zero-instance-argument
  fallback where it is not (Metal lacks the count form).
- Use **binding arrays** with a documented cap (≥ 4,096 texture layers on
  desktop, 1,024 on mobile) rather than requiring desktop-class 65,536-element
  arrays.
- Do **not** depend on mesh shaders, ray tracing, async compute queues or
  push-constant-style immediates in v1; they are experimental, absent or
  backend-specific in wgpu 30.
- Keep the renderer behind a device abstraction so a targeted `ash` backend can
  be added if a measured gap on a reference scene exceeds 20% and cannot be
  closed otherwise.

## Consequences

- Portability across Vulkan, DX12 and Metal from one codebase, which matters for
  contributors, players and CI.
- Some platform-specific fallbacks (notably Metal's emulated multi-draw and
  missing indirect-count) must be designed in from the start instead of
  discovered in a port.
- wgpu validation overhead is real on hot paths; the design compensates with
  cached bind groups/pipelines, arenas and one-time resource creation per frame
  rather than per-draw churn.
- No true async compute on a single queue; culling and shading compete for GPU
  time, which is accounted for in the budget.
- WebGPU/web deployment remains possible later without a renderer rewrite.

## Alternatives considered

- **ash/Vulkan-only first** — rejected: platform reach and contributor cost
  outweigh the unused low-level control at this stage.
- **wgpu with mesh shaders/ray tracing in v1** — rejected: experimental status
  and driver risk, no need for the 512-block target.
- **Bevy's renderer** — rejected for the same reasons as ADR-0002.

## References

- [Engine and render stack research](../research/02-engine-and-render-stack.md)
- [Voxel rendering SOTA](../research/03-voxel-rendering-sota.md)
- [Rendering chapter](../spec/03-rendering.md)

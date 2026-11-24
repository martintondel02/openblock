# ADR-0002 — Rust and a purpose-built engine

> Status: accepted · Date: 2026-11-24 · Decisions: DEC-001, DEC-002, DEC-003

## Context

The brief asks for a Minecraft-like voxel sandbox with "insane performance",
high-quality shaders, a very large eventual feature set, self-hostable
multiplayer, and deep moddability. That combination has unusually specific
technical requirements:

- A GPU-driven voxel renderer with custom visibility, streaming and LOD, running
  at a 6.94 ms frame budget (144 FPS).
- A deterministic, headless authoritative simulation that must run identically
  inside the client's integrated server and on a GPU-less VPS.
- A replication layer with prediction and reconciliation on a 20 Hz tick.
- A hot-path-safe content and modding platform, with sandboxing for untrusted
  server mods.
- Memory safety under adversarial network, save and mod input, for a project
  that will be maintained mostly by volunteers.

The realistic language candidates were Rust, C++, C#, Go and Java. Engine
candidates were Bevy, Fyrox, Godot, Unity/Unreal, and a purpose-built engine on
top of a graphics abstraction.

## Decision

**Rust (edition 2024, pinned stable)** and a **purpose-built engine on wgpu 30**.
We own the app loop, scheduler, renderer, world model, simulation and
networking. We buy battle-tested crates for windowing (winit), math (glam),
jobs (rayon), entities (hecs), text (cosmic-text/glyphon), audio (kira),
networking (quinn/rustls), serialization (serde/postcard) and compression
(zstd). We do **not** adopt the Bevy framework.

## Why not the alternatives

| Option | Verdict |
| --- | --- |
| **Bevy framework** | Excellent ecosystem and ECS, but its retained-scene renderer does not provide voxel streaming, section LOD, Hi-Z occlusion or GPU-driven indirect submission — the hard parts remain. Custom render nodes break across Bevy's frequent releases, and its per-material state costs are measured to hurt on many-object scenes. We may still use `bevy_ecs` à la carte if it ever earns its weight, but the renderer and frame loop must be ours. |
| **Fyrox / Godot / Unity / Unreal** | Engines optimise for general scenes, not voxel terrain; all of them make the required renderer, deterministic headless simulation and custom replication either impossible or an exercise in fighting the engine. Licensing and runtime-cost models also conflict with a permissive open-source project. |
| **C++ + Vulkan** | No performance advantage for this workload; dramatically higher memory-safety and build-infrastructure risk; slower iteration for a small team. |
| **Go / Java / C#** | GC pauses and runtime interop complicate the 20 Hz simulation budget and the native GPU path; Rust's ownership model is a better fit for a concurrently simulated world. |
| **Vulkan-only (ash)** | Maximum control, but excludes macOS contributors and players and adds driver-specific burden before we have measured a need. Kept as a measured escape hatch behind the renderer abstraction. |

## Consequences

- We own a significant amount of infrastructure, which is the largest schedule
  risk in the project. Mitigated by milestone vertical slices, buying every
  non-differentiating library, and treating the renderer as the top staffing
  risk.
- The headless server can be genuinely GPU-free (DEC-003), which is a product
  differentiator and simplifies hosting.
- We can meet the performance and determinism requirements without engine
  workarounds, and the architecture is legible to contributors.
- If wgpu proves insufficient on a specific platform or feature, an `ash`
  backend can be added behind the device abstraction without changing the rest
  of the engine — a documented, measured decision rather than a rewrite.

## References

- [Engine and render stack research](../research/02-engine-and-render-stack.md)
- [Weft teardown](../research/01-weft-teardown.md)
- [Technical architecture](../spec/01-technical-architecture.md)

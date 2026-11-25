# OpenBlock documentation

This directory is the single source of truth for what OpenBlock is, how it is
built and why. Code follows this documentation; when code and documentation
disagree, that is a bug in one of them, and the PR must fix it.

## Start here

| Document | What it answers |
| --- | --- |
| [Vision](vision.md) | What game are we making, for whom, and what is deliberately out of scope? |
| [Decision register](decisions.md) | The binding technical decisions (`DEC-001 … DEC-054`), with rationale and reversal cost. |
| [Spec conventions](spec/00-conventions.md) | Terminology, coordinate systems, identifiers and how to write a spec chapter. |
| [Architecture](spec/01-technical-architecture.md) | How the crates, processes and data flows fit together. |
| [Roadmap](plan/00-roadmap.md) | The two-track path from an empty repo to 1.0 Early Access and 2.0, and what each milestone proves. |
| [Milestones](plan/01-milestones.md) | Concrete, testable exit criteria per milestone (`MS-00 … MS-16` for 1.0, then the post-1.0 tracks `MS-17 … MS-24`). |
| [Server operations](admin/README.md) | Build, configure, secure, back up and upgrade a dedicated server (interim 1.0 build). |

## Map of the specification

The spec is numbered so it can be read front to back, but each chapter is
self-contained once you know the terminology from chapter 00.

| # | Chapter | Area |
| --- | --- | --- |
| 00 | [Conventions](spec/00-conventions.md) | Terminology, units, identifiers, writing rules |
| 01 | [Technical architecture](spec/01-technical-architecture.md) | Processes, crates, threading, data flow |
| 02 | [Engine core](spec/02-engine-core.md) | App lifecycle, scheduler, config, diagnostics, platform layer |
| 03 | [Rendering](spec/03-rendering.md) | wgpu, render graph, voxel pipeline, culling, LOD |
| 04 | [Shaders and visuals](spec/04-shaders-and-visuals.md) | Lighting, shadows, water, sky, post-processing, shader packs |
| 05 | [World model](spec/05-world-model.md) | Sections, palettes, chunks, lighting storage, streaming, ticking |
| 06 | [World generation](spec/06-worldgen.md) | Terrain, biomes, caves, features, structures, determinism |
| 07 | [Light and weather](spec/07-light-and-weather.md) | Sky light, block light, propagation, weather, day/night |
| 08 | [Survival gameplay](spec/08-gameplay-survival.md) | Player, health, hunger, combat, progression, dimensions |
| 09 | [Entities and AI](spec/09-entities-ai.md) | Entity model, movement, pathfinding, mob behaviours, spawning |
| 10 | [Physics and collision](spec/10-physics-and-collision.md) | Voxel collision, raycasts, fluids, moving blocks, projectiles |
| 11 | [Items, inventory, crafting](spec/11-inventory-crafting-items.md) | Item stacks, containers, crafting, smelting, enchanting, trading |
| 12 | [Redstone and logic](spec/12-redstone-and-logic.md) | Signal model, components, update ordering, determinism, machines |
| 13 | [UI and UX](spec/13-ui-and-ux.md) | Screens, HUD, inventory interaction, input, accessibility |
| 14 | [Audio](spec/14-audio.md) | Sound events, spatial mixing, music, ambience, voice chat |
| 15 | [Content system](spec/15-content-system.md) | Registries, content packs, tags, recipes, data schemas |
| 16 | [Modding](spec/16-modding.md) | WASM mods, native plugins, host ABI, capabilities, distribution |
| 17 | [Persistence](spec/17-persistence.md) | Save format, region files, migrations, backups, compatibility |
| 18 | [Networking and multiplayer](spec/18-networking-multiplayer.md) | Protocol, transport, replication, prediction, interest management |
| 19 | [Server administration](spec/19-server-administration.md) | Dedicated server, config, ops, permissions, observability |
| 20 | [Security and anti-cheat](spec/20-security-and-anti-cheat.md) | Threat model, validation, sandboxing, moderation, abuse |
| 21 | [Tooling and pipelines](spec/21-tooling-and-pipelines.md) | Asset pipeline, pack compiler, inspectors, world tools |
| 22 | [Platforms and distribution](spec/22-platforms-and-distribution.md) | Build matrix, packaging, updater, storefronts, certification |
| 23 | [Accessibility and localization](spec/23-accessibility-localization.md) | Screen readers, remapping, colorblind, i18n, fonts |
| 24 | [Testing and benchmarks](spec/24-testing-benchmarks.md) | Test pyramid, golden worlds, replay tests, perf gates |
| 25 | [Performance budgets](spec/25-performance-budgets.md) | Frame, tick, memory, IO, network budgets and how they are enforced |

## Plan

| Document | What it answers |
| --- | --- |
| [Roadmap](plan/00-roadmap.md) | Narrative arc, release phases and the two-track effort model |
| [Milestones](plan/01-milestones.md) | Exit criteria and demos for `MS-00 … MS-16` (1.0), `MS-17` (2.0) and the post-1.0 releases |
| [MVP definition](plan/02-mvp.md) | What "playable" means and what is cut |
| [Work breakdown](plan/03-work-breakdown.md) | Epics, tasks, dependencies, estimates |
| [Team and process](plan/04-team-and-process.md) | How work is organised, including AI-assisted development |
| [Risks](plan/05-risks.md) | Risk register with mitigations and triggers |
| [Release and operations](plan/06-release-and-operations.md) | Versioning, release cadence, server distribution, support |
| [Review findings](plan/07-review-findings.md) | The review board that re-baselined the plan, and dispositions |
| [Measurements](plan/08-measurements.md) | Living log of measured performance, storage and determinism results |
| [Benchmarks and soak](plan/09-benchmarks.md) | The 1.0 CPU benchmark/soak tools, commands and known gaps |
| [1.0 scope and delivery plan](plan/1.0-scope.md) | The 1.0.0 checklist, released status and accepted limitations |
| [1.0 acceptance sweep](plan/1.0-acceptance.md) | The end-to-end gameplay-loop acceptance and bug-bash record |
| [1.0.1 audit](plan/1.0.1-audit.md) | Post-release documentation, security, metadata and packaging findings |

## Research

Research documents are dated snapshots, not commitments. They exist so future
decisions can be re-evaluated against the evidence that produced them.

| Document | Topic |
| --- | --- |
| [Weft teardown](research/01-weft-teardown.md) | What the reference Rust voxel engine does, and what we borrow or avoid |
| [Engine and render stack](research/02-engine-and-render-stack.md) | Rust stack options: framework vs own engine, wgpu vs Vulkan, ECS |
| [Voxel rendering SOTA](research/03-voxel-rendering-sota.md) | Meshing, visibility, LOD, lighting, shading techniques with numbers |
| [Networking SOTA](research/04-networking-sota.md) | Transport, replication, prediction, server operations |
| [Minecraft feature inventory](research/05-minecraft-feature-inventory.md) | Tiered parity checklist used for scope control |
| [Modding and content](research/06-modding-and-content-system.md) | Existing modding platforms, WASM vs native, content model |
| [Legal and licensing](research/07-legal-licensing-and-ip.md) | Minecraft IP, MPL-2.0 obligations, our license and policy choices |
| [UI, audio, localization](research/08-ui-audio-localization.md) | Text, widgets, audio engines, i18n and accessibility stacks |
| [Persistence and packaging](research/09-persistence-assets-packaging.md) | Save formats, asset pipeline, builds and updates |
| [Custom content and differentiation](research/10-custom-content-and-differentiation.md) | What OpenBlock adds beyond parity, and the flagship-content risks |

## Architecture decision records

Eight immutable records of decisions with consequences (ADR-0001 … ADR-0008). The
[decision register](decisions.md) holds the current state; ADRs hold the story.

- [ADR-0001 — Record architecture decisions](adr/0001-record-architecture-decisions.md)
- [ADR-0002 — Rust and a purpose-built engine](adr/0002-rust-and-purpose-built-engine.md)
- [ADR-0003 — wgpu-first renderer](adr/0003-wgpu-first-renderer.md)
- [ADR-0004 — 16³ sections with paletted storage](adr/0004-section-size-and-storage.md)
- [ADR-0005 — Server-authoritative QUIC multiplayer](adr/0005-server-authoritative-quic.md)
- [ADR-0006 — Three-tier modding: packs, WASM, native](adr/0006-three-tier-modding.md)
- [ADR-0007 — Data-driven content with namespaced registries](adr/0007-data-driven-content.md)
- [ADR-0008 — MIT OR Apache-2.0, no Minecraft or Weft code](adr/0008-license-and-provenance.md)

## Keeping documentation honest

- Every chapter has a `Status:` line. **normative** chapters are contracts.
- Requirements cite `REQ-*` IDs; tests name the requirement they verify.
- Performance numbers are budgets, checked in CI where possible
  ([Performance budgets](spec/25-performance-budgets.md)).
- Open questions list an owner milestone. A question without an owner is a
  planning bug and will be rejected in review.

# ADR-0006 — Three-tier modding

> Status: accepted · Date: 2026-11-24 · Decision: DEC-020, DEC-019

## Context

Moddability is a headline requirement: content beyond Minecraft, a healthy
ecosystem, and safe operation on servers. Existing approaches each fail in a
specific way:

- **Minecraft loaders (Forge/NeoForge/Fabric)** — total power, no stability, no
  sandbox; bytecode patching breaks every version and produces unattributable
  crashes.
- **Luanti (Lua)** — friendly and stable, but its own security documentation
  admits no process isolation; server mods are fully trusted.
- **Weft (native C ABI)** — 226 host functions, ABI pinned by a source hash,
  loaded in-process; fast, but every ABI change invalidates the ecosystem and a
  bad mod crashes the host.
- **Veloren (WASM components)** — the right idea, but its configuration enables
  limited sandboxing (no fuel/epoch/memory limits), which is not sufficient for
  an untrusted boundary.

We also need the base game to be built on the public API (no private hooks), or
the API will not be good enough for mods.

## Decision

Three tiers, each with an explicit trust model:

1. **Content packs** (untrusted data): registries, tags, recipes, loot,
   worldgen, biomes, sounds, textures, models, UI screens, translations.
   Schema-validated, deterministic layered loading with EDIT/REPLACE/REMOVE,
   pack hashing for multiplayer verification, hot-reload in development.
2. **WASM mods** (untrusted code): server-authoritative behaviour on a versioned
   host ABI, sandboxed with wasmtime, capability-gated (world read/write, entity
   spawn, UI, network, storage), quota-limited (CPU epoch/fuel, memory,
   host-call rate, action budget), panic/trap containment, per-mod cost
   attribution.
3. **Native plugins** (trusted, post-1.0): a tiny versioned C ABI, operator-
   allowlisted, recommended to run in a supervised separate process, never
   auto-downloaded.

The `openblock:base` game ships as content pack #0 plus first-party mods using
the same API; a CI check fails if base behaviour requires a private hook.

## Consequences

- Server operators can run community content without handing over the process.
- The API is versioned by schema and semantics, not by source hash, so mods
  survive engine releases within a compatibility window.
- The WASM boundary imposes call overhead and data-copy discipline: the host ABI
  must be coarse-grained (batch queries/edits, events) rather than per-block
  calls, or performance will suffer.
- Two runtimes (WASM and native) mean two ABI surfaces; native is deliberately
  deferred and small.
- Clients do not run mod code from servers; they receive content packs and
  prediction descriptors, which is both a cheat and a licensing safeguard.
- The content schema and load/freeze pipeline become the real ecosystem
  contract, so they must be designed and documented with the same care as code.

## Alternatives considered

- **Native-only (Weft-style)** — rejected as the default tier: unsandboxed, no
  hot reload, ecosystem-wide rebuilds.
- **Lua/Rhai as the untrusted boundary** — rejected: no memory isolation, no
  standard metering, harder cross-platform determinism.
- **In-process WASM without quotas** — rejected: a mod can hang or exhaust the
  host; quotas are part of the sandbox definition.
- **Mixin-style patching** — rejected outright.

## References

- [Modding research](../research/06-modding-and-content-system.md)
- [Modding chapter](../spec/16-modding.md)
- [Content system chapter](../spec/15-content-system.md)

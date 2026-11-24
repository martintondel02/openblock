# Work breakdown — epics and tasks from MS-00 to MS-16

> Status: normative planning · Last updated: 2026-11-24
> Depends on: [DEC-010](../decisions.md), [DEC-035](../decisions.md), [DEC-036](../decisions.md), [roadmap](00-roadmap.md), [milestones](01-milestones.md), [MVP definition](02-mvp.md)
> Implements: MS-00 – MS-16 (all milestones), including the `0.0.1`, `0.1` and `0.2` checkpoints

## 1. Purpose, scope and sizing

This chapter turns [milestones](01-milestones.md) into assignable work. One **epic**
(`EPIC-NNN`) is a demoable slice of a milestone that one person can own for one or two
weeks; one **task** is one issue, one pull request and one verifiable outcome. Outcomes,
not activities: "the palette round-trips" is a task, "work on storage" is not.

| Size | Meaning | Nominal effort | Rule |
| --- | --- | --- | --- |
| **S** | Under one day | 0.5 dev-day | One sitting, one file or test. |
| **M** | One to three days | 1.5 dev-days | One crate, a few files, tests included. |
| **L** | About one week | 3 dev-days | One subsystem slice; needs a short design note. |
| **XL** | Two weeks or more | 5 dev-days | Must be split before work starts; an epic, not a task. |

Effort uses the roadmap's **dev-weeks** (5 dev-days) for milestones and epics and
dev-days for tasks. Sizes are estimates with the usual ±25% accuracy before design;
each milestone's task list is reconciled against the roadmap budget at grooming, and a
projected overrun above 20% is re-scoped or re-baselined in the roadmap, never hidden.
Engineering effort only: art, audio, level design and QA are budgeted separately
(roadmap §12) and are not counted in the 159 dev-weeks. A task that cannot be finished
in a week is an epic.

## 2. Requirements

| ID | Requirement | Verification |
| --- | --- | --- |
| REQ-WBS-001 | Every milestone MS-00 – MS-16 MUST have at least one epic, and every epic MUST name its milestone, size and dependencies. | Grep the `EPIC-` rows in this file; no unassigned epic. |
| REQ-WBS-002 | A milestone projected to exceed its roadmap effort by more than 20% MUST be re-scoped or the roadmap re-baselined in the same PR. | Monthly reconciliation of task estimates against [milestones §1](01-milestones.md). |
| REQ-WBS-003 | Every task MUST have one size, a dependency note, and a "done when" that a reviewer can check without asking the author. | Issue-form validation; review. |
| REQ-WBS-004 | Every task in MS-00 – MS-03 MUST name its target crate or file path and its test evidence. | Review of §4–§7 against the workspace layout. |
| REQ-WBS-005 | No task may exceed one week of effort; larger work is split into an epic and tasks first. | Size label audit (`size:XL` on an issue is a bug). |
| REQ-WBS-006 | Every issue MUST carry exactly one `area:` label and one `size:` label. | Label-checker CI job. |
| REQ-WBS-007 | A blocked task MUST name the blocking issue number in a comment and carry `blocked`. | Triage audit, weekly. |
| REQ-WBS-008 | Each milestone MUST have a tracking issue with a task list and a demo/release task. | Milestone board inspection. |
| REQ-WBS-009 | The 90-day plan MUST be re-baselined at the end of each month with actuals. | Monthly roadmap review notes. |
| REQ-WBS-010 | Anything on the "not now" list (§12) MUST NOT be scheduled without a DEC. | Diff of [decisions](../decisions.md) against new issues. |

## 3. Milestone to epic map

Effort is from [milestones §1](01-milestones.md) and sums to 159 dev-weeks. Tracks are
the three parallel lanes from [roadmap §14](00-roadmap.md): **A** engine/rendering,
**B** world/simulation, **C** content/networking/tools.

| MS | Name | Release | Effort | Epics | Count | Tracks |
| --- | --- | --- | --- | --- | --- | --- |
| MS-00 | Foundation | — | 1 | EPIC-001–004 | 4 | A, C |
| MS-01 | World core | — | 4 | EPIC-005–010 | 6 | B |
| MS-02 | Engine core | — | 4 | EPIC-011–016 | 6 | A |
| MS-03 | First playable | `0.0.1` | 10 | EPIC-017–025 | 9 | A, B, C |
| MS-04 | GPU rendering | — | 8 | EPIC-026–033 | 8 | A |
| MS-05 | Content + UI | `0.1.0-alpha` | 10 | EPIC-034–042 | 9 | B, C |
| MS-06 | Networking | — | 12 | EPIC-043–052 | 10 | C |
| MS-07 | Dedicated server | `0.2.0-alpha` | 5 | EPIC-053–057 | 5 | C |
| MS-08 | Worldgen | — | 14 | EPIC-058–066 | 9 | B |
| MS-10 | Survival loop | — | 12 | EPIC-067–075 | 9 | B |
| MS-13 | Persistence maturity | `0.3.0-beta` | 5 | EPIC-076–080 | 5 | C |
| MS-09 | Modding | — | 10 | EPIC-081–088 | 8 | C |
| MS-11 | Depth | — | 24 | EPIC-089–099 | 11 | B |
| MS-12 | Visuals + audio | `0.4`, `0.5` | 10 | EPIC-100–106 | 7 | A |
| MS-14 | Parity + polish | `0.6.0-beta` | 14 | EPIC-107–114 | 8 | A, B, C |
| MS-15 | Scale + anti-cheat | `1.0.0-rc` | 8 | EPIC-115–120 | 6 | C |
| MS-16 | 1.0 launch | `1.0.0` | 8 | EPIC-121–127 | 7 | A, C |
| | **Total** | | **159** | | **127** | |

### 3.1 Phase reconciliation

| Phase | Milestones | Dev-weeks | Epics | Share of total |
| --- | --- | --- | --- | --- |
| A | MS-00 – MS-03 | 19 | 25 | 12% |
| B | MS-04, MS-05 | 18 | 17 | 11% |
| C | MS-06, MS-07 | 17 | 15 | 11% |
| D | MS-08, MS-10, MS-13 | 31 | 23 | 19% |
| E | MS-09, MS-11, MS-12 | 44 | 26 | 28% |
| F | MS-14, MS-15 | 22 | 14 | 14% |
| G | MS-16 | 8 | 7 | 5% |
| | **Total** | **159** | **127** | **100%** |

## 4. MS-00 — Foundation (1 dev-week, tasks ≈ 5 dev-days)

The gate milestone: a stranger can clone, build, test, benchmark and release. Nothing
here is throwaway, and everything here blocks two other tracks, so it is never
parallelised away.

| Task | Epic | Size | Deps | Target | Done when |
| --- | --- | --- | --- | --- | --- |
| T-M00-01 Workspace, crate topology, toolchain and lint config | 001 | S | — | `Cargo.toml`, `crates/*/Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `justfile` | `cargo check --workspace --all-targets` and `just ci` pass on a clean clone. |
| T-M00-02 CI matrix with failure-injection proof | 002 | M | 01 | `.github/workflows/ci.yml` | A scratch PR with a clippy warning, a failing test, a forbidden dependency and a broken shader is rejected by CI; docs job lints and link-checks. |
| T-M00-03 Dependency, license and asset policy | 002 | S | 01 | `deny.toml`, `LICENSE-*`, SPDX headers, `xtask asset check` | `cargo deny check` passes; an asset without a `.license` sidecar fails the build; provenance register exists. |
| T-M00-04 `hello window` through wgpu 30 | 003 | S | 01 | `crates/render`, `crates/client` | Window opens and clears on Linux, Windows and macOS; no validation errors in debug. |
| T-M00-05 Headless server process and tools binary | 003 | S | 01 | `crates/server`, `crates/tools` | `openblock-server` prints version and idles at 20 Hz with no GPU or audio dependency; `openblock-tools --help` works. |
| T-M00-06 Benchmark harness skeleton | 004 | M | 02 | `xtask/src/bench.rs`, `benches/` | `cargo xtask bench --scene empty` writes p50/p95/p99 to a report and uploads it as an artifact. |
| T-M00-07 Release dry run and repo policy docs | 004 | S | 02 | `.github/workflows/release.yml`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `MAINTAINERS.md` | A tag produces a signed-release dry run with a changelog check; contributor, security and provenance rules are committed. |

## 5. MS-01 — World core (4 dev-weeks, ≈ 20 dev-days)

| Task | Epic | Size | Deps | Target | Done when |
| --- | --- | --- | --- | --- | --- |
| T-M01-01 Coordinate newtypes and conversions | 005 | M | T-M00-01 | `crates/math` | `WorldPos`, `BlockPos`, `ChunkPos`, `SectionPos`, `LocalPos` round-trip; no raw `f64` triples in public APIs (clippy lint). |
| T-M01-02 AABB and raycast primitives | 005 | M | 01 | `crates/math` | Sweep and slab tests cover axis-aligned and diagonal cases; property test against a brute-force sampler. |
| T-M01-03 Seeded RNG with per-subsystem derivation | 005 | S | 01 | `crates/math` | Same `(seed, dim, subsystem, coords, tick)` yields the same stream across platforms; golden vectors committed. |
| T-M01-04 Paletted section storage: uniform, 4/8/16-bit, overflow | 006 | L | 01 | `crates/world/src/section` | All modes pass get/set property tests at 256 and 65,536 states; uniform section ≤16 B; packed section ≤2 KiB + palette. |
| T-M01-05 Section access, dirty tracking, neighbours | 006 | L | 04 | `crates/world/src/section` | Neighbour iteration is ordered and allocation-free; dirty sets dedupe; fuzz target does not panic on malformed palettes. |
| T-M01-06 Light containers, packed 2 B per block | 007 | M | 04 | `crates/world/src/light` | Nibble read/write for sky and RGB is branch-free and property-tested; memory measured at 2 KiB/section. |
| T-M01-07 Chunk columns and dimension height model | 008 | M | 04 | `crates/world/src/column` | `min_y`/`max_y` validated as multiples of 16 ≤1024; section indexing is O(1) and bounds-checked. |
| T-M01-08 Block-change records and batching | 008 | M | 05 | `crates/world/src/edit` | A batched edit produces one ordered change list; replaying it reproduces the section bytes exactly. |
| T-M01-09 Serializer v0 with deterministic encoding | 009 | L | 05 | `crates/world/src/serialize` | save→load→save is byte-identical; encoding is stable across platforms and hashable. |
| T-M01-10 Round-trip property tests and fixtures | 009 | S | 09 | `crates/world/tests` | 10,000 random edit sequences round-trip; two fixture worlds check in and reload. |
| T-M01-11 Fuzz targets for malformed input | 009 | M | 09 | `fuzz/` | 24 h local fuzz without panic, OOB read or unbounded allocation; corpus committed. |
| T-M01-12 Benchmarks, memory accounting, GPU-free guard | 010 | M | 06, 07 | `benches/`, `xtask` | 512-block-radius terrain ≤350 MB measured; `cargo tree -p ob_world` has no GPU/audio crates. |

## 6. MS-02 — Engine core (4 dev-weeks, ≈ 20 dev-days)

| Task | Epic | Size | Deps | Target | Done when |
| --- | --- | --- | --- | --- | --- |
| T-M02-01 App lifecycle, stages, plugin registration | 011 | M | T-M00-01 | `crates/core/src/app` | `App::run` drives `Input → PreTick → Tick → PostTick → Prepare → Render → Present`; a plugin registers systems with one call. |
| T-M02-02 Fixed tick clock and interpolation alpha | 011 | M | 01 | `crates/core/src/clock` | 20 Hz default and 30 Hz option; alpha is exact under simulated jitter; no float tick comparison (test). |
| T-M02-03 Scheduler with read/write sets and conflict rejection | 012 | L | 01 | `crates/core/src/sched` | Conflicting write sets are rejected at startup with a readable error; ordering is deterministic across runs. |
| T-M02-04 Job system with priority queues | 012 | L | 02 | `crates/core/src/jobs` | Scaling to 8 threads within 80% of ideal on the worldgen microbench; no work-stealing starvation under a priority flood. |
| T-M02-05 Service registry with explicit lifecycles | 011 | S | 01 | `crates/core/src/service` | Services start/stop in dependency order; a panicking service reports and shuts down cleanly. |
| T-M02-06 Config: TOML, CLI, environment precedence | 013 | M | 01 | `crates/core/src/config` | Precedence CLI > env > file > defaults is tested; unknown keys warn, invalid values fail with a path. |
| T-M02-07 Platform abstraction | 013 | M | 01 | `crates/core/src/platform` | Window, input, time, paths and threads behind traits; the server target compiles with the window feature off. |
| T-M02-08 Entity store, stable network IDs, SoA hot data | 014 | L | 01 | `crates/core/src/ecs`, `crates/sim` | IDs are stable across save/load and never reused; 100k entities insert/iterate within budget. |
| T-M02-09 `ob_sim` tick pipeline, world façade, player state | 015 | M | 03, 08 | `crates/sim` | A scripted two-player session runs 1,000 ticks headlessly with no window; events are queued, not dispatched mid-system. |
| T-M02-10 Determinism harness: replay and state hash | 016 | L | 09 | `crates/sim/tests`, `xtask` | Recorded input log replays to the same hash; an intentionally unordered system fails the negative test. |
| T-M02-11 Cross-platform hash job | 016 | M | 10 | CI | Linux and Windows runners produce equal hashes over 10,000 ticks on every PR touching `ob_sim`. |
| T-M02-12 Sim fuzz runner and profiler overlay | 016 | M | 09 | `xtask`, `crates/client` | 100k random inputs show no NaN, tunnelling or out-of-bounds; per-system timing is visible in-game and headless. |

## 7. MS-03 — First playable (10 dev-weeks, ≈ 50 dev-days)

The vertical slice. Everything here is on the critical path for `0.0.1`; the only
parallelism is inside the milestone (render, mesher, physics, save), not across it.

| Task | Epic | Size | Deps | Target | Done when |
| --- | --- | --- | --- | --- | --- |
| T-M03-01 Device, surface, swapchain, depth, validation | 017 | M | MS-02 | `crates/render/src/device` | Window resizes without validation errors; device lost is recovered. |
| T-M03-02 Minimal render graph with pass contracts | 017 | L | 01 | `crates/render/src/graph` | Two passes with declared resources compile, barrier and execute; a missing dependency is a compile-time or startup error. |
| T-M03-03 Opaque voxel pass with texture-array binding | 017 | L | 02 | `crates/render/src/passes` | 16 B/quad vertex pulling renders a test section identically on Vulkan and Metal backends. |
| T-M03-04 Debug UI pass and FPS/frame-time overlay | 017 | M | 02 | `crates/render`, `crates/ui` | Overlay draws p50/p99 frame times and GPU/CPU split without affecting the measured frame time. |
| T-M03-05 Reference per-face mesher | 018 | M | MS-01 | `crates/render/src/mesh` | Matches a brute-force face oracle on 1,000 random sections; used only in tests and as a fallback. |
| T-M03-06 Greedy mesher v1 with AO and UV/tile encoding | 018 | L | 05 | `crates/render/src/mesh` | Corner-edit remesh ≤2 ms on the reference machine; no visible seam; AO matches a reference implementation. |
| T-M03-07 Background meshing workers and budget | 018 | M | 06 | `crates/client/src/mesh_sched` | Meshing runs off the main thread; per-frame budget is enforced and measured; no frame stall >5 ms from meshing. |
| T-M03-08 Section load/unload at 256 blocks with priorities | 019 | L | 06 | `crates/client/src/stream` | Ring loads in under 3 s from a cold start; nearest-first ordering is tested; unload frees GPU and CPU memory. |
| T-M03-09 Per-frame upload budget and hitch measurement | 019 | M | 08 | `crates/render/src/upload` | No frame >33 ms while flying at 30 blocks/s; staging ring never grows without bound. |
| T-M03-10 Deterministic flat/valley generator and spawn search | 020 | M | MS-01 | `crates/worldgen` | Same seed produces identical columns on two platforms; spawn is on solid ground, in bounds, with sky access. |
| T-M03-11 AABB sweep, ground and step detection, sub-stepping | 021 | L | MS-01 | `crates/sim/src/physics` | No tunnelling at 40 blocks/s with 100 ms ticks; step-up works on 0.6-block ledges; fuzz invariants hold. |
| T-M03-12 Walk, jump, sprint, fly tuning and input map | 021 | L | 11 | `crates/sim/src/player`, `crates/client/src/input` | Speeds and jump heights match the documented constants; input is remappable and frame-rate independent. |
| T-M03-13 First-person camera and tick interpolation | 021 | M | 12 | `crates/client/src/camera` | Camera interpolates between ticks without jitter at 60–240 FPS; sensitivity and FOV are configurable. |
| T-M03-14 Raycast, highlight, break/place with batching | 022 | L | 11 | `crates/sim/src/interact` | Edit applies at the next tick and remeshes; reach and cooldown are enforced server-side; undo of a batch is exact. |
| T-M03-15 Sky-light column pass and AO update on edit | 022 | M | MS-01 | `crates/world/src/light` | Edited columns re-light within the frame budget; no dark seams at chunk borders (screenshot check). |
| T-M03-16 In-code registry and programmer-art asset pack | 023 | L | T-M00-03 | `crates/content`, `assets/` | ~40 block types with textures load through the asset pipeline; every asset has a license row. |
| T-M03-17 Integrated server, loopback transport, save/load v0 | 024 | L | MS-01, 14 | `crates/sim/src/host`, `crates/world` | Single-player runs through the server host; save→quit→relaunch restores world, player and hotbar exactly. |
| T-M03-18 Chunk borders, stats overlay, teleport | 025 | S | 08 | `crates/client/src/debug` | Debug tools are behind a flag and excluded from benchmark runs. |
| T-M03-19 Performance pass at 256 blocks | 025 | L | 09, 15 | `crates/render`, `crates/client` | 256 blocks ≥60 FPS average, p95 ≤20 ms on the reference machine (PERF-MVP-001). |
| T-M03-20 Playtest, demo capture and `0.0.1` release | 025 | M | 19 | release notes, `benchmarks/MS-03.md` | Playable bar rows 1–6 ticked; 60-second demo recorded; artifact published with the in/out table. |

## 8. Coarse breakdown, MS-04 to MS-16

Sizes are epic-level: L ≈ 1 week, XL ≈ 2.5 weeks. Each epic's first tasks are listed so a
new contributor can see the shape; the epic issue expands them into `T-*` tasks with the
template in §10.

| Epic | MS | Scope and first tasks | Size | Depends |
| --- | --- | --- | --- | --- |
| EPIC-026 | 04 | Render graph v1: barriers, transient aliasing, per-pass GPU timestamps | L | MS-03 |
| EPIC-027 | 04 | 32³ cluster meshing, per-layer buckets, mesh cache and invalidation | L | 026 |
| EPIC-028 | 04 | Buffer arena, staging ring, size classes, defragmentation, LRU eviction | L | 027 |
| EPIC-029 | 04 | Region/cluster frustum culling, occlusion, multi-draw-indirect submission | XL | 027, 028 |
| EPIC-030 | 04 | CSM shadows (2–3 × 2048²), tone mapping, TAA, sky, basic water | L | 026 |
| EPIC-031 | 04 | Benchmark scenes (`flythrough`, `tnt`, `water`, `cave`, `megabuild`) and perf CI gate | L | 029, 030 |
| EPIC-032 | 04 | Quality presets, render-distance settings, auto-detect, settings plumbing | L | 030 |
| EPIC-033 | 04 | Texture arrays, offline KTX2/BC7 compilation, asset cache | M | MS-03 |
| EPIC-034 | 05 | Registry with typed entries and components, namespaced IDs, tag resolution | L | MS-03 |
| EPIC-035 | 05 | Pack loader (dir/zip), layering, semver deps, schema validation, pack hash | L | 034 |
| EPIC-036 | 05 | `openblock:base` as a pack: blocks, items, recipes, sounds, models, translations, UI | L | 035 |
| EPIC-037 | 05 | Retained UI core: taffy layout, cosmic-text/glyphon, focus, theming, UI scale | XL | MS-03 |
| EPIC-038 | 05 | Screens: menu, world list, create world, settings, pause, HUD | L | 037 |
| EPIC-039 | 05 | Inventory and container screens with server-authoritative transactions (DEC-023) | L | 038, 035 |
| EPIC-040 | 05 | Audio event bus on kira/cpal, placeholder SFX, volume and device settings | M | MS-03 |
| EPIC-041 | 05 | Fluent catalogues, one shipped locale, `en-XA` lint, bitmap + vector fonts | M | 037 |
| EPIC-042 | 05 | Content developer UX: `xtask pack validate`, inspector overlay, template pack | M | 035 |
| EPIC-043 | 06 | Protocol message catalog, bit-packed hot-path codecs, docs generator, fuzz targets | L | MS-05 |
| EPIC-044 | 06 | `Transport` trait, QUIC via quinn + rustls, MTU framing, backpressure, rate limits | L | 043 |
| EPIC-045 | 06 | Handshake: protocol version, content hash, mod manifest, actionable mismatch errors | M | 044 |
| EPIC-046 | 06 | Interest management, snapshot + delta, priority scheduling, per-player bandwidth caps | XL | 045 |
| EPIC-047 | 06 | Progressive join: spawn ring, expanding rings, precompressed cache, loading UI | L | 046 |
| EPIC-048 | 06 | Client prediction, input buffer, reconciliation, rollback of local effects | XL | 045 |
| EPIC-049 | 06 | Entity replication, interpolation, clock sync, lag compensation | L | 046, 048 |
| EPIC-050 | 06 | LAN discovery (mDNS or equivalent), "open to LAN" flow, shareable tickets (DEC-026) | M | 044 |
| EPIC-051 | 06 | In-process network simulator: loss, reorder, duplication, jitter, blackholes | M | 044 |
| EPIC-052 | 06 | Protocol fuzz and soak harness, generated protocol documentation in CI | M | 043, 051 |
| EPIC-053 | 07 | Server binary, TOML config, console commands, permission levels, graceful shutdown | L | 052 |
| EPIC-054 | 07 | World management: create/load/delete, pregeneration, backup/restore with retention | M | 053, MS-03 |
| EPIC-055 | 07 | Observability: JSON logs, Prometheus metrics, `/health`, Grafana examples | M | 053 |
| EPIC-056 | 07 | Packaging: distroless-like multi-arch image, compose, systemd unit, admin docs | L | 053 |
| EPIC-057 | 07 | Bot load harness, capacity report, first-day checklist and troubleshooting docs | M | 055, 056 |
| EPIC-058 | 08 | Noise stack, climate parameters (continentalness, erosion, temperature, humidity), splines | XL | MS-05 |
| EPIC-059 | 08 | Carvers: caves, aquifers, ravines, canyons | L | 058 |
| EPIC-060 | 08 | Ore distribution, geodes, fossils, vegetation and decoration passes | L | 058 |
| EPIC-061 | 08 | Data-driven biomes, climate blending, surface rules, per-biome ambience hooks | L | 058, 036 |
| EPIC-062 | 08 | Jigsaw/template structure API and the first structure set | XL | 060 |
| EPIC-063 | 08 | Nether and End dimensions, portals, dimension-relative seed derivation | L | 058 |
| EPIC-064 | 08 | Staged parallel generation, neighbour-aware scheduling, chunk status levels | XL | 058, 062 |
| EPIC-065 | 08 | Pregeneration CLI and worldgen debug tools (top-down map, biome map, seed browser) | M | 064 |
| EPIC-066 | 08 | Determinism suite, 10,000-seed nightly corpus, ≥250 columns/s performance gate | M | 064 |
| EPIC-067 | 10 | Player state: health, damage types, armor, absorption, hunger, saturation, XP | L | MS-05 |
| EPIC-068 | 10 | Combat: cooldown, knockback, crits, sweep, shields, projectiles, effects | L | 067 |
| EPIC-069 | 10 | Items: stacks, durability, enchantments, tool tiers, armor slots, offhand, use actions | XL | 067 |
| EPIC-070 | 10 | Block behaviour: furnaces, crafting table, stonecutter, chests, hoppers, beds, doors | XL | 069 |
| EPIC-071 | 10 | Recipes, smelting and fuel, brewing, stonecutting, smithing, loot tables | L | 069, 036 |
| EPIC-072 | 10 | World interaction: gravity blocks, fluids and waterlogging, fire, crops, bonemeal | L | 070 |
| EPIC-073 | 10 | Weather, day/night, lightning, sleeping | M | 067 |
| EPIC-074 | 10 | Death, drops, respawn, difficulty modes, game rules, permission checks | M | 068 |
| EPIC-075 | 10 | Onboarding flow and Tier-1 parity tests for damage, hunger and XP | M | 067, 074 |
| EPIC-076 | 13 | Region format v1 frozen: header, checksummed index, compressed sections, sidecars | L | MS-03 |
| EPIC-077 | 13 | Migration framework with fixture saves and downgrade refusal | M | 076 |
| EPIC-078 | 13 | `ids.json` content mapping, unknown-content quarantine and restoration | M | 076, 035 |
| EPIC-079 | 13 | World management UI: metadata, thumbnails, import/export, backup and restore | M | 076, 038 |
| EPIC-080 | 13 | Integrity, corruption detection and repair, atomic-write and fsync tests, save benchmarks | M | 076 |
| EPIC-081 | 09 | Pack SDK crate, JSON schemas, validator, pack template, example packs | L | MS-05 |
| EPIC-082 | 09 | WASM host on wasmtime 51: capabilities, epoch, memory limits, host-call and action quotas | XL | 081 |
| EPIC-083 | 09 | Host ABI v1: queries, queued actions, batched events, KV storage, UI data binding | L | 082 |
| EPIC-084 | 09 | Server mod verification: manifest signing, allowlist, content delivery of packs only | M | 083, MS-06 |
| EPIC-085 | 09 | Per-mod profiler, cost attribution, mod health panel, trap policy | M | 082 |
| EPIC-086 | 09 | Migrate the entire base game onto the public API, with a "no private hooks" CI check | L | 083, MS-11 |
| EPIC-087 | 09 | Hot reload for packs and WASM, developer loop, in-game error overlay | M | 082, 042 |
| EPIC-088 | 09 | API stability policy, deprecation process, conformance suite against previous release | M | 083 |
| EPIC-089 | 11 | Entity framework: data-defined entities, goal/priority AI, A* pathfinding, sensory model | XL | MS-08, MS-10 |
| EPIC-090 | 11 | Mob set: passive, neutral, hostile, villagers, golems, aquatic, nether, end, two bosses | XL | 089 |
| EPIC-091 | 11 | Redstone signal model, dust, torches, repeaters, comparators, pistons, observers | XL | MS-10 |
| EPIC-092 | 11 | Deterministic update ordering, cross-region circuits, loop detection, update budget | L | 091 |
| EPIC-093 | 11 | Farming, breeding, bee and animal products, villager professions and trading | L | 090 |
| EPIC-094 | 11 | Enchanting, anvil, brewing and potions, full loot-chest loop | L | 069, 093 |
| EPIC-095 | 11 | Dimension progression: portals, dragon fight, end cities, elytra, respawn anchors | L | 063, 090 |
| EPIC-096 | 11 | Commands, game rules, statistics, advancements | L | 075 |
| EPIC-097 | 11 | Minecarts, rails, boats, projectiles, falling blocks, TNT | L | 089 |
| EPIC-098 | 11 | Spectator mode and creative-mode completeness | M | 089 |
| EPIC-099 | 11 | Entity replication tuning and mob performance (200 visible entities ≤20 KB/s per player) | M | 089, 049 |
| EPIC-100 | 12 | Post stack: GTAO, SSR water, volumetric fog and clouds, bloom, tone mapping options | XL | MS-04 |
| EPIC-101 | 12 | Particle system, weather visuals, block-break and footstep effects, screen effects | L | 100 |
| EPIC-102 | 12 | Shader pack API v1: declared hooks, capability manifest, validation, samples, docs | XL | 100 |
| EPIC-103 | 12 | Audio: full SFX, positional ambience, occlusion and reverb zones, music with crossfades | L | 040 |
| EPIC-104 | 12 | Controller support, input remapping polish, photo mode | M | 038 |
| EPIC-105 | 12 | Quality presets per hardware tier, render scaling, auto-detect tuning | M | 100, 032 |
| EPIC-106 | 12 | Visual parity review against the checklist; update the "deliberately different" list | M | 100, 102 |
| EPIC-107 | 14 | Generate the parity checklist from registry dumps; build the executable parity corpus | XL | MS-11 |
| EPIC-108 | 14 | Tier-2 closure for remaining blocks, items, mobs and structures | XL | 107 |
| EPIC-109 | 14 | Commands, game rules, advancements and statistics parity | L | 096 |
| EPIC-110 | 14 | Accessibility: narrator, remap, colour-blind palettes, subtitles, reduced motion | L | 038, 103 |
| EPIC-111 | 14 | Localization completeness, RTL rendering, CJK and emoji font fallback, translation workflow | L | 041 |
| EPIC-112 | 14 | UX polish: onboarding, tooltips, recipe book, search, error dialogs, crash recovery | L | 075, 109 |
| EPIC-113 | 14 | Performance presets that target hardware tiers, plus auto-detect and benchmarks | M | 105 |
| EPIC-114 | 14 | Parity delta process for future Minecraft drops; bug triage policy and enforcement | M | 107 |
| EPIC-115 | 15 | Parallel region ticking, interest-set tuning, replication batching, chunk send budgeting | XL | MS-07 |
| EPIC-116 | 15 | Anti-cheat validation suite (movement, reach, break speed, placement, combat, chat) and replay evidence | XL | 115, 099 |
| EPIC-117 | 15 | Moderation: mute, ban, kick with reasons, report command, admin audit log | M | 115 |
| EPIC-118 | 15 | Scale harness: 500 bots, 50-player join storm, 20k-chunk world, 24 h soak and leak detection | M | 115 |
| EPIC-119 | 15 | DDoS and abuse hardening: connection limits, Retry, per-IP quotas, decompression bombs | M | 115 |
| EPIC-120 | 15 | Capacity calculator and published benchmark matrix (vCPU × players × simulation distance) | M | 118 |
| EPIC-121 | 16 | Signed builds for Windows, Linux (AppImage/Flatpak/AUR) and macOS (universal, notarised) | L | MS-14, MS-15 |
| EPIC-122 | 16 | Auto-updater: signed manifest, delta patches, rollback, stable/beta/nightly channels | L | 121 |
| EPIC-123 | 16 | Website, docs site generated from `docs/`, download page, opt-in server list | L | 121 |
| EPIC-124 | 16 | Save migration from every `0.x`, protocol and mod API 1.0 freeze with migration guide | M | 077, 088 |
| EPIC-125 | 16 | Launch content: final art/audio, tutorial world, showcase worlds and mod pack | L | MS-12, MS-14 |
| EPIC-126 | 16 | Legal: trademark clearance sign-off, EULA/terms, privacy policy, security disclosure | M | 121 |
| EPIC-127 | 16 | Launch communications: trailer, devlog, press kit, community channels | M | 123, 125 |

## 9. First 90 days

Baseline: three contributors, ~30 focused hours per week each (≈2.4 dev-weeks per
calendar week including the 20% coordination overhead), tracks A/B/C as in §3. A solo
full-time contributor follows the same order at roughly 2.2× the calendar duration; a
six-person team splits each track in two and shortens phases A and B, not the order.

| Week | A — engine/render | B — world/sim | C — content/tools/QA | Gate |
| --- | --- | --- | --- | --- |
| 1 | MS-00 workspace, CI, window | MS-00 headless server, benches | MS-00 policy docs, asset tool | CI green on 3 OSes; bench artifact |
| 2 | App loop, tick clock | Palettes, section access | Provenance tool, templates | Palette property tests green |
| 3 | Scheduler, jobs | Light containers, columns | Serializer and fuzz targets | save→load→save byte-identical |
| 4 | Entity store, platform layer | Block changes, benches | Fixture saves, memory bench | MS-01 memory budget measured |
| 5 | Sim pipeline, determinism harness | Raycast and AABB primitives | Content registry, texture pipeline | 10k-tick hash equal Linux/Windows |
| 6 | Device, render graph | Physics sweep, controller | Streaming loader, world dir | MS-02 exit criteria met |
| 7 | Greedy mesher, AO | Break/place, batching | Worldgen v0, spawn search | Edit latency ≤2 frames at camera |
| 8 | Upload budget, opaque pass | Camera, interpolation | Integrated server, save/load | First 256-block FPS measurement |
| 9 | Perf pass (with B) | Perf pass (with A) | Debug tools, demo world | Playable bar rows 1–3 green |
| 10 | Hardening, P0/P1 fixes | Hardening, P0/P1 fixes | External playtest (5 testers) | Playable bar complete |
| 11 | `0.0.1` release, demo, retro | Post-release fixes | Release notes, devlog | Artifact published with in/out table |
| 12 | MS-04 graph v1, cluster meshing | MS-05 registry and tags | Pack loader and validator | `0.1` board groomed |
| 13 | MS-04 culling/MDI spike | MS-05 UI core spike | CI bench box, pack template | 512-block perf prototype numbers |

Weeks 10–11 are buffer, not new scope. If week 9 shows the slice is not usable, apply
the [MVP cut lines](02-mvp.md) before extending the schedule. Day 90 deliverable: `0.0.1`
shipped, MS-02 exited, MS-04 and MS-05 both started with their first epic merged.

## 10. Task template

Issue form fields mirror the epics above. The template below is what a maintainer pastes
into a new task issue (a `task.yml` form should replace it, see §15); `bug_report.yml`,
`feature_request.yml` and `spec_proposal.yml` stay for their own cases.

```markdown
### Task: <area>: <imperative outcome>            <!-- e.g. world: pack 4-bit palettes -->
Milestone: MS-01 · Epic: EPIC-006 · Size: M · Type: code | test | docs | data | infra
Depends on: #123, DEC-005 · Blocked by: #456 (or "none")
Owner: @github-handle · Reviewers: @area-owner

**Context.** Why this exists; the requirement or milestone criterion it serves.

**Deliverable.** The concrete artefact (file, API, test, document), in one paragraph.

**Approach (optional).** Known constraints, alternatives rejected, links to the spec
chapter and any spike result. One or two paragraphs at most.

**Done when**
- [ ] <observable behaviour> is verified by <test name / benchmark / recording>.
- [ ] <edge case> is covered by <test>.
- [ ] Docs/changelog updated if user-visible; DEC added or cited if normative.
- [ ] Benchmarks recorded if the task touches a budgeted path.

**Test evidence.** Test path(s), benchmark scene, or demo link expected in the PR.

**Agent assistance.** none | research | code | tests | docs | data (tool and scope).
```

| Field | Source of truth | Rule |
| --- | --- | --- |
| Milestone | [milestones](01-milestones.md) | Exactly one `MS-xx`; the GitHub milestone matches it. |
| Epic | §4–§8 of this chapter | Exactly one `EPIC-NNN`; the epic tracks it in a task list. |
| Size | §1 | One `size:` label; `XL` is rejected — split first (REQ-WBS-005). |
| Depends on | Epic dependency column | Real blockers only; "soft order" is not a dependency. |
| Done when | Requirement or milestone criterion | Checkable by a reviewer, never "works". |
| Test evidence | §4–§7 / spec/24 | A path or scene, not "tested manually". |

## 11. GitHub mapping

- **One GitHub milestone per `MS-xx`**, named `MS-04 GPU rendering`, with the milestone
  body copied from [01-milestones.md](01-milestones.md) plus a link to the tracking issue.
- **One tracking issue per epic** (`type: epic`) with a task list of links to task issues,
  the epic's size, its dependencies and its exit evidence. Closing an epic requires every
  linked task closed and the demo evidence attached.
- **One issue per task** (`type: task`), assigned to one person, in the epic's milestone.
  A task that changes behaviour without an issue is not reviewable (REQ-WBS-003).

| Label | Values | Rules |
| --- | --- | --- |
| `type:` | `bug`, `feature`, `spec`, `task`, `epic` | Exactly one; matches the issue form. |
| `area:` | `core`, `render`, `world`, `worldgen`, `sim`, `content`, `ui`, `audio`, `net`, `server`, `modding`, `tools`, `platform`, `docs`, `ci` | Exactly one (REQ-WBS-006); the area owner reviews. |
| `size:` | `S`, `M`, `L`, `XL` | Exactly one; `XL` on an issue is a defect. |
| `priority:` | `P0`, `P1`, `P2`, `P3` | Set at triage; P0/P1 have response targets in [04](04-team-and-process.md). |
| `good first issue` | — | ≤1 day, file pointers in the issue, a named mentor, no design decisions. |
| `blocked` | — | Requires a comment naming the blocking issue or decision (REQ-WBS-007). |
| `needs-design` | — | Blocks coding; names the spec chapter that must change first. |
| `needs-triage` | — | Default for new issues; cleared within 3 working days. |
| `security-embargo` | — | Private security work; exempt from the DEC comment window until disclosure. |
| `parity` | — | Tracks a Tier-2 checklist item; links the generated checklist ID. |

Automation: a label-checker job fails PRs and issues without one `area:` and one
`size:`; a PR-size comment warns above 400 changed lines (see [04](04-team-and-process.md));
a stale bot closes issues untouched for 90 days as `P3` after a one-month warning, and
`needs-design` issues after 60 days. A DCO check is required on every commit.

| Budget | Target | Enforcement |
| --- | --- | --- |
| PERF-WBS-001 | Pull-request CI p50 ≤15 minutes, p95 ≤25 minutes. | Weekly CI timing report; a slower job is optimised or split. |
| PERF-WBS-002 | Nightly suite (determinism, benchmarks, fuzz, parity corpus) ≤60 minutes. | Nightly job timing; exceeding it moves work to weekly. |

Useful views: `area:render size:S no:assignee` (quick wins), `label:blocked` (unblock
list), `milestone:"MS-03" is:open` (burn-down), `label:good-first-issue` (onboarding).

## 12. Not now

| Item | Why not now | Revisit when |
| --- | --- | --- |
| LOD and far terrain | Not needed at 512 blocks; visual-only by decision (DEC-018). | `1.4` (MS-20) or a measured frame-time need. |
| Client-side WASM gameplay code | Breaks server authority and anti-cheat; OPEN-006 open. | MS-09 review, and only with a prediction descriptor. |
| Native plugins | Trust model unresolved (OPEN-007); operator-only, post-1.0. | MS-21, after the WASM tier is stable. |
| Web/WebGPU client, Android | Platform expansion after 1.0 (DEC-034, OPEN-008). | MS-22 / MS-19. |
| Minecraft import or protocol compatibility | Non-goal in v1; legal and product risk. | Never in `1.x`; only as an isolated community bridge. |
| Marketplace, paid-mod cut, telemetry | Non-goals (vision §7). | Never. |
| Multi-server proxy, lobby, transfer | Adds a distributed-systems problem before the single server is boring. | `1.5` (MS-21). |
| In-engine visual editor, scripting language | Content packs and WASM cover authoring; a second language splits tooling. | Post-1.0, only with user research. |
| Controller UI and accessibility deep pass | Needs final screens first. | MS-12 (controller) and MS-14 (accessibility). |
| Voice chat | Needs moderation tooling and a relay budget (DEC-031). | Post-1.0. |
| Custom physics for moving structures | Not required for parity; large risk. | After MS-11, with a measured use case. |
| Hosted official servers, accounts | Contradicts "self-hosted is the reference"; DEC-026 keeps accounts optional. | Never as a requirement. |

## 13. Failure modes of this plan

| Failure | Signal | Response |
| --- | --- | --- |
| Epic theatre: epics with no demoable output. | An epic closes without a recording, benchmark or visible change. | Re-open the epic; every epic ends in something runnable (milestone DoD). |
| Hidden integration tax. | Three tasks are "done" but the milestone criterion needs all three plus a fourth. | Dependency column must name integration tasks explicitly; review the critical path weekly. |
| Parallel work without interfaces. | Two contributors edit the same crate in one week. | Interface-first tasks; crate ownership per track; land the trait before the implementations. |
| Estimate drift inside XL epics. | An XL epic has no merged task after two weeks. | Split at the first missing merge; XL epics get a week-by-week task list up front. |
| Task inflation from agents. | 100 tiny issues, few merged PRs. | One issue per outcome (REQ-WBS-003); maintainers merge issues rather than create more. |
| The vertical slice grows forever. | MS-03 passes 12 dev-weeks of recorded effort. | Apply the [MVP cut lines](02-mvp.md) and ship `0.0.1` smaller. |
| Milestone closes on paper only. | Exit criteria are ticked without links. | REQ-WBS-003: no link, no tick; the definition of done in [01](01-milestones.md) is enforced by review. |
| Stale backlog hides the real work. | Open issues grow faster than closed for two months. | Monthly grooming; close `P3` after 90 days; cap the milestone to work that fits the phase. |

## 14. Verifying the plan itself

At each milestone exit, the milestone owner checks: every epic closed with evidence, the
effort reconciliation against the roadmap budget (REQ-WBS-002), every task labelled, and
the next milestone's first three epics broken into tasks. At each month end the lead re-baselines the 90-day
plan against actuals and records the delta in the roadmap (REQ-WBS-009). A milestone whose
epics do not reconcile is not a planning problem to hide; it is a signal that the epic
list is wrong and must be split or re-scoped before work continues.

## 15. Open questions

1. **Register item needed: planning currency and granularity.** Dev-days for tasks,
   dev-weeks for epics and milestones, one issue per outcome, size labels mandatory.
   Without it, estimates drift between trackers and the ±10% rule is unenforceable.
   Owner: MS-00.
2. **Register item needed: content production is budgeted separately.** Art, audio, level
   design and QA are not part of the 159 engineering dev-weeks; formalises roadmap §12
   so planning cannot silently absorb them. Owner: MS-16.
3. **Epic representation.** GitHub issues (`type: epic`) versus a project-board "Epic"
   field. Issues are linkable and searchable; fields are cheaper. Proposal: issues, with
   a board view. Owner: MS-00.
4. **Task form.** A dedicated `.github/ISSUE_TEMPLATE/task.yml` should encode §10 and
   the label rules; today a maintainer pastes the template. Owner: MS-00.
5. **World-management UI ownership.** EPIC-079 is listed under MS-13 but the screens are
   MS-05 work; the effort may need splitting between the two milestones. Owner: MS-13.
6. **90-day baseline.** The plan assumes three contributors at ~30 h/week; if the team is
   two or six, the week table is rescaled, not reordered. Owner: MS-00 (lead).

## 16. References

- [Roadmap](00-roadmap.md) (effort model, phases, parallel tracks) and
  [milestones](01-milestones.md) (exit criteria, definition of done).
- [MVP definition](02-mvp.md) for the checkpoint acceptance criteria used in MS-03,
  MS-05 and MS-07 tasks.
- [Team and process](04-team-and-process.md) for review rules, CI gates, labels,
  branch strategy and the AI-assistance policy referenced by the task template.
- [Decision register](../decisions.md): DEC-005, DEC-010, DEC-020, DEC-023, DEC-033,
  DEC-035, DEC-036.
- [Contributing](../../CONTRIBUTING.md), [maintainers](../../MAINTAINERS.md), and the
  existing [issue templates](../../.github/ISSUE_TEMPLATE/) and
  [PR template](../../.github/pull_request_template.md).
- [Testing and benchmarks](../spec/24-testing-benchmarks.md),
  [tooling and pipelines](../spec/21-tooling-and-pipelines.md),
  [performance budgets](../spec/25-performance-budgets.md).
- [Modding research](../research/06-modding-and-content-system.md) for the MS-09 epic
  shape (SDK, WASM quotas, verification, hot reload).

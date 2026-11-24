# Milestones — exit criteria and definition of done

> Status: normative planning · Last updated: 2026-11-24
> Depends on: [Roadmap](00-roadmap.md), [Decision register](../decisions.md)
>
> Each milestone below is a contract: it is "done" only when every exit
> criterion is demonstrably true on CI and on the reference machine. The
> **definition of done** that applies to every milestone is listed in §2.

## 1. Milestone map

| ID | Name | Phase | Effort (dev-weeks) | Depends on | Release |
| --- | --- | --- | --- | --- | --- |
| MS-00 | Foundation | A | 1 | — | — |
| MS-01 | World core | A | 4 | MS-00 | — |
| MS-02 | Engine core | A | 4 | MS-00 | — |
| MS-03 | First playable | A | 10 | MS-01, MS-02 | `0.0.1` |
| MS-04 | GPU rendering | B | 8 | MS-03 | — |
| MS-05 | Content + UI | B | 10 | MS-03 | `0.1.0-alpha` |
| MS-06 | Networking | C | 12 | MS-04, MS-05 | — |
| MS-07 | Dedicated server | C | 5 | MS-06 | `0.2.0-alpha` |
| MS-08 | Worldgen | D | 14 | MS-05 | — |
| MS-10 | Survival loop | D | 12 | MS-05 | — |
| MS-13 | Persistence maturity | D | 5 | MS-03 | `0.3.0-beta` |
| MS-09 | Modding | E | 10 | MS-05, MS-07 | — |
| MS-11 | Depth (mobs, redstone, …) | E | 24 | MS-08, MS-10 | — |
| MS-12 | Visuals + audio | E | 10 | MS-04 | `0.4.0-beta`, `0.5.0-beta` |
| MS-14 | Parity + polish | F | 14 | MS-09, MS-11, MS-12 | `0.6.0-beta` |
| MS-15 | Scale + anti-cheat | F | 8 | MS-07 | `1.0.0-rc` |
| MS-16 | 1.0 launch | G | 8 | MS-14, MS-15 | `1.0.0` |

## 2. Definition of done (applies to every milestone)

A milestone is complete only when **all** of these hold:

- [ ] Every exit criterion in the milestone is verified by an automated test or
      a recorded manual demonstration attached to the milestone issue.
- [ ] No `#[ignore]`d tests, no `todo!()` on a path reachable from a shipped
      build, no debug-only behaviour differences.
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `cargo deny`,
      rustdoc `-D warnings` and shader validation pass on Linux, Windows and
      macOS.
- [ ] Performance budgets for touched areas are met, with benchmark results
      recorded in `benchmarks/<milestone>.md` or a CI artifact.
- [ ] The dedicated server still builds without `wgpu`, `winit` or `cpal`
      (DEC-003) and starts/ticks headlessly.
- [ ] Documentation is updated in the same PR: the relevant spec chapter(s),
      the decision register if a decision changed, and the changelog.
- [ ] Saves written before the milestone load after it (or a migration exists).
- [ ] No new `unsafe` outside audited modules (see [spec/02](../spec/02-engine-core.md)
      for the audited list); any new `unsafe` has a safety comment and a test.
- [ ] The milestone demo is recorded and linked from the release notes.

## 3. Phase A — Foundation and first playable

### MS-00 — Foundation

**Goal:** the project can be built, tested, benchmarked and released by a
stranger following only the repository docs.

**Deliverables**

- Workspace with the crate topology from DEC-003, all crates present, CI green.
- CI pipeline: matrix (Linux/Windows/macOS), fmt, clippy, `nextest`, docs,
  `cargo-deny`, shader validation (`naga`), `cargo tree` guard forbidding GPU
  crates in `ob_server`.
- Benchmark harness: criterion benches for hot paths + a scripted frame-time
  benchmark scene runner (headless + windowed).
- `openblock` opens a window showing a wgpu-cleared surface; `openblock-server`
  starts, prints its version and idles; `openblock-tools` prints help.
- Conventions, decision register, feature inventory, roadmap and milestones
  committed; `CONTRIBUTING`, `SECURITY`, `CODE_OF_CONDUCT`, license texts,
  provenance policy.
- Asset policy tooling: a script that fails if an asset lacks a license/source
  record.

**Exit criteria**

- [ ] A fresh clone builds and runs the window and the headless server on all
      three desktop OSes following only `CONTRIBUTING.md`.
- [ ] CI catches a deliberate `clippy` warning, a failing test, a forbidden
      dependency and a broken shader in a test PR.
- [ ] `cargo xtask bench --scene empty` produces a report with frame-time
      percentiles and stores it as an artifact.
- [ ] `cargo deny check` passes with the dependency policy from DEC-009.

### MS-01 — World core

**Goal:** the voxel world data model is correct, memory-efficient and
serializable, independent of rendering.

**Deliverables**

- `ob_math`: coordinate newtypes (`WorldPos`, `BlockPos`, `ChunkPos`,
  `SectionPos`, `LocalPos`), AABBs, fixed-timestep helpers, seeded
  splitmix/xoshiro RNG with per-subsystem derivation.
- `ob_world`: paletted section storage (uniform/4/8/16-bit with overflow
  palette), light containers (packed 2 B/block), chunk columns, dimension height
  model, block get/set, dirty tracking, block-change records, section
  neighbors, raycast and AABB iteration primitives.
- `ob_world` serialization: a preliminary (pre-format-freeze) serializer with
  round-trip property tests and fuzz targets for malformed input.
- Benchmarks: section get/set, palette conversion, chunk iteration, memory per
  section across content distributions, save/load round-trip.

**Exit criteria**

- [ ] A 512-block-radius world of terrain-like data fits within the memory budget
      of [spec/25](../spec/25-performance-budgets.md) (≤ 350 MB for block + light
      data with the stated content distribution) — measured, not estimated.
- [ ] Property tests: for random edit sequences, get-after-set returns what was
      written, and save→load→save is byte-identical (deterministic encoding).
- [ ] Palette overflow behaves correctly at 256 and 65,536 distinct states, and
      a malformed palette never panics or reads out of bounds (fuzz target).
- [ ] Section memory is ≤ 2 KiB + 2 KiB (data + light) for typical terrain, and
      uniform sections are ≤ 16 bytes in memory.
- [ ] `ob_world` has no dependency on `ob_render`, `wgpu`, `winit` or `cpal`.

### MS-02 — Engine core

**Goal:** a deterministic simulation loop, a scheduler that scales with cores,
and a platform layer that both client and server share.

**Deliverables**

- `ob_core`: app lifecycle (`App::run`, stages, plugins), fixed-tick clock with
  interpolation alpha, job system (rayon pools + priority queues), service
  registry with explicit lifecycles, configuration (TOML + CLI + env), tracing
  and profiling integration, crash reporting hooks, platform abstraction
  (window, input, time, paths, threads).
- `hecs` entity store wrapper with stable network IDs, plus custom SoA storage
  for hot simulation data (positions, velocities, collision states).
- `ob_sim` skeleton: tick pipeline, world access façade, player state machine,
  command/event queues, deterministic system ordering.
- Determinism test harness: replay a recorded input log and compare state hashes;
  cross-platform hash comparison job in CI; a "sim fuzz" runner that executes
  thousands of random inputs and checks invariants (no NaN, no tunnelling, no
  out-of-bounds).
- Profiler UX: per-stage timing, per-system attribution, frame budget overlay.

**Exit criteria**

- [ ] The headless binary runs 10,000 ticks of a scripted world and produces the
      same state hash on Linux, Windows and macOS CI runners (DEC-007).
- [ ] A deliberately non-deterministic system (unordered hash iteration) is
      caught by the harness in a negative test.
- [ ] Tick time with 2,000 loaded chunks and 50 simulated players is ≤ 35 ms
      p99 on the reference server hardware in the headless harness.
- [ ] Adding a system to a stage requires only one registration call; the
      scheduler rejects conflicting write sets at startup.
- [ ] The client and server binaries share the same `ob_sim` code path; a test
      asserts a two-player scripted session produces identical hashes on both.

### MS-03 — First playable (`0.0.1`)

**Goal:** the vertical slice. A real world, a real player, real meshing, real
physics, real saves. This is the milestone that de-risks the project.

**Deliverables**

- `ob_render` v0: wgpu 30 device/queue, surface, swapchain, depth buffer, a
  minimal render graph, chunk geometry upload, opaque pass, a debug UI pass.
- `ob_client`: window, first-person camera, input map, player controller,
  crosshair HUD, FPS/frame-time overlay.
- Mesher: correct per-face meshing with greedy meshing v1, ambient occlusion,
  texture array sampling from a programmer-art pack; background worker pipeline.
- Streaming: load/unload sections around the player within a 256-block radius,
  with priorities, budgeted per frame.
- `ob_worldgen` v0: deterministic flat/valley test generator behind a feature
  flag (real worldgen is MS-08).
- `ob_sim` v0: AABB voxel collision, walk/jump/sprint/fly (creative), raycast
  break/place, block-change batching, integrated server + loopback transport.
- `ob_content` v0: in-code block registry (data packs arrive in MS-05),
  programmer-art block palette, textures loaded from the asset pipeline.
- Save/load of the world to a simple, clearly-marked pre-1.0 format.
- Debug tooling: wireframe chunk borders, chunk stats overlay, teleport command.

**Exit criteria**

- [ ] Walk/jump/fall/collide in a generated world for 10 minutes with no crash,
      no tunnelling through blocks at normal speed, and no visible mesh seams.
- [ ] Breaking and placing blocks updates geometry and lighting within 2 frames
      at the camera and within 100 ms at 8 chunks away.
- [ ] 256-block view distance holds ≥ 60 FPS average on the reference machine
      with the pre-GPU-driven pipeline (this is the baseline MS-04 must quadruple).
- [ ] Save → quit → relaunch restores the world, player position, inventory and
      every block exactly (verified by a hash and a screenshot comparison).
- [ ] A recorded replay of 60 seconds of input reproduces the same final state on
      a second machine.
- [ ] `openblock --world <path>` and the integrated server are the same code path
      as the dedicated server's simulation (asserted by test).

**Demo:** the 60-second "first blocks" clip. This is the milestone to show
publicly and to recruit with.

## 4. Phase B — Engine maturity and content platform

### MS-04 — GPU rendering

**Goal:** 512 blocks of view at 120+ FPS with a renderer that has headroom, and
a benchmark suite that makes regressions visible.

**Deliverables**

- Render graph v1 with pass/resource contracts, transient aliasing, barriers,
  and per-pass GPU timestamp queries.
- Section mesh caching, 32³ cluster meshing (provisional, DEC-004/OPEN-001),
  per-layer opaque/translucent buckets, 16 B/quad vertex pulling, texture arrays.
- Streaming/upload: staging ring buffers, budgeted per-frame uploads, buffer
  arena with size classes, defragmentation, eviction policy (LRU per distance).
- Visibility: region+cluster frustum culling, occlusion (section-level at first,
  Hi-Z compute pass as target), multi-draw-indirect submission with per-layer
  argument buffers.
- Shading: CSM shadows (2–3 × 2048²), tone mapping, TAA, fog, sky, basic water.
- Benchmarks: scripted scenes (`flythrough`, `tnt`, `water`, `cave`, `megabuild`)
  with p50/p95/p99 frame times, GPU/CPU breakdown, CI perf gate with tolerance.
- Quality presets (potato/low/medium/high/ultra) and render-distance settings.

**Exit criteria**

- [ ] 512-block render distance at 1080p on the reference machine: ≥ 120 FPS
      average, p99 frame ≤ 12 ms, CPU main-thread ≤ 2.0 ms in the benchmark.
- [ ] The 144 FPS performance mode is hit at 512 blocks with the documented
      preset (OPEN-003), or the preset/threshold is re-baselined with data.
- [ ] Fly-through benchmark shows no frame > 33 ms (hitch) during streaming at
      30 blocks/s movement.
- [ ] GPU memory at 512 blocks ≤ 768 MB; client RSS ≤ 4 GiB (DEC-035).
- [ ] Zero GPU validation errors in debug builds across the whole benchmark suite
      (`wgpu` validation enabled).
- [ ] Benchmark regression gate is wired into CI and fails on a > 10% regression
      in frame-time p95 for the fly-through scene.

### MS-05 — Content + UI (`0.1.0-alpha`)

**Goal:** content is data, the UI is a real product surface, and inventory
transactions are trustworthy over the network.

**Deliverables**

- `ob_content` v1: registry with typed entries and components, namespaced IDs,
  tags, pack loader (directory + zip), schema validation with useful errors,
  deterministic multi-pack loading with EDIT/REPLACE/REMOVE and dependency
  ordering, pack hash computation, hot reload in development (DEC-019).
- `openblock:base` pack: all first-party blocks, items, recipes, sounds, models,
  translations, UI screens as data — no code path that only vanilla can use.
- `ob_ui` v1: retained widget tree, RON screen definitions, taffy layout,
  cosmic-text/glyphon text, focus/navigation, theming, UI scale, input mapping;
  main menu, world list, create-world, settings, HUD, pause, inventory, container
  screens; drag/split/shift-click cursor semantics (DEC-023).
- `ob_audio` v0: event bus, kira/cpal backend, placeholder SFX, volume settings.
- Localization pipeline: Fluent catalogs, one additional shipped locale, string
  lint that fails on hard-coded UI text.
- Content developer UX: `cargo xtask pack validate`, a content inspector overlay,
  and a template pack that adds a block, item and recipe.

**Exit criteria**

- [ ] The base game can be replaced: a pack that overrides a block texture,
      recipe and sound changes the game without touching `ob_content` code.
- [ ] A malformed pack produces a precise, actionable error and does not crash;
      packs are fuzzed for panics.
- [ ] Inventory transactions survive the in-process network simulator at 30%
      loss / 150 ms RTT with zero duplications or item loss, under a randomized
      click/fuzz driver.
- [ ] UI at 1080p and 4K with UI scale 50–200% is pixel-crisp (no fractional
      glyph snapping), and keyboard-only + gamepad-only navigation reach every
      screen.
- [ ] All user-visible strings come from catalogs (lint-enforced); pseudo-locale
      (`en-XA`) shows no truncation beyond the documented widget policies.
- [ ] Content hot reload rebuilds the registries in < 2 s for the base pack.

## 5. Phase C — Multiplayer and servers

### MS-06 — Networking

**Goal:** responsive multiplayer with an authoritative host, a documented
protocol, and a test harness that can reproduce bad networks.

**Deliverables**

- `ob_protocol`: versioned message catalog, bit-packed hot-path codecs (block
  edit, entity delta, input frame, tick packet), postcard-based control messages,
  content-hash + protocol handshake, protocol documentation generator, fuzz
  targets.
- `ob_net`: `Transport` trait, QUIC implementation (quinn + rustls, datagrams +
  streams), TLS identity pinning and tickets (DEC-026), connection limits, rate
  limits, MTU-safe framing, backpressure.
- Replication: interest sets per player (chunks in simulation distance, relevant
  entities, events), priority scheduling, snapshot + delta, 16³-granular chunk
  deltas, progressive join with precompressed caches, per-player bandwidth caps
  (DEC-025).
- Prediction/reconciliation: local input buffer, authoritative correction,
  rollback of local-only effects, entity interpolation, clock sync, lag
  compensation for combat.
- LAN discovery (mDNS or equivalent) and an "open to LAN" flow; in-process
  loopback transport.
- Network simulator: loss, reordering, duplication, latency jitter, bandwidth
  caps, blackholes; scripted scenarios in CI.

**Exit criteria**

- [ ] Two players join a dedicated server over the internet (automated nightly
      or a recorded manual test) and build together; edits propagate within one
      tick; no ghost blocks or duplicated items under loss.
- [ ] Movement at 80 ms RTT + 2% loss: local player input-to-photon ≤ 1 frame of
      additional latency vs single-player; no rubber-banding during normal
      movement; corrections logged and bounded.
- [ ] A 512-block-radius join streams progressively and completes in ≤ 60 s on a
      10 Mbit/s link with the loading UI showing progress, without starving the
      tick (p99 tick stays ≤ 35 ms during join).
- [ ] Protocol fuzz: 10 million mutated packets over 24 h produce no panic, no
      OOM, no hang; malformed input closes the connection with a reason.
- [ ] Content-hash mismatch is rejected with a message naming the mismatched
      packs (tested).
- [ ] The documented protocol spec is generated and builds in CI.

### MS-07 — Dedicated server (`0.2.0-alpha`)

**Goal:** a server an ordinary person can run for a month.

**Deliverables**

- `openblock-server`: TOML config, CLI flags, world directory layout, console
  commands (`stop`, `list`, `say`, `op`, `kick`, `ban`, `whitelist`, `save`,
  `tps`, `metrics`), permission levels, RCON-compatible remote console, graceful
  shutdown with save, crash-safe restart loop.
- World management: create/load/delete worlds, pregeneration command with
  progress, backup/restore with retention policy and `--verify` mode.
- Observability: structured JSON logs, Prometheus endpoint (tick time, entity
  counts, chunk counts, player bandwidth, GC-free memory stats), `/health`.
- Packaging: multi-arch container image (distroless-like, no GPU/audio libs),
  compose file, systemd unit, Helm-free simple deployment docs.
- Load harness: bot clients that join, move, chat and edit; scripts for 10/25/50
  players; capacity report generation.
- Administration docs: first-day checklist, firewall/port notes, backup
  strategy, upgrade procedure, troubleshooting.

**Exit criteria**

- [ ] From a fresh VPS: install container runtime, `docker compose up -d`,
      server prints a join ticket; a client joins in < 5 minutes of wall-clock
      including world creation.
- [ ] 4 vCPU / 8 GiB reference VPS: 16 bot players for 1 hour at 20 TPS, p99 tick
      ≤ 35 ms, RSS ≤ 2 GiB + 20 MiB/player.
- [ ] `kill -9` during a save never corrupts the world; the backup/restore
      procedure is automated and tested in CI with a simulated power loss.
- [ ] Metrics endpoint exposes every metric in [spec/19](../spec/19-server-administration.md)
      and docs include example Grafana panels.
- [ ] The container image contains no `wgpu`, `winit`, `cpal`, X11, Wayland,
      PulseAudio or ALSA dependencies (image inspection test).

## 6. Phase D — World and survival

### MS-08 — Worldgen

**Goal:** a world worth exploring: biomes, caves, ores, structures and
dimensions, generated fast and deterministically.

**Deliverables**

- Noise stack: domain-warped 2D/3D noise, continentalness/erosion/temperature/
  humidity climate parameters, river networks, terrain shaping with spline
  curves; integer-only or pinned-float noise for cross-platform determinism.
- Carvers: caves (cheese/spaghetti/noodle-style), aquifers, ravines, canyons.
- Ore distribution with per-biome rules, geodes, fossils, vegetation features,
  tree/plant decoration passes.
- Biome system: data-driven biome definitions, climate blending, surface rules,
  per-biome sounds/ambience/fog/colours.
- Structures: jigsaw-style structure API, a first set (villages, ruins, temples,
  strongholds, ocean monuments, woodland mansions, ancient cities, trial
  chambers equivalents), structure placement and spacing rules.
- Nether and End dimensions with portals; dimension-relative seed derivation.
- Parallel generation pipeline: staged (biomes → terrain → surface → carvers →
  features → structures) with neighbor-aware scheduling, chunk status levels,
  and a budgeted consumer on the client.
- Pregeneration tooling and worldgen debugging tools (top-down maps, biome
  visualiser, seed browser, structure viewer).

**Exit criteria**

- [ ] ≥ 250 columns/s (terrain + features + structures) on 8 threads of the
      reference machine; ≤ 50 MB peak transient memory per worker.
- [ ] A 10,000-seed nightly corpus produces no floating structures, no
      interrupted rivers through structures, no spawns in water/lava, no
      unreachable ore pockets (statistical thresholds documented).
- [ ] Generation is deterministic across Linux/Windows/macOS and between client
      and server (hash comparison in CI).
- [ ] Biome transition quality meets a visual review checklist (no hard borders
      except water/cliff cases).
- [ ] Structure placement is data-driven: a content pack can add a new structure
      with no engine change (tested with the template pack).
- [ ] A generated 2,000×2,000 world pregenerates in ≤ 10 minutes on 8 threads.

### MS-10 — Survival loop

**Goal:** the game has a progression loop that a new player can follow for
10+ hours.

**Deliverables**

- Player state: health, damage types and sources, armor, absorption, hunger/
  saturation, exhaustion, regeneration, XP levels and orbs, death drops and
  respawn, inventory persistence.
- Combat: attack cooldown, knockback, critical hits, sweep, shields, projectiles,
  mob damage, potion effects, enchantment application points.
- Items: item registry, stack data components (durability, enchantments,
  custom data), tool tiers and mining rules, armor slots, offhand, item use
  actions (eat, place, mine, interact, throw), item entities and despawn.
- Blocks with behaviour: furnaces/blast furnaces/smokers, crafting tables,
  stonecutter, anvil, enchanting table, brewing stand, chests/barrels/shulkers,
  hoppers/droppers/dispensers, beds, doors, buttons/levers, note blocks, jukebox.
- Crafting and processing: shaped/shapeless/recipe-book recipes, smelting and
  fuel values, brewing recipes, stonecutting, smithing, enchanting tables and
  anvils, loot tables with conditions and functions, trade tables (stub for
  MS-11).
- World interaction: gravity blocks (sand/gravel/concrete powder), fluids
  flowing and waterlogging, fire spread and extinguishing, snow/ice melt,
  crop growth and bonemeal, animal breeding (entity side in MS-11).
- Weather and time: rain/snow/thunder, lightning, day/night cycle, sleeping.
- Difficulty modes, game rules, and server-side permission checks for creative
  actions.
- Tutorial/onboarding flow using the parity-agnostic "shelter before night"
  scenario.

**Exit criteria**

- [ ] A scripted new-player playthrough (punch wood → craft → shelter → mine →
      smelt → iron tools → survive a night → farm) completes with no admin help.
- [ ] All damage types, hunger, XP and death/respawn behaviour match the parity
      checklist at Tier 1, verified by unit tests and manual corpus.
- [ ] Item duplication fuzz: 100k randomized container/hopper/crafting sequences
      in the network simulator produce zero duplications and zero item loss.
- [ ] Redstone-adjacent blocks (pistons, observers, hoppers) are *not* required
      here; they belong to MS-11, but the block-state model must already support
      them (proven by a spike test).
- [ ] A 10-hour play session's save grows ≤ 5 MB beyond world data (item/entity
      churn is bounded).

### MS-13 — Persistence maturity (`0.3.0-beta`, with MS-08/MS-10)

**Goal:** saves are a promise, not a hope.

**Deliverables**

- Region file format v1 frozen: header, chunk index with checksums, per-chunk
  compressed sections, block entities, entities, scheduled ticks, generated-only
  flags (DEC-033).
- Migration framework: version detection, ordered migrations with fixture saves
  in the repo, downgrade refusal with a clear error, dry-run mode.
- Content mapping: `ids.json` resource-ID ↔ numeric mapping preserved across
  content changes; unknown content preserved as opaque placeholders and
  restorable when the pack returns.
- World management: world list metadata, thumbnails, last-played, size, backup
  and restore UI, import/export.
- Integrity: per-chunk checksums, region-level fsync policy, atomic double-buffer
  writes, corruption detection and repair tool.
- Benchmarks: save/load throughput, save pause time, world size at 512-block
  radius, backup time.

**Exit criteria**

- [ ] Power-loss simulation (kill -9 during writes, fault injection) over 1,000
      iterations never produces an unreadable world; at most the last save is
      lost.
- [ ] Fixture saves from every released version (starting at `0.1`) migrate
      forward automatically and are byte-stable on re-save.
- [ ] Unknown modded content round-trips: remove a pack, load, re-add, reload —
      data is intact.
- [ ] Save of a 512-block-radius world ≤ 1.5 GiB compressed; incremental save
      pause ≤ 30 ms p99.
- [ ] No `fsync`-per-block-edit: write batching verified by syscall counter test.

## 7. Phase E — Depth and ecosystem

### MS-09 — Modding

**Goal:** a modder can add real content in minutes, and a server operator can
run untrusted mods safely.

**Deliverables**

- Content pack SDK published as a crate + JSON schemas + validator + docs;
  pack template (`cargo generate`-style); example packs (blocks, machines,
  biomes, UI, minigame).
- WASM mod host: wasmtime integration, capability model, quota system (CPU
  epoch/fuel, memory, host-call rate, action budget), sandboxed filesystem and
  network access behind capabilities, deterministic mode for mods that affect
  world state, panic/trap containment, hot reload in development.
- Host ABI v1: world queries (block/section reads with limits), block edits,
  entity spawn/query, inventory APIs, scheduling (tick/after), events
  (player join/leave/chat, block break/place, entity death, world tick), UI
  data binding, storage KV, server commands, content registration at load time.
- Server mod verification: manifest signing, pack hashes in the handshake,
  operator allowlist, client-side content delivery of packs the server
  authorises (never mod code).
- Per-mod profiler and cost attribution, plus a mod health panel.
- Migration of the entire `base` game onto the public API (with a CI check that
  no `base` behaviour needs a private hook).
- API stability policy, deprecation process, and a compatibility test that runs
  the previous release's example mods against the new ABI.

**Exit criteria**

- [ ] From `git clone` of the template to a new block visible in-game in ≤ 10
      minutes following the docs (timed, recorded).
- [ ] A WASM mod adds a block, an item, a recipe, a mob behaviour and a UI
      screen, and works on a server with unmodified clients.
- [ ] Malicious mod corpus (infinite loop, memory bomb, filesystem traversal,
      network exfiltration, registry corruption, panic) is contained: host stays
      alive, offending mod is disabled with a log entry, no capability escape.
- [ ] Per-mod overhead ≤ 0.3 ms/frame client, ≤ 1.5 ms/tick server with the
      reference mod set (budgets from [spec/25](../spec/25-performance-budgets.md)).
- [ ] The base game passes the "no private hooks" CI check; deleting the mod
      loading code would be a compile error only in `ob_modding`, not in
      gameplay crates.
- [ ] Hot reload of a WASM mod preserves world state and completes in ≤ 5 s.

### MS-11 — Depth (mobs, redstone, magic, progression)

**Goal:** the systems that turn a sandbox into a world with things happening in
it.

**Deliverables**

- Entity framework: entity definitions as data, component-based AI behaviours,
  goals/priority system, pathfinding (A*, 3D, jump/cost-aware), sensory model,
  navigation budgets, spawning rules (light level, biome, height, structures),
  mob caps and persistence, boss bars.
- Mobs: the Tier-2 set — passive animals, neutral mobs, hostile mobs, villagers
  with professions/trades, golems, aquatic, nether and end mobs, two bosses
  (dragon and wither equivalents), pet/breeding behaviours, mounted mobs.
- Redstone: signal model (power 0–15, weak/strong), dust, torches, repeaters,
  comparators, pistons/sticky pistons with block movement, observers,
  dispensers/droppers, hoppers, note blocks, daylight sensors, pressure plates,
  tripwire, rails, buttons/levers/targets, redstone lamps, update ordering and
  quasi-connectivity decisions documented, deterministic across regions,
  update budget and loop detection.
- Farming and husbandry: crops, bonemeal behaviour, bee/animal products,
  villagers as farmers.
- Enchanting/anvil/brewing/potions/trading full loops; loot chests; structures
  loot.
- Dimensions progression: portal activation, nether/end travel, dragon fight,
  end cities, elytra, respawn anchors.
- Commands and game rules; statistics and advancements; spectator mode.
- Minecarts, rails, boats; projectiles; falling blocks; TNT.

**Exit criteria**

- [ ] Parity corpus: mob, redstone, enchanting, trading and command checklists
      pass at Tier 2 (with documented deliberate divergences).
- [ ] Reference redstone circuits (door, flush piston door, clock, comparator
      logic, item sorter, observer-driven farm) behave deterministically across
      save/load and region boundaries; a cross-region redstone test suite passes.
- [ ] 500 mobs in a loaded area tick within 25% of the tick budget; pathfinding
      has a per-tick budget and degrades gracefully (no tick death spiral).
- [ ] Two players fight the dragon and complete End progression in a recorded
      session; boss state persists through save/load.
- [ ] Entity replication: 200 visible entities at 20 Hz stay within 20 KB/s per
      player in the benchmark.

### MS-12 — Visuals + audio (`0.5.0-beta`)

**Goal:** the game looks and sounds like a modern product, and shader packs have
a real contract.

**Deliverables**

- Rendering: GTAO, SSR for water, volumetric fog and clouds, improved sky/
  atmosphere, bloom, tone mapping options, particle system (CPU simulated,
  GPU instanced), weather visuals, dynamic lights (torches/emitters) within the
  light budget, quality presets, FSR-style render scaling (our own), optional
  temporal upscaler hooks.
- Shader pack API v1: declared hooks (geometry output, lighting injection, post
  chain), capability manifest, validation, hot reload, sample packs, docs;
  visual-only enforcement (packs cannot change gameplay).
- Audio: full SFX set from the content pack, positional ambience, occlusion/
  reverb zones, music with crossfades and context selection, dynamic range and
  subtitles, per-channel volume, audio event editor tool.
- Particles and effects: block break particles, weather, footstep surfaces,
  underwater/under-lava effects, screen effects (hurt, nausea, portal).
- Controller support and input remapping polish; photo mode (optional).

**Exit criteria**

- [ ] Ultra preset at 512 blocks holds 120 FPS on the reference GPU; measured
      cost per effect documented and bounded by [spec/25](../spec/25-performance-budgets.md).
- [ ] Frame-time p99 stays within budget with 200 mobs + weather + particles in
      the stress scene.
- [ ] A sample shader pack written by an outside tester (docs only) loads,
      passes validation, and changes the look without breaking gameplay or
      anti-cheat rules.
- [ ] Audio: no clicks/pops on rapid sound events, ≤ 64 voices + 4 streams by
      default, audio thread never blocks the main thread (instrumented).
- [ ] All Tier-2 parity visuals reviewed against the checklist; the
      "deliberately different" list is updated.

## 8. Phase F — Parity, scale and polish

### MS-14 — Parity + polish (`0.6.0-beta`)

**Goal:** close the Tier-2 parity gap and make the whole thing feel finished.

**Deliverables**

- Generated parity checklist from registry dumps; executable parity corpus:
  golden worlds, scripted scenarios, screenshot comparisons, mechanic assertions.
- Closure of the remaining Tier-2 items: spectator, remaining blocks/items/mobs,
  structures, commands/game rules, statistics/advancements, full UI screens,
  world creation options (seeds, gamemode, difficulty, generator settings).
- Accessibility: narrator (menu + chat + HUD), full remapping, colour-blind
  palettes, subtitles/captions, reduced motion, UI scale, controller glyphs,
  hold-to-toggle options, one-handed layouts where feasible.
- Localization completeness: full catalogs, pseudolocale lint, RTL rendering
  test, font fallback for CJK/emoji, community translation workflow.
- UX polish: onboarding, tooltips, recipe book, search, key hints, error dialogs,
  crash recovery UI, telemetry-free diagnostics export.
- Performance presets that actually target hardware tiers, plus an auto-detect
  setting.
- Parity delta process documented for future Minecraft drops (DEC-022).

**Exit criteria**

- [ ] Parity corpus ≥ 90% of Tier 2 checks pass; every failure is either fixed
      or documented as a deliberate divergence with a replacement.
- [ ] Accessibility checklist passes with a manual audit by two external testers.
- [ ] A player who has never played OpenBlock completes the onboarding and first
      night without external help (5-user usability test, recorded).
- [ ] Two shipped locales render correctly (including RTL) with no truncation.
- [ ] The game reaches `0.6` with no P0/P1 open bugs; the bug triage policy is
      documented and enforced.

### MS-15 — Scale + anti-cheat (`1.0.0-rc`)

**Goal:** public servers run stably with 50+ players and cheaters do not ruin it.

**Deliverables**

- Performance at scale: parallel region ticking (if not already), interest-set
  tuning, replication batching, entity culling, chunk send budgeting, tick
  slicing, memory pressure handling.
- Anti-cheat validation suite: movement (speed, acceleration, collision, noclip,
  fly), reach and line of sight, block-break speed, placement rules, inventory
  transactions, combat (range, cooldown, rotation), chat rate, packet rate;
  configurable enforcement (log → rubber-band → kick), replay evidence bundles.
- Moderation: chat filter hooks, mute/ban/kick with reasons, report command,
  admin audit log, server-side screenshots not required (privacy).
- Scale harness: bot swarm (500 bots), join storm (50 simultaneous joins),
  world with 20k loaded chunks, memory leak detection over 24 h.
- DDoS/abuse hardening: connection limits, handshake rate limits, Retry,
  per-IP quotas, decompression bomb guards, log-flood protection.
- Server capacity calculator and published benchmark matrix (vCPU × players ×
  simulation distance).

**Exit criteria**

- [ ] 50 bots at 256-block simulation distance on the 4 vCPU reference: 1 hour,
      20 TPS, p99 tick ≤ 35 ms, RSS within budget, no leak (RSS slope ≈ 0).
- [ ] 500-bot join storm and 24-hour soak test pass without crash or save
      corruption.
- [ ] Anti-cheat suite: scripted cheater bots (fly, speed, reach, nuker,
      duplication, combat aura) are detected within ≤ 5 s and actioned according
      to config; false-positive rate on legitimate movement corpus ≤ 0.1%.
- [ ] Fuzz corpora (protocol, save, pack, mod ABI) run 24 h with no crash.
- [ ] Security review completed with no open high-severity findings (see
      [spec/20](../spec/20-security-and-anti-cheat.md)).

## 9. Phase G — Launch

### MS-16 — 1.0 launch (`1.0.0`)

**Goal:** a release a stranger can install, run and trust.

**Deliverables**

- Signed builds for Windows (MSI/portable), Linux (AppImage + Flatpak + AUR),
  macOS (universal, notarised), and server tarballs + containers; reproducible
  build documentation.
- Auto-updater: signed manifest, delta patches, rollback, staged rollout
  channels (stable/beta/nightly).
- Website: download, docs, server list (opt-in), mod/content index (curated
  links), roadmap, contributor guide; documentation site generated from
  `docs/`.
- Save migration tooling from every `0.x` release; protocol documented; mod API
  1.0 frozen with compatibility promise and migration guide.
- Launch content: completed base pack art/audio, tutorial world, 2–3 showcase
  worlds and a showcase mod pack.
- Trademark clearance sign-off (DEC-037), EULA/terms, privacy policy (no
  telemetry by default), security disclosure policy, support process.
- Launch trailer, devlog, press kit, and community channels (Discord/Matrix,
  GitHub Discussions).

**1.0 ship criteria**

- [ ] All prior milestone criteria hold on the release candidate.
- [ ] A `0.1.0-alpha` save migrates to `1.0.0` losslessly and a `1.0` save opens
      in the next patch release.
- [ ] 30 consecutive days of release-candidate play with no P0 bug; no known
      exploitable duplication or remote-crash issue.
- [ ] Dedicated server runs 7 days unattended on the reference VPS without leak,
      corruption or TPS degradation.
- [ ] Storefront/platform requirements met (Steam Deck verification if shipping
      on Steam, macOS notarisation, Windows code signing).
- [ ] Documentation is complete enough that a contributor can land their first
      PR without a maintainer's help (verified with a first-time contributor).

## 10. Post-1.0 milestones

| ID | Theme | Exit criteria sketch | Effort |
| --- | --- | --- | --- |
| MS-17 | Flagship content I (1.1) | Programmable logic blocks + energy/fluid networks shipped as first-party packs, built only on the public API; docs and showcase world | 10–14 |
| MS-18 | Dungeons and map sharing (1.2) | Procedural dungeon API; adventure showcase; map/structure sharing format; in-game browser for local content | 8–12 |
| MS-19 | Android (1.3) | Play Store release, touch UI, mobile perf tier (30 FPS on a 2023 mid-range phone), save compatibility with desktop | 8–12 |
| MS-20 | Far terrain (1.4) | Visual-only LOD out to 128+ chunks, stable 120 FPS at 1024 blocks | 4–6 |
| MS-21 | Native plugins + proxy (1.5) | Signed native plugin tier in a supervised process; multi-server proxy/lobby with transfer | 6–10 |
| MS-22 | Web and reach (1.6) | WebTransport feasibility report + web client spike; accessibility deep pass; Steam Deck verified | 8–12 |
| MS-23 | Compatibility gateway (TBD) | Optional isolated Minecraft-client gateway for one pinned MC version, best-effort, never core | 10–16 |

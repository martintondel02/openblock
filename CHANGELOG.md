# Changelog

All notable changes to OpenBlock are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/) from `1.0.0` onward. Pre-1.0
releases may change APIs, save formats and the protocol between minors; see
[docs/plan/00-roadmap.md](docs/plan/00-roadmap.md) §2.

## [0.0.2] — 2026-11-24 (tech preview, MS-04 slice)

### Added
- Textured rendering: 14-layer procedural block texture array (grass, dirt,
  stone, sand, water, logs, leaves, bedrock, gravel, clay, snow), UV+layer
  vertex format, texture-array sampling in the shared pipeline.
- Greedy meshing with ambient occlusion: per-face/slice masks merged by
  texture, brightness and AO key; a 16x16 water surface becomes 6 quads, a 4x4
  platform top becomes 1 quad.
- Frustum culling for both the windowed and offscreen renderers; culled
  sections are no longer meshed or drawn.
- Benchmark harness: `openblock-tools bench` with cached-geometry frames,
  p50/p95/max timing, JSON reports and a `--max-frame-ms` CI gate (lavapipe on
  Ubuntu).
- World-generation quality: mountains and plateaus, snow peaks, stone slopes,
  sand/gravel/clay shores, connected tree canopies and sparse boulders.
- Client diagnostics: `--seconds N` runs the window for N seconds and prints
  mean FPS with p50/p95/p99 frame times; `--x11` forces the X11 backend.
- Graceful server shutdown: Ctrl-C/SIGTERM saves before exit.

### Changed
- Save format v2: zstd-compressed payload using the pure-Rust zstd crate, with
  compact uniform light storage (no 8 KiB array for uniform sections) and v1
  read compatibility. The 169-chunk reference world shrank from 35.6 MiB to
  48.9 KiB; host memory for 504 sections dropped from 4,416 KiB to 576 KiB.
- Content registry has 12 block types with per-face textures; hotbar unchanged.
- `RenderStats.sections` now counts sections drawn after culling.

### Measured
- See [docs/plan/08-measurements.md](docs/plan/08-measurements.md): 115 FPS at a
  128-block radius and 135 FPS at a 192-block radius on RADV integrated graphics
  at 1280x720 (steady state); 0.62 ms p50 for cached offscreen frames.

## [0.0.1] — 2026-11-24 (tech preview, specification stage)

The first runnable build: a windowed voxel client on wgpu 30 and a headless
host, wired to the first implementation of the engine core.

### Added
- `openblock` client: winit 0.30 window, wgpu 30 renderer, procedural terrain
  streaming, walk/fly AABB physics, block break/place, hotbar selection, world
  autosave, FPS/position in the window title.
- `openblock-server`: headless 20 Hz host with the same simulation code as the
  client, autosave, world reload, `--smoke` determinism check and `--ticks N`.
- Engine foundation: coordinate/AABB/RNG crate, in-code block registry,
  paletted 16³ sections with 4/8/16-bit palettes, packed sky/RGB light storage,
  chunk columns, raycasting and dirty-section tracking; deterministic terrain
  generator; player physics and block interaction; v0 snapshot save format
  with CRC32C; wgpu mesher with face shading and ambient occlusion.
- Offscreen render-to-PNG path (`--screenshot`) that works without a window or
  display, plus a GPU test that verifies non-black geometry output and skips
  cleanly when no Vulkan adapter exists.
- Linux/Fedora-first CI: Fedora 44/45 container jobs, a headless host smoke test
  in every Rust job, and a lavapipe offscreen render check on Ubuntu.

### Changed
- Crate versions move to 0.0.1; packaging metadata and Flatpak manifest are
  pinned to the same version.
- Networking, modding, UI text, audio and the GPU-driven render path remain
  later milestones (MS-06 and onward); this build is a single-player tech
  preview.

## [Unreleased]

### Added

- Project specification (`docs/spec/`), research (`docs/research/`), roadmap and
  milestones (`docs/plan/`), decision register (`docs/decisions.md`) and ADRs
  (`docs/adr/`).
- Cargo workspace skeleton for the crate topology in DEC-003, CI, license and
  provenance policy.
- **Fedora-first platform decision (DEC-054)**: Fedora 44/45 x86_64 (Wayland,
  Vulkan, PipeWire) is the primary platform and reference distribution; Flathub
  Flatpak, COPR RPM, a hardened Fedora Server RPM/systemd/firewalld package, and
  required `fedora:44`/`fedora:45` CI jobs. Packaging scaffolding in `packaging/`.
- Linux-first platform section in the README, Fedora build prerequisites, and
  Fedora-specific requirements in spec/02, 03, 14, 19, 21, 22, 24 and 25.

### Changed

- **Documentation re-baseline after the 2026-11-24 review board.** The plan is now
  two tracks: 1.0 Early Access at Tier-1 parity (~200–240 engineering
  dev-weeks) and full Tier-2 parity at 2.0 (`MS-17`, ~500–800 dev-weeks). The
  previous 159-dev-week total was 5–8× below the project's own research
  inventory. Roadmap effort/calendar/content/critical-path sections, milestones,
  MVP, work breakdown and risks are reconciled; the README separates 1.0 from
  2.0 and post-1.0.
- **DEC-051:** the parity gate is defined once as two numbers — 100% of in-scope
  checks pass, and ≥ 85% vanilla coverage from a hand-authored behavioural
  checklist. The "≥ 90% coverage" pass claim is retired; coverage and pass rate
  are reported separately, and full Tier-2 parity is 2.0, not 1.0.
- **DEC-048:** 32³ render mesh clusters are the MS-04 default (16³ remains the
  storage/edit/light/network unit); OPEN-001 is closed.
- **DEC-049:** 1.0 clients run no mod or WASM code; mod UIs use server-computed
  data. OPEN-006 is closed and the client mod CPU budget is a reserved envelope.
- **DEC-050:** translations are not part of the join-critical hash; `content_hash`
  is strict and `view_hash` is advisory unless the server opts into strict view
  matching.
- **DEC-052:** dedicated servers default to a 256-block render distance equal to
  simulation distance; 512 blocks is opt-in with a published cost, and capacity
  claims must state co-located versus distributed players.
- **DEC-053:** the base pack uses original dimension and entity names
  (`openblock:underworld`, `openblock:terminus`); Minecraft names do not appear in
  shipped content identifiers or base-pack data.
- Milestone detail: MS-01 light budget corrected to 8 KiB per mixed section, MS-04
  cluster default, MS-11 scoped to Tier-1 depth, MS-14 redefined as Tier-1 parity
  plus corpus infrastructure and Tier-2 coverage reporting, MS-15 split into
  co-located and distributed capacity rows, MS-16 redefined as the Early Access
  launch, and `MS-17` added for the 2.0 track.
- Documentation index now lists `research/10-custom-content-and-differentiation.md`
  and `plan/07-review-findings.md`; ADR and milestone counts are corrected.

No playable build exists yet; the first one is milestone `MS-03` (`0.0.1`).

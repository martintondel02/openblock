# Measurements log

> Status: living record · Last updated: 2026-11-25
> Methodology is defined in [spec/25](../spec/25-performance-budgets.md). Entries are
> raw measurements on named hardware, never marketing claims.

## 2026-11-24 — v0.0.2 (textures, greedy meshing, frustum culling, zstd saves)

### Environment

| | |
| --- | --- |
| OS | Fedora Linux 45 (Wayland) |
| CPU/GPU | AMD Ryzen 7 9800X3D, integrated RADV "RAPHAEL_MENDOCINO" (Vulkan) |
| Renderer | wgpu 30.0.2, Vulkan/RADV, 1280×720 window (FIFO present) |
| Build | `cargo build --release` (thin LTO, codegen-units 1) |
| World | seed 42, procedural terrain, default content pack |

### Client frame time (window, steady state, first 2 s excluded)

| Render distance | Mean FPS | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| 8 chunks (128-block radius) | 115.4 | 8.75 ms | 16.86 ms | 18.68 ms | 22.37 ms |
| 12 chunks (192-block radius) | 135.5 | 8.69 ms | 9.36 ms | 11.05 ms | 17.82 ms |

Command: `openblock --seed 42 --fly --seconds 10 --render-distance N`.
The 12-chunk run is faster because the world was already generated and the
window was already warm; the 8-chunk run still streamed a few columns. Neither
run is a 512-block gate: the reference gate in spec/25 is a discrete GPU
(RTX 3060 class) at a 512-block radius, which this integrated GPU cannot stand
in for. The numbers show the frame path is healthy at 720p, not that the MS-04
gate is met.

### Renderer and meshing (offscreen, cached geometry)

| Metric | Value |
| --- | --- |
| Cached sections (render distance 8) | 1,133 |
| Triangles (all cached sections) | 106,398 |
| One-shot cache build (mesh + upload, CPU) | 1,302 ms total ≈ 1.15 ms/section |
| Cached frame (raster + GPU readback), 1280×720 | p50 0.62 ms, p95 0.71 ms |
| Draw calls (frustum-visible) | 244 of 1,133 cached sections |
| Offscreen screenshot | `/docs/img/v0.0.2-screenshot.png` (16,745 greedy quads) |

Command: `openblock-tools bench --frames 30 --seed 42 --render-distance 8`.
Greedy meshing reduced a 16×16 water surface from 544 unit faces to 6 quads and
a 4×4 platform top to 1 quad; frustum culling drops ~79% of cached sections for
the default camera.

### World generation and storage

| Metric | Value |
| --- | --- |
| Terrain generation, 300 chunks | 0.94 ms/chunk average, 1.10 ms worst |
| Save size, 169 chunks, v0.0.1 snapshot | 36,473,830 B (35.6 MiB) |
| Save size, same world, v0.0.2 (zstd + compact light) | **50,049 B (48.9 KiB)**, 729× smaller |
| Uncompressed payload | 4,956,016 B → 50,022 B compressed (99.1×) |
| Host heap, 21 chunks / 504 sections | 4,416 KiB → **576 KiB** after compact light |
| Save v1 read compatibility | verified on a real 169-chunk v1 save (upgraded to v2 on load) |

### CI gates active at this commit

- `cargo test --workspace --all-targets`: 92 tests, including a real RADV
  offscreen render (skips cleanly with no adapter).
- Headless host `--smoke` determinism hash: identical across save/load and
  across processes.
- `openblock-tools bench` on lavapipe in Ubuntu CI (`--max-frame-ms 2000`).
- Fedora 44/45 container jobs, Windows and macOS builds, clippy `-D warnings`,
  rustdoc, architecture guard, dependency policy, docs consistency.

### Known measurement gaps (honest)

- No frame-time gate at a 512-block radius yet: that needs the MS-04 GPU-driven
  submission and a discrete-GPU bench box.
- Meshing is still main-thread and budgeted per frame; cached-section build is
  ~1.15 ms/section and becomes the bottleneck at large radii. Worker-thread
  meshing and a GPU upload ring are the next MS-04 tasks.
- Window numbers include the compositor; no GPU timestamp queries yet, so
  CPU-vs-GPU split is not separated.
- Offscreen "cached frame" includes a GPU-to-CPU readback to force completion;
  it is a comparable proxy, not GPU-only time.

## Open questions

1. **Bench box** (owner MS-04): the 512-block gate needs a discrete-GPU bench
   box with a pinned driver; none exists yet and no budget line is allocated.
2. **GPU timestamps** (owner MS-04): enable `TIMESTAMP_QUERY` where available to
   split CPU, GPU and readback cost instead of reporting one blended frame time.
3. **Parallel meshing** (owner MS-04): move section meshing to worker threads and
   measure the upload ring under a 30 blocks/s fly-through with p99 gates.
4. **Vsync/present mode** (owner MS-12): the window numbers use FIFO; measure
   uncapped and VRR modes separately and record the display refresh rate.

## References

- [Performance budgets](../spec/25-performance-budgets.md) — normative targets,
  reference machines and the measurement protocol
- [Rendering](../spec/03-rendering.md) — meshing, submission and culling design
- [Persistence](../spec/17-persistence.md) — save format and compression
- [Milestones](01-milestones.md) — MS-04 exit criteria
- Benchmark artifacts: `perf/reports/`, `perf/budgets.toml`, `perf/scenes.toml`

## 2026-11-24 — v0.0.3 (parallel meshing/generation, HUD/F3/settings)

### Parallel chunk generation (Host::ensure_streaming, rayon)

Serial baseline and parallel final on the same ignored throughput probe
(256 chunks, budget 256, fresh world):

| Metric | Serial | Parallel | Speedup |
| --- | ---: | ---: | ---: |
| 256 chunks wall time | 116.8 ms | 13.2 ms | 8.8× |
| Throughput | 2,191 chunks/s | ~19,200 chunks/s | 8.8× |

Determinism is unchanged: `--smoke` produced the identical hash
`8857c89203e37ab4` with `RAYON_NUM_THREADS=1`, `4` and `16`. Host memory for
504 sections stayed 576 KiB.

### Parallel section meshing (client MeshScheduler)

Methodology caveat: the client has no headless surface path in this environment,
so these runs used a hidden/unmapped winit window with temporary instrumentation
that was removed before release; a strict watchdog prevents further windowed
runs. Treat as indicative, not a CI gate.

Warm 10 s runs (seed 42, pre-generated worlds, RADV iGPU, 1280×720):

| Configuration | Before (serial mesher) | After (worker mesher) |
| --- | ---: | ---: |
| RD12 mean FPS (median of runs) | 92.8 | 145.2 |
| RD12 stable runs | 181.6 once backlog drained | 174.5–174.7 FPS, p50 4.6–5.7 ms, p95 11.0–11.5 ms |
| RD16 uncapped | 59.9–105.1 | 174.9 FPS, p50 4.7–5.0 ms, p95 13.1–14.2 ms |
| Cold start, first 2 s mean frame | 9.80 ms | 5.68 ms (−42%) |
| Cold start, first 2 s p50 | 9.75 ms | 5.52 ms (−43%) |

Snapshot capture costs ~25.9 µs per solid section, so 32 submissions/frame cost
~0.83 ms — cheap relative to the 8 ms previous main-thread mesh budget.

### UI and tests

- `ob_render::ui`: one pass, three texture batches (solid/font/blocks), 36-byte
  vertex stride, generated 128×64 font atlas. GPU tests verify exact solid
  colours, text ink pixels, block-icon colour variety and a UI overlay drawn on
  top of a rendered world.
- Workspace test count: 149 (v0.0.2: 92), including GPU tests that skip cleanly
  when no Vulkan/Metal/DX12 adapter exists.
- World save and host memory: unchanged from v0.0.2 (48.9 KiB for the 169-chunk
  reference world; 576 KiB heap for 504 sections).

### Known gaps

- F3/settings have no automated screenshot golden yet; the HUD is verified by a
  GPU overlay test and by the offscreen `--screenshot` path.
- Window FPS numbers above still include compositor/present limits and hidden
  window caveats; the offscreen benchmark remains the CI gate.

## 2026-11-24 — v0.0.4 (texture styles, fog, sky)

- Four procedural texture styles were added (classic 16², painterly 32²,
  lush 32², minimal 16²), each with a full mip chain (nearest magnification,
  linear minification/mip filtering).
- Alpha cut-out leaves and a deterministic per-quad brightness variation
  (±5%) add visual depth without changing the geometry or batching model.
- Sky gradient + distance fog replace the flat clear colour. Fog resolves to the
  horizon colour, so chunk edges dissolve into the sky.
- Rendering cost unchanged within noise: the cached offscreen benchmark still
  reports ~0.6–0.7 ms p50 at 1280×720 on RADV integrated graphics (fog/sky are
  one fullscreen triangle plus a few ALU ops per fragment; mips reduce texture
  cache pressure at distance).
- Workspace tests: 166 (v0.0.3: 149), including CPU tests for every texture
  style, mip determinism and leaf alpha; GPU texture-grid previews remain an
  explicitly-run ignored test to keep CI GPU-free.

## 2026-11-24 — v0.0.5 (inventory, mining, day/night, biomes)

### World generation (biomes)

- Five biomes were added (plains, forest, desert, snowy taiga, mountains) using
  low-frequency temperature/humidity/highland fields and smooth weights that
  also blend height parameters, so biome borders have no height seams.
- Public 512×512 biome shares: seed 1 — plains 14.6%, forest 22.7%, desert
  21.3%, snowy taiga 15.8%, mountains 25.6%; seed 42 — 9.0 / 26.5 / 9.8 / 12.6 /
  42.2%. Every biome is present within the default world's streaming area for
  the seeds tested.
- Generation throughput (test profile, opt-level 1): 300 chunks in 435 ms =
  1.45 ms/chunk, worst chunk 1.85 ms. This is well under the 2 ms/chunk budget
  used for streaming, and chunk generation is parallelised across the rayon
  pool in `Host::ensure_streaming`.
- Effective tree densities (seed 42, 256×256 columns): forest 1 trunk / 50
  columns, taiga 1 / 55, plains 1 / 145, mountains 1 / 754, desert 0; taiga also
  places supported 2×2 snow drifts.

### Rendering (day/night + outline)

- The day/night environment is presentation-only and does not affect the
  simulation/save hash. Sun path: east at sunrise, zenith at noon, west at
  sunset; moonlight keeps block light at 0.154–0.22 of daylight, and night
  geometry measured 148 → 65 mean luma (readable, not black).
- Star field: deterministic hash over a rotating azimuth/altitude grid; 0 star
  pixels at noon vs 224 isolated star pixels at midnight in the GPU test.
- Targeted-block outline: 24-vertex line list drawn after the world pass,
  depth-tested with no depth writes; GPU test finds 0 outline pixels without a
  target vs 114 with one, all four projected top edges, and full occlusion
  behind a nearer pillar.
- Cost is negligible: one fullscreen sky triangle, a handful of ALU ops per
  fragment, one 24-vertex line-list draw. The cached offscreen benchmark gate is
  unchanged.

### Interactive systems

- Inventory uses the existing immediate-mode UI renderer (no new draw calls per
  widget beyond the existing batches); the whole frame stays within the UI
  batch budget.
- Mining is a pure state machine driven by the frame clock; the per-block
  hardness table lives in `ob_content` (stone 1.5 s, log 2.0 s, leaves 0.2 s,
  bedrock/water unbreakable) so tools and future content can reuse it.
- World tests: 216 total (biome determinism/surfaces/trees, environment/outline
  CPU and GPU tests, inventory/mining/UI tests, plus the pre-existing suites).

## 2026-11-24 — v0.0.6 (runtime resource packs)

- Added a runtime loader for Minecraft-format block texture packs; no assets are
  bundled. Reference local pack (rotrBLOCKS 128×): target array 128×128, 8 mip
  levels, only `debug` and `water` fall back to procedural layers; loading time is
  dominated by PNG decode and is done once at startup.
- Biome tinting/compositing (grayscale grass + leaves, grass-side overlay) is CPU
  work done once during load and has no per-frame cost.
- Texture memory: 14 layers × 128² × 4 B × 1.33 mip ≈ 1.2 MiB, well inside the
  renderer budget. The world pipeline, UI pipeline and benchmark gates are
  unchanged.
- Test count: 244 (216 in v0.0.5), including synthetic-pack CPU tests for layout
  resolution, scaling, animated frames, alpha and tinting, GPU array/renderer
  tests, and an ignored real-pack load test.

## 2026-11-24 — v0.0.7 (survival foundations, movement)

- Survival systems are simulation-rate only: fall damage/hunger/starvation/regen
  run on the existing 20 Hz tick and per-tick cost is a handful of float ops plus
  an inventory lookup on block actions. No measurable frame-time change; the
  offscreen benchmark gate is unchanged.
- Movement: crouch caps target speed at 1.31 m/s (walk 4.317, sprint 5.612),
  lowers the eye from 1.62 m to 1.27 m, and adds a support probe (one thin AABB
  test against the existing voxel query) per horizontal axis per tick while
  sneaking. Sprinting eases the camera FOV to 110% over ~80 ms.
- Persistence: player.txt grew from 7 to 10 fields (old saves load with 20/20
  health/hunger and alive); inventory.txt is a small text file written
  atomically with the world. The smoke state hash now covers vitals and
  inventory: `smoke OK hash=00fc740debf500c0` for seed 42.
- Test count: 293 (items/drops, vitals cadence, inventory/actions/persistence,
  crouch/sprint/ledge tests, HUD/death UI).

## 2026-11-24 — v0.0.8 (crafting, tools, dropped items, settings)

- Texture array grew 14 -> 18 layers (planks, cobblestone, crafting table
  top/side) in all four styles, still with full mip chains; memory stays under
  2 MiB at 32 px.
- Dropped-item entities: one instanced draw pass, one 40-byte instance per item
  (grows on demand), a static 36-vertex cube and no extra textures/uniforms.
  Server physics is 20 Hz over a handful of items; pickup/persistence costs are
  negligible. The chunk benchmark gate is unchanged.
- Crafting/mining math is a few registry lookups per break/craft; the tool speed
  table replaces raw hardness in the client mining timer.
- Save format v4 (append-only registry migration from v1-v3); smoke seed 42:
  `smoke OK hash=62b21c82b5d848e0`, stable across two fresh runs and including
  dropped-item entities.
- Test count: 367 (293 in v0.0.7).

## 2026-11-24 — v0.0.9 (UI readability, pickup radius)

- F3 overlay layout is pure CPU UI work, drawn in the existing UI pass: 16
  stat lines in four grouped sections at scale 1.8 (side column 1.6), line step
  22.4 px with +8 px gaps; no measurable frame-time change. Adaptive
  side-by-side/stacked/left-only layouts keep 1280x720 and 960x540 readable.
- Menu spacing changes are static layout constants (settings 620x520, crafting
  620x560, larger row pitch) with the same UI pass cost.
- Dropped-item pickup now attracts items within a 3 m radius at 7 m/s and
  collects at 0.9 m; per-item extra cost is one distance test plus a lerp per
  tick for the handful of nearby entities. No measurable server-tick change.
- Smoke hash unchanged (`62b21c82b5d848e0`, seed 42); test count 382 (367 in
  v0.0.8), including new F3 layout/960x540, menu-spacing, clickable-arrow, and
  pickup-radius tests.

## 2026-11-25 — 1.0.0 worldgen slice (deterministic caves and ravines)

### Design and parameters

- Three 3D value-noise fields drive carving: two independent spaghetti fields
  (`|field| < 0.09` on both is a tunnel) and a low-Y cavern field (threshold
  0.52 at y=32, tapering to 0.42 at y=-64; no cavern carving above y=32).
  Noise is sampled on a 5×5×5 lattice per section at four-block spacing and
  trilinearly interpolated per block with integer fixed-point arithmetic.
- Cave carving stops 3 blocks below the surface (5 under water, 8 inside the
  spawn-protection square) and never removes water, bedrock or the bottom world
  row. A rare per-column hash (1 in 4,096) opens a one-column shaft when a cave
  with real volume lies within 24 blocks; the shaft removes the seal so every
  entrance reaches an existing cave.
- Ravines are one candidate per 8 regions of 128×128 blocks (1 in 8), with a
  hashed centre/orientation and an ellipsoidal plan: length 40–100, surface
  half-width 3–7, depth 24–52. Half-width drops one block every 6 blocks of
  depth, producing stepped walls; the carve is open to the sky and only on dry
  columns (`height > sea level + 2`).
- Spawn safety: chunks within Chebyshev distance 2 of the origin keep the top
  8 blocks solid, and ravine candidates whose conservative footprint touches
  that 5×5-chunk square are suppressed entirely. No new blocks, dependencies or
  content changes; `generate_chunk` is unchanged and the addition to the public
  API is the diagnostic-only `CaveDebugStats` / `Generator::cave_debug_stats`.

### Throughput (test profile, `opt-level = 1`, 200 chunks/seed)

| Seed | Before (ms/chunk) | After (ms/chunk) | Added |
| --- | ---: | ---: | ---: |
| `0x1` | 1.380 | 2.011 | **+0.631** |
| `0x2a` (42) | 1.407 | 1.980 | **+0.573** |
| `0xc0ffee0012345678` | 1.410 | 2.058 | **+0.648** |

The ≤ 0.8 ms/chunk added-cost target is met (worst observed +0.648). The
before/after runs used separate target directories so Cargo could not reuse a
test binary between the two source revisions; measurements were taken on the
Ryzen 7 9800X3D host. Concurrent agent builds can spike individual runs by more
than 1 ms, so the table uses stable runs; repeat runs varied by about ±0.1 ms.

### Distribution (24×24 chunks = 576 chunks, seed 1 / seed 42)

| Metric | Seed 1 | Seed 42 |
| --- | ---: | ---: |
| Cave air in the stone band | 8.73% | 10.33% |
| Tunnels above y=32 | 6.92% | 6.78% |
| Cave air below y=0 | 10.22% | 12.64% |
| Surface entrance columns | 10 | 11 |
| Air at bedrock row | 0 | 0 |
| Undercut top blocks | 0 | 0 |
| Water directly over carved air | 0 | 0 |

Ravine candidates: 125 per 1,024 regions (seed 1), 104 of them with a dry
centre; sampled centre chunks carve 1,700–3,645 blocks each. Chunk hashes
`hash(0,0)` (FNV-1a over all blocks) for reporting: seed 1
`0x99b954dd0e092968`, seed 42 `0x968563f88323fc44`, seed
`0xc0ffee0012345678` `0x5478ba2ae16ca166`.

### Tests

`cargo test -p ob_worldgen --lib`: 19 passed, 3 ignored reports. New tests cover
out-of-order/same-chunk determinism, cave density bounds and bedrock integrity,
surface entrances (32×32-chunk search for seeds 1 and 42), ravine discovery and
stepped depth, water support, and the spawn crust. Ignored reports:
`cave_throughput_report`, `cave_distribution_report`, and the existing
tree-density report (`--ignored --nocapture`). Clippy
`-p ob_worldgen --all-targets -- -D warnings` and `cargo fmt` are clean.

### Compromises

- The coarse 4-block cave lattice cannot express sub-4-block tunnels; observed
  tunnel/entrance surfaces are blocky at that scale.
- Entrances are 1×1 vertical shafts rather than stairs or sloped openings.
- Caves are dry: there are no aquifers or flowing water; the 5-block underwater
  seal is what keeps water from hanging over carved air.
- Ravines are restricted to dry land and fully suppressed near spawn rather
  than clipped at the protection boundary.

## 2026-11-25 — audio output backend: runtime-loaded PulseAudio (no cpal/ALSA headers)

### Environment

Fedora Linux 45 (Wayland), Ryzen 7 9800X3D, PipeWire 1.6.11 serving the host's
default sink through `pipewire-pulse` (`libpulse-simple.so.0`). Dev/test profile
(`opt-level = 1`) for all numbers, including the end-to-end open/shutdown probe
(`ob_audio` example, `dev-cargo run`).

### Backend probe

| Metric | Value |
| --- | ---: |
| `AudioEngine::new()` end-to-end (bank synthesis + dlopen + `pa_simple_new`) | 29.25 ms |
| Startup bank synthesis (79 clips, test profile) | 19.52 ms |
| Implied PulseAudio load + stream open | ≈ 9.7 ms |
| `Drop` / shutdown (disarm, final drain, free, join) | 22.1–36.9 ms observed |
| Reported status | `pulse: default / 48000 Hz / 2 ch / 25.0 ms` |

The 512-frame writer block is 10.67 ms; the requested server buffer is 25 ms
(`tlength` 9,600 B, `maxlength` 38,400 B). On a host with no PulseAudio server
the same path returns `NoOutputDevice` after the failed `pa_simple_new` with no
panic, and the client continues silently. The CI matrix now builds with no
`libasound2-dev`/`alsa-lib-devel` packages at all.

### Mixer (test profile, one stereo second)

| Metric | Value |
| --- | ---: |
| Bank synthesis, 79 clips | 19.67 ms |
| Mix one second of stereo output | 1.11 ms |
| Real-time cost per 60 Hz frame | ≈ 0.019 ms |

`cargo test -p ob_audio`: 31 passed (including `dlopen` symbol-table fallback,
writer block/error/drop tests). The previous release figures (12.6 ms synthesis,
0.94 ms per stereo second) remain the reference for the unchanged synthesis
and mixer code; this entry adds the backend measurements.

## 2026-11-25 — 1.0.0 worldgen slice (deterministic ore veins)

### Environment and method

Same host and profile as the cave/ravine entry above: Fedora Linux 45, AMD
Ryzen 7 9800X3D, test profile (`[profile.dev] opt-level = 1`, dependencies at
`opt-level = 3`), CPU-only. All runs used
`CARGO_TARGET_DIR=/home/martintondel/Projects/openblock/target-1.0-ores`.
"Without ores" is the same binary and seed with the internal ore pass disabled,
so the added-cost column isolates the vein pass rather than comparing two
source revisions. Chunk hashes are FNV-1a over every finished block.

### Parameters

| Ore | Block id | Y band (inclusive) | Vein size | Target share of band stone |
| --- | --- | ---: | ---: | ---: |
| Coal | `openblock:coal_ore` (15) | 5..=96 | 6–14 blocks (mean 10) | 0.8–1.2% |
| Iron | `openblock:iron_ore` (16) | 5..=64 | 3–8 blocks (mean 5.5) | 0.4–0.7% |
| Copper | `openblock:copper_ore` (17) | 24..=96 | 4–10 blocks (mean 7) | 0.3–0.6% |

Algorithm: after terrain, caves, ravines and surface features, each chunk
derives one independent `Rng` stream per ore from the world seed, the chunk's
world-space origin and a per-ore salt (`COAL`, `IRON`, `COPR`). A vein starts at
a hashed local XZ in the ore's Y band, capped one block below the terrain
surface, and is grown as a connected six-neighbour blob. Every candidate is
written only when it is still `stone`, inside the same chunk and inside the Y
band; the grower tracks the placed cells in a fixed 14-cell buffer and picks a
random existing cell before each step, so veins are compact and connected. The
vein count scales with the chunk's height-band volume rather than a fixed
number of veins per chunk, which keeps density stable on mountains and lowlands. A vein
can stop early only when the chunk border or a cave encloses it.

### Distribution (24×24 = 576 chunks per seed)

Density is the share of pre-ore stone converted to that ore in the ore's Y
band: `ore / (remaining stone + all ores)` restricted to the band. Caves are
already carved, so this is post-carve stone.

| Ore | Target | Seed 1 | Seed 42 |
| --- | ---: | ---: | ---: |
| Coal | 0.8–1.2% | **1.03%** | **1.05%** |
| Iron | 0.4–0.7% | **0.62%** | **0.63%** |
| Copper | 0.3–0.6% | **0.47%** | **0.48%** |

| Ore | Blocks/chunk (seed 1 / 42) | Veins/chunk (seed 1 / 42) | Mean blocks/vein | Chunks with ore |
| --- | ---: | ---: | ---: | ---: |
| Coal | 178.9 / 159.7 | 17.8 / 16.0 | 10.0 | 576 / 576 |
| Iron | 87.7 / 87.3 | 16.0 / 15.9 | 5.5 | 576 / 576 |
| Copper | 60.5 / 52.2 | 8.6 / 7.5 | 7.0 | 576 / 576 |

Every observed ore block fell inside its configured Y band, and all three ores
appeared in every sampled chunk. Blocks/chunk varies with terrain because the
vein count follows the height-band estimate; the density stays within a few
hundredths of a percent across seeds.

### Throughput (test profile, 200 chunks × 2 interleaved rounds per seed)

One representative run (a repeated run produced negative noise on one seed):

| Seed | With ores (ms/chunk) | Without ores (ms/chunk) | Added |
| --- | ---: | ---: | ---: |
| `0x1` | 2.045 | 2.025 | **+0.020** |
| `0x2a` (42) | 1.991 | 1.980 | **+0.011** |
| `0xc0ffee0012345678` | 1.954 | 1.939 | **+0.014** |

The ≤ 0.35 ms/chunk added-cost target is met with a wide margin. Across three
repeat runs the deltas ranged from −0.001 to +0.020 ms/chunk; that spread is
timer run-to-run noise, and the worst observed added cost is +0.020. Total
generation remains ≈ 1.95–2.05 ms/chunk, matching the cave-slice entry, so ores
add under 1% to generation on this host.

### Determinism and hashes

Same seed + chunk coordinates regenerated identically, reverse-order generation
(with unrelated neighbours generated between samples) produced identical ore
layouts, and a different seed produced a different layout. Ore streams share no
RNG state with terrain, caves, ravines or each other. Reference chunk hashes
(`hash(0,0)`, finished blocks with ores; these deliberately differ from the
cave-slice hashes because the chunk payload now includes ore):

| Seed | `hash(0,0)` |
| --- | --- |
| 1 | `0x25d12c39c22015fd` |
| 42 | `0x6716df9f5cf776b2` |
| `0xc0ffee0012345678` | `0x0169a0c9bbc20e12` |

### Tests

`cargo test -p ob_worldgen`: **23 passed, 0 failed, 5 ignored** (the cave slice
left 19 passed, 3 ignored). New non-ignored tests:

- `ores_are_deterministic_and_order_independent` — same seed/pos, reverse order
  with intervening neighbours, regeneration and a different seed; also checks
  `ore_debug_stats` against a fresh block scan.
- `all_three_ores_appear_with_expected_depth_ranges` — 576 chunks, every ore
  present, every block inside its configured band.
- `no_ore_replaces_non_stone_or_floats_in_caves` — generates the same chunks
  with and without ores and asserts every one of 4,573 sampled ore blocks was
  `stone` before the pass and lies below the terrain surface; the top block of
  every sampled column is unchanged.
- `ore_density_is_bounded` — 12×12 chunks for seeds 1, 42 and
  `0xc0ffee0012345678`, integer per-mille bounds from the table above.

Ignored reports: `ore_throughput_report` and `ore_distribution_report` (plus the
existing cave/tree reports). `cargo test -p ob_worldgen --lib -- --ignored
--nocapture` passes all five reports. Clippy
`-p ob_worldgen --all-targets -- -D warnings`, `cargo fmt --check` and rustdoc
with `-D warnings` are clean. The full workspace suite also passed at this
point: `cargo test --workspace --no-fail-fast` reported **547 passed, 0 failed
and 9 ignored**, including the concurrent agents' mob, network and client
work; no external failures needed to be discounted.

### Compromises and deviations

- Ores are engine constants in `ob_worldgen`, not the content-configured
  `ore_vein` features described in spec/06 §3.6; there is no discard chance,
  trapezoid height distribution or `replaceable` tag yet. Spec/06 is outside
  this slice's ownership, so this measurement entry is the slice record.
- Veins are grown entirely inside one chunk, so they clip at chunk borders
  instead of being generated from a cross-chunk feature cell. The adaptive
  per-chunk count compensates for the lost border blocks; individual veins
  never cross a boundary.
- Ores are written only below the terrain surface (`y < height`), so grass,
  sand, snow, exposed stone tops, boulders at `height + 1` and logs/leaves are
  never replaced. An exposed cliff can still show ore at `height - 1`.
- The band contract intentionally leaves stone below Y=5 and above Y=96
  ore-free; the deep world has no coal, iron or copper.
- The no-ore switch is private (`GenerationOptions` plus the internal
  `generate_chunk_with_options`); the only public API addition is
  `OreDebugStats` / `Generator::ore_debug_stats`. `Generator::generate_chunk`
  keeps its signature and now includes ores.
- Because the density denominator is post-carve stone, cave volume affects the
  measured share. The adaptive vein count keeps it stable for the sampled
  seeds; an unusually cave-heavy seed could drift toward an edge of the
  documented bounds.
- `biome_surfaces_and_biome_at_agree` now accepts stone **or** an ore directly
  below desert sand, because ore legitimately replaced that stone; it still
  rejects dirt, air and every other non-stone material.

## 2026-11-25 — 1.0.0 CPU benchmark and soak tools

### Environment and method

Fedora Linux 45 (kernel 7.3.4), AMD Ryzen 7 9800X3D, release build
(`cargo build --release --locked -p ob_tools --bins`; thin LTO,
`codegen-units = 1`), CPU-only, no GPU or window. All commands ran from the
`/tmp/ob-play` worktree with the 1.0 benchmark target directory. This was a
shared machine during the 1.0 integration wave, so the repeat counts below are
part of the record rather than a claim of a quiet bench box.

The four tools and their quick/reference profiles are documented in
[Benchmarks and soak](09-benchmarks.md). Tick percentiles use the nearest-rank
definition of spec/25 §3.7; "TPS" is `1000 / mean tick ms`, not the mean of
instantaneous rates. The offscreen render benchmark is unchanged and remains
the render gate.

### World generation (`worldgen_bench --full`)

40×40 chunks around the origin × 3 seeds = 4,800 chunks, serial
`Generator::generate_chunk` calls; cave stats regenerate each chunk for the
carve report, so wall time is roughly 2× the measured generation time.

| Seed | Mean ms/chunk | p50 | p95 | p99 | Max | Chunks/s | Non-air blocks | Carved blocks | Entrance columns | Coal / iron / copper |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `0x1` | 0.939 | 0.910 | 1.093 | 1.160 | 1.367 | 1,065.6 | 56,090,492 | 5,182,109 | 29 | 281,931 / 139,961 / 95,154 |
| `0x2a` (42) | 0.916 | 0.898 | 1.019 | 1.227 | 1.595 | 1,092.1 | 52,783,106 | 5,599,788 | 33 | 268,683 / 140,023 / 89,475 |
| `0xc0ffee0012345678` | 0.908 | 0.904 | 0.978 | 1.005 | 1.374 | 1,101.4 | 52,752,234 | 4,245,739 | 16 | 258,230 / 138,263 / 84,936 |
| **aggregate** | **0.921** | **0.904** | **1.049** | **1.147** | **1.595** | **1,086.2** | **161,625,832** | **15,027,636** | **78** | **808,844 / 418,247 / 269,565** |

- Carved share of pre-carve solids: seed 1 8.46%, seed 42 9.59%, seed
  `0xc0ffee0012345678` 7.45%. Ore density: 9.22 / 9.44 / 9.13 ore blocks per
  1,000 non-air blocks respectively (9.26 aggregate). Both are in the range of
  the earlier cave/ore entries, so the benchmark sees the same world as the
  feature reports.
- Determinism: 181/181 sampled chunks per seed regenerated to the identical
  canonical block hash (origin, 5×5 spawn-protection square and the square
  edge), and the three seeds produced distinct origin hashes.
- The 1.0 scope target is ≤ 2.5 ms/chunk average: **PASS** at 0.921 ms
  (2.7× margin). The p99 is 1.147 ms and the worst single chunk is 1.595 ms.
- `worldgen_bench --quick` (6×6 × 3 seeds = 108 chunks) runs in **0.31 s**:
  mean 0.940 ms/chunk, p95 1.055, p99 1.138, 1,063 chunks/s.
- The earlier cave/ore entries used the test profile (`opt-level = 1`) at
  ≈1.95–2.05 ms/chunk; release is ≈0.9 ms. The release numbers are the 1.0
  envelope numbers.

### Server simulation (`server_bench`, 2,025 columns)

Seed 42, radius 22 chunks (45×45 = 2,025 columns), 1,000 measured ticks per
phase, 512 dropped items, 64 lit furnaces and 32 full chests. The `mobs` phase
pre-rolls a day/night cycle with passive spawning suppressed until late
afternoon, then measures at night; it reached the 18-mob cap before the phase
(16 remained at the end as individual mobs aged/wandered). Tick samples exclude
chunk generation because `Host::tick` does not stream; streaming is measured
separately.

| Scenario | Ticks | Mean ms | p50 | p95 | p99 | Max | TPS | p99 ≤ 35 ms | Population at end |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: | --- |
| idle | 1,000 | 0.0001 | 0.0000 | 0.0001 | 0.0002 | 0.0155 | 14,230,824 | **PASS** | 0 mobs, 0 items, 0 containers |
| player (flying circle) | 1,000 | 0.0037 | 0.0019 | 0.0084 | 0.0091 | 0.0160 | 271,032 | **PASS** | 1 mob, 0 items |
| mobs (night caps) | 1,000 | 0.0146 | 0.0145 | 0.0175 | 0.0190 | 0.0244 | 68,481 | **PASS** | 16 mobs, 0 items |
| items (512 entities) | 1,000 | 0.0212 | 0.0211 | 0.0236 | 0.0261 | 0.0413 | 47,173 | **PASS** | 10 mobs, 512 items |
| containers (64 furnaces, 32 chests) | 1,000 | 0.0185 | 0.0184 | 0.0226 | 0.0242 | 0.0262 | 54,106 | **PASS** | 8 mobs, 512 items, 64/32 |

Repeat p99 ranges over three runs: player 0.0090–0.0097 ms, mobs
0.0190–0.0251 ms, items 0.0261–0.0367 ms (one 0.165 ms scheduling outlier),
containers 0.0242–0.0256 ms. Marginal mean cost over idle is +0.0036 ms
(player), +0.0145 ms (mobs), +0.0211 ms (items) and +0.0184 ms (containers).
All measured p99 values are three orders of magnitude below the 35 ms
single-player baseline. This is **not** the 50-player, 2,000-column
`tick50` capacity case: one host player is ticked, not 50 network players.

Streaming and memory:

| Metric | Value |
| --- | ---: |
| Initial cold stream (new columns) | 2,020 columns in 210.5 ms = 0.104 ms/chunk, 9,596 chunks/s |
| Fresh 9×9 area, 32 chunks away | 81 columns in 9.7 ms = 0.119 ms/chunk, 8,371 chunks/s |
| Process VmRSS after streaming | 168.1 MiB |
| Host world heap at 2,025 columns | **155.01 MiB (162.54 MB decimal)**; scaled to 2,000 columns ≈160.5 MB, ≈0.3% over the 160 MB PERF-MEM-013 budget |
| Process VmRSS after save/reload | 330.3 MiB |

Save/reload at 2,025 columns (three-save median): save mean **160.9 ms**, p95
**161.8 ms** (range 158.8–170.3 mean; 160.3–181.8 p95). Warm reload median
**572.3 ms** (range 560.7–590.1) for 2,025 columns. World directory 4.70 MiB
(`world.obs` 4.62 MiB; `block_entities.txt` 26.7 KiB, `items.txt` 53.0 KiB,
`mobs.txt` 767 B, `inventory.txt` 98 B, `equipment.txt` 150 B,
`player.txt` 80 B, `network_players.txt` empty). State hash and every entity
count matched after reload; tick count 16,100 was preserved.

`server_bench --quick` (radius 5, 300 ticks/phase): idle 0.0001 ms mean /
0.0003 p99, player 0.0008 / 0.0022, 18 mobs 0.0143 / 0.0164, 32 items 0.0106 /
0.0168, 8 furnaces + 4 chests 0.0062 / 0.0101; 121 resident columns; hash
round-trip PASS; **0.21 s** wall.

### Save and persistence (`save_bench`)

| Profile | Chunks | Heap | Save mean / p50 / p95 / max | Reload mean | World dir | Uncompressed / ratio | State hash |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | :---: |
| 289 columns, 256 items, 18 mobs, 32 furnaces, 16 chests | 289 | 21.59 MiB | 21.24 / 20.57 / 24.13 / 24.13 ms | 73.5 ms | 694.5 KiB | 21.63 MiB / 33.8× | PASS |
| 2,025 columns, 512 items, 64 mobs, 64 furnaces, 32 chests | 2,025 | 155.01 MiB (162.54 MB) | 155.97 / 156.32 / 157.60 / 157.60 ms | 539.6 ms | 4.70 MiB | 155.27 MiB / 33.6× | PASS |

The 289-column numbers are the median of three runs; the 2,025-column numbers
are the median of three runs with `--no-migration`. Save p50/p95/p99 are the
per-save wall clock. Sidecars for the 289-column profile: `block_entities.txt`
12.5 KiB, `items.txt` 25.9 KiB, `mobs.txt` 838 B, `inventory.txt` 99 B,
`equipment.txt` 150 B, `player.txt` 37 B.

Migration smoke (289-column world; synthesized v1/v2/v3 layouts plus the real
v4 snapshot, all loaded through the migration readers and compared by block
hash):

| Version | Bytes | Encode ms | Load ms | Chunks | Block hash |
| --- | ---: | ---: | ---: | ---: | :---: |
| v1 (uncompressed, dense light) | 54.29 MiB | 116.8 | 162.1 | 289 | PASS |
| v2 (uncompressed) | 21.63 MiB | 43.3 | 103.1 | 289 | PASS |
| v3 (uncompressed, fingerprint) | 21.63 MiB | 45.4 | 104.2 | 289 | PASS |
| v4 (real zstd snapshot) | 655.0 KiB | 25.1 | 106.3 | 289 | PASS |

`save_bench --quick` (radius 4, 81 columns, 32 items, 8 mobs, 8 furnaces,
4 chests): save 5.29 ms mean, reload 19.5 ms (release), world dir 196.6 KiB,
uncompressed 6.25 MiB (33.8×), v1–v4 migration PASS, **0.34 s** wall.

Persistence against the 1.0 budgets:

- Uncompressed serialize+compress throughput at 2,025 columns is
  **≈996 MiB/s** (155.27 MiB in 155.97 ms), above the 150 MiB/s
  PERF-IO-002 target. The compressed on-disk footprint is written at
  ≈29.6 MiB/s, which is the number the label "world directory per second"
  reports; the 150 MiB/s budget is interpreted on the uncompressed payload.
- Reload is 539.6 ms for 2,025 columns = 0.267 ms/chunk average, inside the
  1.5 ms/chunk PERF-IO-001 ceiling on average (p99 not instrumented).
- A fresh 2,025-column world is 4.70 MiB compressed, far below the 120 MiB
  PERF-IO-008 size budget; the 4,225-column 512-block-radius row was not run.
- **Save pause misses the 30 ms PERF-IO-004 impact budget at scale**: the
  current `Host::save` serializes and compresses the whole world
  synchronously, taking 156–160 ms at 2,025 columns (21–24 ms at 289
  columns). This is a real 1.0 gap, not a measurement artefact; the spec's
  incremental/region save and IO-thread architecture is not implemented.
- Process RSS is a `/proc/self/status` proxy and includes allocator retention;
  the host world heap is the comparable memory metric.

### Soak (`soak --ticks 60000`)

One 60,000-tick (50 game-minute) scripted survival session on a 6-chunk radius,
seed 42: circular flight, 1,200 break/place attempts on local surface columns,
periodic direct mob tops-ups and combat, meals, streaming drain, dirty-section
and pickup drains, a save every 10,000 ticks, and a final save/drop/reload.

| Metric | Value |
| --- | ---: |
| Wall time | 4.9 s (12,283 ticks/s = 614× real time) |
| Mining / building | 1,182 / 1,200 breaks changed; 1,104 / 1,170 places changed; 982 drops |
| Combat | 124 attacks, 124 hits, 0 kills (hand damage vs. mob health/despawn) |
| Mobs | 440 spawned, peak 15 of the 18 cap |
| Items | peak 27, 245 pickup events |
| Survival | 0 deaths, 0 respawns, 42 meals |
| Streaming | 16,636 columns generated, 16,592 unloaded |
| Dirty sections drained | 404,962 |
| Saves | 6; mean 15.92 ms, p95 21.08 ms, last 17.26 ms |
| Save size | 421.8 KiB → 548.8 KiB (peak 548.8 KiB; final 213 columns) |
| RSS | 3.4 MiB process baseline → 19.4 MiB after setup → 71.5 MiB end; plateau after 40k ticks |
| Save/reload | hash `b907560a3c528183` stable; 213 columns; 56.2 ms reload |
| Invariants | **PASS** (finite positions/vitals, caps, inventory validity, container/column bounds) |

The RSS series is not a leak proof: it rises as allocator arenas, rayon worker
buffers and save snapshots are touched, then plateaus. A true 2 h/24 h leak
gate needs the bot soak of spec/24 §3.13. `soak --quick` (4,000 ticks) runs in
**0.45 s** and passes the same invariants and hash round-trip.

### 1.0 performance envelope verdicts

| Budget / target | Measured | Verdict |
| --- | --- | :---: |
| 1.0 scope: worldgen ≤ 2.5 ms/chunk average | 0.921 ms/chunk (p99 1.147) | **PASS** |
| PERF-TICK-009: tick p99 ≤ 35 ms single-player baseline | idle/player/mobs/items/containers p99 0.0002–0.0332 ms | **PASS** for this baseline |
| PERF-MEM-013: server world ≤ 160 MB at 2,000 columns | 162.54 MB at 2,025 columns; ≈160.5 MB scaled to 2,000 | **MARGINAL MISS (~0.3%)** |
| PERF-IO-001: chunk load ≤ 1.5 ms p99 | 0.267 ms/chunk average at 2,025 columns | PASS on average; p99 not measured |
| PERF-IO-002: serialize+compress ≥ 150 MiB/s | ≈996 MiB/s uncompressed at 2,025 columns | **PASS** |
| PERF-IO-004: save snapshot ≤ 5 ms, tick impact ≤ 30 ms | 21–24 ms at 289 columns; 156–160 ms at 2,025 | **FAIL at scale**, pass at 289 |
| PERF-IO-008: fresh 512-block world ≤ 120 MiB | 4.70 MiB compressed at 2,025 columns (not 4,225) | PASS at measured size; full row not run |
| PERF-SRV-002/003: 50 co-located bots, 2,000 columns, p99 ≤ 35 ms, RSS ≤ 3 GB | not measured; single host player only | **NOT MEASURED** |
| PERF-TICK-002: 500 mobs ≤ 10 ms p99 | natural cap 18 mobs, p99 0.019–0.025 ms | **NOT MEASURED at 500** |
| Offscreen render gate | unchanged, separate tool | n/a here |

### 1.0 performance envelope summary and known gaps

The CPU simulation and persistence tools establish that single-player
generation, tick and uncompressed save throughput have large margins at the
2,025-column reference size; world heap is essentially at its limit. Four gaps
remain material for 1.0:

1. **Whole-world synchronous save.** `Host::save` at 2,025 columns takes
   156–160 ms, five times the 30 ms save-pause budget and three tick periods.
   The 289-column profile (21–24 ms) is the currently demonstrated safe size.
   The region file/IO-thread save design in spec/17 is the fix.
2. **Multiplayer capacity is unmeasured.** The 35 ms tick target is the
   50-player, 2,000-column reference-server case; this slice measures a
   single-player host. Raw protocol-v2 loopback bots (`DedicatedServer` + 2/4/8
   clients) and the 16/50-player matrix remain a follow-up owned by the
   server/networking slice.
3. **The natural mob cap is 18, not the 500-mob AI scene.** Entity AI/tick
   headroom at 500 mobs and the 200-entity network row are not demonstrated.
4. **World heap is marginally over the 160 MB decimal budget.** 2,025 columns
   use 162.54 MB (80,268 B/column), which scales to ≈160.5 MB at the budget's
   2,000 columns — about 0.3% over. It is effectively at the limit, not under
   it; the earlier "160 MB" line should not be quoted as a pass without the
   per-column basis.

Smaller gaps: the 4,225-column (512-block-radius) save/reload row, a real
2 h/24 h wall-clock soak, GPU/frame coverage (owned by the existing offscreen
render benchmark), and p99 per-chunk load instrumentation. The RSS proxy and
the 50-minute simulation soak are labelled as such above.

### CI

A new `bench` job in `.github/workflows/ci.yml` builds the release tools and
runs the four quick profiles on Ubuntu. Local wall times on the reference host:
worldgen 0.31 s, server 0.21 s, save 0.34 s, soak 0.45 s — **1.31 s total**,
far inside the 60 s guideline even allowing for a slower hosted runner. The
quick profiles assert determinism, invariants and the save/reload hash; they do
not assert wall-clock thresholds on a shared runner. The existing offscreen
render gate, save-size guard and OS matrix are unchanged.

## 2026-11-25 — 1.0 multiplayer load gate (8 players, 6-chunk interest)

### Environment and method

| | |
| --- | --- |
| OS | Fedora Linux 45 (kernel 7.3.4-300.fc45.x86_64), x86_64 |
| CPU / RAM | AMD Ryzen 7 9800X3D, 8 cores / 16 threads (16 logical CPUs available), 30 GiB |
| Build | release (thin LTO, `codegen-units = 1`), `CARGO_TARGET_DIR=/home/martintondel/Projects/openblock/target-1.0-load`, `./dev-cargo` |
| Server | `openblock-server` 0.1.0, protocol 2 (JSON/TCP), creative default, seed 42, fresh temporary data directory |
| Harness | [scripts/load-gate.py](../../scripts/load-gate.py), Python stdlib, loopback TCP only, no window/GPU |
| Run | one run per row; the machine was shared with other agents/builds (no CPU pinning, no `performance` governor) |

This is **not** the 4 vCPU / 8 GiB reference-server class named by the 1.0
performance envelope, and it is not a three-run median under the spec/25 §3.7
protocol. It is the first end-to-end measurement of the real dedicated server
with real protocol-v2 clients; treat it as the software-path gate, not a
published reference-hardware capacity claim.

**Gate definition.** 8 stock loopback clients (`scripts/load-gate.py`) join a
fresh `openblock-server` at a 6-chunk interest radius (169 columns each) in
creative mode, wait until every client has received its full interest set, then
run for 60 s with Move at 20/s per client, a place+break pair every 1.5 s, chat
every 3 s, ping every 1 s, and a missing-column chunk-request fallback only
after 3 s. The target is **server tick p99 <= 35 ms**, with functional checks:
8/8 handshakes and joins, every interest set complete, all chunk frames
received, accepted edits, pong samples, no client errors and no unexpected
disconnects. The server records each tick's `process_events + core.tick`
duration (the 20 Hz work, excluding sleep) in a rolling 1,200-tick window and
reports nearest-rank p50/p95/p99/max, cumulative overruns above 35 ms and 50 ms,
TPS, players, chunk frames/bytes, socket bytes, action counters, entity/column
counts and Linux `/proc` RSS/thread/fd proxies.

Commands:

```bash
CARGO_TARGET_DIR=/home/martintondel/Projects/openblock/target-1.0-load \
  ./dev-cargo build --release --locked -p ob_server --bin openblock-server

python3 scripts/load-gate.py --clients 8 --seconds 60 \
  --binary /home/martintondel/Projects/openblock/target-1.0-load/release/openblock-server \
  --json target/load-gate-full.json --markdown target/load-gate-full.md

python3 scripts/load-gate.py --quick \
  --binary /home/martintondel/Projects/openblock/target-1.0-load/release/openblock-server

python3 scripts/load-gate.py --soak \
  --binary /home/martintondel/Projects/openblock/target-1.0-load/release/openblock-server
```

### Server metrics implementation

`crates/server/src/net.rs` now owns a lock-free `Metrics` block: a 1,200-slot
`AtomicU64` tick-duration ring, cumulative overrun counters, chunk frames and
chunk bytes queued, socket bytes in/out, and received Move/edit/chat/ping/
chunk-request counters. The tick thread writes one `AtomicU64` and increments a
sequence per tick after releasing the core mutex; reader and writer threads add
frame bytes; `Core::send` counts chunk frames and `send_action_rejected` counts
action rejections. No lock is added to any network I/O path, and the snapshot
(`DedicatedServer::metrics`) takes the existing core mutex only for a few reads
and a sort of at most 1,200 integers.

`openblock-server --metrics-json PATH --metrics-interval SECONDS` (listener
mode only) writes `openblock.server-metrics/1` JSON atomically (temporary file
+ rename) immediately after binding and then every interval; the first snapshot
also advertises the ephemeral `listen` address. Write failures are logged and
never stop the tick. The JSON includes tick percentiles and overruns, players,
TPS, chunk/bandwidth counters, accepted/rejected edits and actions, moves,
chats, pings, chunk requests, mob/item/column counts and RSS/thread/fd proxies.

**Instrumentation cost.** `server_bench --quick` (the `Host::tick` path, which
does not run the network tick loop) before → after the change: idle p99 0.0004
→ 0.0003 ms, player 0.0022 → 0.0021 ms, mobs 0.0184 → 0.0183 ms, items 0.0140 →
0.0185 ms, containers 0.0101 → 0.0102 ms. The differences are timer noise; the
only change on the network path is one relaxed atomic store and two compare/
increment operations per tick, outside `Host::tick`.

### Full 8-player / 60 s gate — PASS on this host

| Metric | Value |
| --- | ---: |
| Handshakes / joins | 8 / 8 |
| Interest sets complete | 8/8, p50/p95 2.152 s, max 2.152 s |
| Interest set per client | 169 columns (13x13) |
| Unique chunk frames received by clients | 1,352 (169.0/client) |
| Server chunk frames queued | 1,352 (21.12/s over the 64.0 s rate window) |
| Server bytes out / in | 225.1 KiB/s / 16.9 KiB/s (28.1 KiB/s down per client) |
| Tick p50 / p95 / p99 / max | **0.061 / 0.124 / 0.163 / 13.574 ms** |
| Tick overruns > 35 ms / > 50 ms | 0 / 0 |
| TPS (tick delta / measured window) | 19.99 |
| Server RSS / peak | 22.3 MiB / 22.3 MiB |
| Loaded columns at end | 169 |
| Server startup / shutdown drain | 0.05 s / 0.12 s |
| Moves / chats / pings / chunk requests received | 9,800 / 168 / 496 / 0 |
| Block edits accepted / attempted / rejected | 656 / 656 / 0 |
| Action rejections / client errors / unexpected disconnects | 0 / 0 / 0 |
| Client ping p50 / p95 / p99 | 25.38 / 50.63 / 50.71 ms |
| Client chat delivery p50 / p95 | 25.37 / 45.65 ms |

**Verdict: PASS against the 8-player / 6-chunk / 35 ms p99 target on this
host** (p99 0.163 ms, about 215x inside the target; 0 overruns). The final
1,200-tick window starts after the 2.15 s interest-set streaming, so the quoted
p99 is steady-state; the cumulative counters retain the cold phase and reported
0 ticks above 35 ms and 0 above 50 ms for this run. A separate 15 s 8-client
run, whose window still contained the cold phase, measured p99 28.2 ms,
max 58.3 ms and one tick above 50 ms while the 169 columns were generated. It
also passed the 35 ms p99 target, but the cold tail is why max/overruns are
reported beside p99 rather than hidden.

### Scaling (8 bots vs 2/4, 30 s each)

Each row is a fresh server and world, same seed, creative, radius 6, 30 s of
load after all interest sets were complete.

| Clients | Tick p50 | p95 | p99 | Max | TPS | RSS | Chunks/s | Out total | Down/client | Accepted edits |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 0.028 | 0.400 | 2.425 | 2.559 | 19.99 | 20.8 MiB | 9.93 | 53.6 KiB/s | 26.8 KiB/s | 84 |
| 4 | 0.037 | 0.721 | 4.841 | 6.367 | 19.98 | 20.9 MiB | 19.87 | 117.7 KiB/s | 29.4 KiB/s | 168 |
| 8 | 0.060 | 1.325 | 1.982 | 9.981 | 19.99 | 21.1 MiB | 39.75 | 276.9 KiB/s | 34.6 KiB/s | 336 |

The p99 is not monotonic in client count because these are single runs on a
shared host and streaming/GC noise lands differently; all rows are well inside
35 ms. RSS includes the fixed world and Python-independent server only; the
8-client row is 21 MiB, far below the `2 GiB + 20 MiB/player` budget.

### Quick CI profile (4 bots / 12 s, tick target advisory)

Fresh server, seed 42, creative, radius 6: 4/4 joins, 4/4 interest sets in
2.15 s, 676 chunks, 72 accepted edits, 0 rejections/errors/disconnects;
tick p50/p95/p99/max 0.034 / 0.915 / 5.418 / 6.640 ms, 19.98 TPS, 0 overruns,
20.9 MiB RSS, 42.21 chunks/s, 178.4 KiB/s out (44.6 KiB/s per client). Run
time was about 14 s including startup, 12 s load and shutdown. The CI job runs
this profile with the target advisory; functional checks still fail the job.

### Soak (10 min, 8 bots, 10 s autosave)

One `--soak` run: 8 bots, radius 6, creative, seed 42, 600 s of load after the
interest sets were complete, autosave every 10 s. The server ran 12,115 ticks
(20.00 TPS) and wrote 60 autosaves.

| Metric | Value |
| --- | ---: |
| Tick p50 / p95 / p99 / max | 0.056 / 0.100 / 0.146 / 11.287 ms |
| Ticks > 35 ms / > 50 ms | 1 / 1 |
| RSS at 5 s / 60 s / end | 21.0 / 47.7 / 49.8 MiB |
| RSS growth after 120 s warm-up | +1.9 MiB (last-half range 1.1 MiB) |
| Open FDs (first half -> end) | 29 -> 29 |
| Server threads (first half -> end) | 37 -> 37 |
| Autosaves | 60 (>= 59 expected) |
| World file after stop | 390,416 B |
| Save/reload state hash | `1b4459542eb792df`, stable across two reloads |
| Block edits accepted / rejected | 6,432 / 0 |
| Moves / chats / pings received | 95,472 / 1,616 / 4,832 |
| Client errors / unexpected disconnects | 0 / 0 |
| Server bytes out / in | 173.8 KiB/s / 17.3 KiB/s (21.7 KiB/s down per client) |

The memory series rose from 21 MiB to ~49 MiB while the first autosaves and
allocator arenas were touched, then plateaued: after a 120 s warm-up it grew
only +1.9 MiB over the remaining 8 minutes, with a 1.1 MiB range in the last
half. FDs and threads were flat at 29/37 and the save/reload hash was stable.
The soak passes the memory-plateau, leak, autosave and hash checks.

One tick exceeded both the 35 ms and 50 ms thresholds: the server logged
`tick overran the 50 ms budget: 85.825545ms` immediately after the autosave at
tick 602. The p99 stayed at 0.146 ms, but this is a real zero-overrun gap:
`PERF-TICK-010` asks for zero ticks above 50 ms in an hour, and one in 10
minutes is a poor extrapolation. The likely contributor is the synchronous
whole-world `Host::save` inside the tick (`PERF-IO-004` is already a known 1.0
gap); the run is too short to prove a rate.

### CI and scripts

`.github/workflows/ci.yml` has a new `load-gate` job that builds the release
server and runs the quick profile on Ubuntu, then uploads the JSON/Markdown
report. `scripts/load-gate.py` is `py_compile`d in the server-deps job together
with `scripts/load-test.py`. The existing `bench` job is unchanged, so the four
Rust quick benchmarks still run as before.

### Known gaps and caveats

- **Hardware:** 8-core/16-thread workstation, not the 4 vCPU reference server;
  one run, no governor pinning, shared machine, no three-run median.
- **Scale:** 8 co-located players sharing a 169-column interest set, not the
  16/50-player 256-block or 2,000-column PERF-SRV rows. The 50-player matrix
  is still unmeasured.
- **Cold tail:** chunk generation runs inside the network tick; a 15 s run saw
  a 58.3 ms maximum and one >50 ms tick while streaming, although the 60 s p99
  was 0.163 ms. Streaming is fast (2.15 s for 169 columns/client) but not free
  at the join instant.
- **Autosave overrun:** the 10-minute soak recorded one 85.8 ms tick directly
  after an autosave. The synchronous whole-world save is still inside the tick,
  so a zero-overrun-per-hour claim is not yet supported even though p99 passes.
- **Creative gate:** edits are deterministic creative place/break; survival
  item, hunger and death paths are exercised by the scripted `soak` tool, not
  by these bots. The bots consume but do not verify chunk payloads or world
  state, so this is not a protocol conformance test.
- **Loopback:** no packet loss, RTT or bandwidth policing; client ping/chat
  latency includes Python scheduling and the 20 Hz tick, so it is end-to-end
  application latency, not network RTT.
- **RSS proxy:** `/proc/self/status` VmRSS includes allocator retention and the
  whole process, not only the host world heap; the soak plateau check is a
  coarse leak detector, not a 24 h proof.
- **No network-side perf regression gate:** the tick loop's new atomic writes
  were measured only indirectly; a CI regression threshold for the load gate
  needs a quieter runner first.

## 2026-11-25 — 1.0.0 release verification

- 1.0 acceptance scenario (`crates/server/tests/acceptance.rs`): 8/8 tests in
  ~12.5 s, deterministic seed 42, full single-player survival loop + raw
  protocol-v2 multiplayer loop; spawn safety now persists in an additive
  `spawn.txt` sidecar and is covered by the state hash.
- Headless smoke (seed 42, save format 4): `1dba6105028ba6e5` across fresh runs
  (the previous `33875a6597c2ce7b` predates the spawn sidecar/hash change).
  1.0.1 completes the state hash over persisted yaw/pitch/flying and canonical
  angles, so the current seed-42 hash is `eba0c42219691190` (still deterministic
  across fresh runs and save/reload).
- Multiplayer load gate (8 clients / 60 s): tick p50/p95/p99
  0.061/0.124/0.163 ms, 20.0 TPS, 0 overruns >35 ms, RSS 22.3 MiB; 10-minute
  soak plateaued with stable hash and no fd/thread leaks. Known gaps:
  synchronous save spikes, reference-hardware capacity, 16/50-player rows.
- UI workload at 1280x720: 243–1335 quads and ≤3 texture draw calls per screen;
  full 960x540 audit passes.

## 2026-11-25 — 1.0.1 performance/reliability audit (v1.0.0 re-measurement)

Post-release audit of the **v1.0.0** tag (`19e489e`) from a pristine worktree,
plus extended reliability and CI checks for the 1.0.1 line. Full detail and the
save-pause design proposal are in
[the 1.0.1 performance audit](1.0.1-performance.md). The machine was shared
with other agents/builds; every row below is a repeat median or a stated range,
not a quiet-box claim.

### Baseline re-measurement

| Metric | 1.0.0 published | 1.0.1 audit | Verdict |
| --- | ---: | ---: | :---: |
| `worldgen_bench --full` mean | 0.921 ms/chunk | **0.920** (0.918–0.922; p99 1.114–1.123) | PASS |
| `worldgen_bench --quick` mean, 3 runs | 0.940 | **0.947** (0.936–0.952) | PASS |
| `server_bench` player/mobs/items/containers p99 | 0.0091 / 0.0190 / 0.0261 / 0.0242 ms | **0.0090 / 0.0191 / 0.0245 / 0.0257** | PASS |
| 289-column save / reload | 21.24 / 73.5 ms | **21.39 / 72.20 ms** | PASS |
| 2,025-column save / reload | 155.97 / 539.6 ms | **154.48 / 542.03 ms** | PASS |
| Host world heap at 2,025 columns | 162.54 MB | **162.54 MB** | marginal PERF-MEM-013 miss |
| `soak --ticks 60000` save mean | 15.92 ms | **16.42 ms** (16.36–16.60) | PASS |
| 60k soak hash | `b907560a3c528183` | **`c828635abf0645df`** (3/3) | deterministic; old value stale |
| 60k soak RSS end | 71.5 MiB | **88.1 MiB** (84.4–100.5) | allocator spread, plateau |
| load gate quick p99 | 5.418 ms | **5.05 ms** (4.94–6.47 across 6 runs) | PASS |
| load gate 8×60 p99 / overruns | 0.163 ms / 0 | **0.892 ms** (0.225–1.288) / 0,0,1 | PASS; cold tail |

The four quick tools, quick load gate and backup/restore test ran repeatedly
with no functional flake. The 8-client full gate's higher p99/max is consistent
with the shared host and with the join/streaming tail already recorded for
1.0.0 (one run's 26.3 ms maximum and one cumulative >50 ms tick came from the
cold phase; the steady-state p99 stayed ≤1.3 ms).

### Save pause (PERF-IO-004)

Pristine v1.0.0 decomposition at 2,025 columns: `Host::save` = 152 ms on
tmpfs and 158 ms on btrfs (one 168 ms btrfs outlier); 57–62% zstd-3
compression, 37–39% serialization/allocation/CRC, ~1% write+rename and ~1.5%
fsync probes. An IO thread alone cannot meet the 30 ms budget.

End-to-end autosave impact, measured window started after interest sets:

| Server load | Autosave | Tick p99 | Max | Overruns | RSS end |
| --- | ---: | ---: | ---: | ---: | ---: |
| 8 clients, 169 cols | off | 0.154 ms | 0.259 ms | 0 | 21.3 MiB |
| 8 clients, 169 cols | 1 Hz | **11.066 ms** | 20.717 ms | 0 (>50 ms) | 87.1 MiB |
| 1 client, 1,089 cols | off | 0.203 ms | 1.729 ms | 0 | 92.9 MiB |
| 1 client, 1,089 cols | 1 Hz | **85.007 ms** | 151.289 ms | 44 | 125.3 MiB |

At 1,089 columns all 45 logged overrun warnings landed on the line immediately
after an autosave; the 1 Hz run correctly fails the 35 ms p99 target. At the
2,025-column reference size the network server cannot load enough columns
(interest radius is capped at 16), so the 152–158 ms `save_bench` value is the
applicable one. `--save-interval 0` disables the timer but not the pause: the
console `save` and shutdown save call the same synchronous path.

### Reliability and CI (summary)

The new `scripts/reliability-audit.py` ran 20 network start/stop/reload
cycles (20/20 PASS, world dir 399.8–400.6 KiB), 60 s of 8-client
connect/disconnect churn (29 cycles, 0 client errors, FDs 29→29, threads
37→37), a 300 s persistent-bot soak (RSS 7.9→49.3 MiB, plateau by 40 s, +1.6
MiB over the last 240 s; 29 autosaves, no overrun warnings, hash stable),
truncated `mobs.txt`/`spawn.txt` and `world.obs`, SIGTERM under save (10/10 at
radius 6; 10/10 with 9 autosaves in a focused radius-16 run), a console
save+stop race (5/5) and read-only / file-size-limit disk proxies (previous
snapshot preserved and reloadable). The notable findings are:

- **Churn memory growth:** with 8 fresh clients every ~2 s, RSS grew
  21→88 MiB in 60 s and 21→167 MiB in 180 s while FDs/threads stayed flat;
  `MALLOC_ARENA_MAX=2` bounded it at 60.8 MiB (plateau by ~44 s), pointing at
  glibc per-thread arena retention rather than an object leak.
- **`--ticks 0` is not read-only:** `Host::open` streams the two-column ring
  and the headless path saves it, so repeated opens of the same directory grow
  the resident set and change the hash. The harness compares two independent
  copies instead.
- **No last-good fallback:** a truncated `world.obs` is refused with exit 1
  (good: no silent partial load), but there is no previous-generation recovery.
  Truncated sidecars degrade to warnings and missing records.

A clean 10-minute 8-bot load-gate soak (10 s autosave) passed with tick
p50/p95/p99/max 0.058/0.103/0.186/11.928 ms, 20.00 TPS, **zero** overruns
above 35/50 ms, 60 autosaves, RSS 49.8 MiB (+2.1 MiB after a 120 s warm-up,
1.1 MiB last-half range), flat FDs 29/threads 37 and a stable
`9ab9070a666601db` hash across two reloads. The published 1.0 soak's single
85.8 ms tick did not reproduce on this quiet run, but the 1 Hz 1,089-column
diagnostic still produces 44 overruns, so this is not a zero-overrun/hour
claim.

CI now uploads benchmark and load-gate artifacts, has per-job timeouts, runs
`save_bench --quick --phases`, uses `--wait-interest` in the quick gate, and
has a manual `reliability` job (20 cycles, 60 s churn, 300 s soak). No
repository-setting or GitHub-permission change is required beyond the
read-only `contents` token already added during the 1.0.1 audit.

## 2026-11-25 — 1.1 worldgen slice (deterministic surface structures)

### Environment and method

Same host and profile as the cave and ore entries above: Fedora Linux 45,
AMD Ryzen 7 9800X3D, test profile (`[profile.dev] opt-level = 1`,
dependencies at `opt-level = 3`), CPU-only, no window. All runs used
`CARGO_TARGET_DIR=/home/martintondel/Projects/openblock/target-structures`.
"Without structures" is the same binary with the private structure pass
disabled, so the added-cost column isolates the new pass rather than comparing
two source revisions. Chunk hashes are FNV-1a over every finished block.

### What ships

| Kind | Footprint | Recognisable content | Biome variant |
| --- | --- | --- | --- |
| Ruined tower (`ruined_tower`) | 5×5 footprint, 5–11 blocks tall | Cobblestone plinth and broken walls, planks bands, hashed door and up to three windows, plank lintel, 2×2 planks rug, crafting table and two benches, three-wide porch, snow caps on some wall tops | Snow caps in snowy taiga; desert uses the same masonry |
| Abandoned camp (`abandoned_camp`) | 7×7 clearing, 3×3 lean-to | Plank lean-to with solid back/side walls and roof, four corner posts, coal-ore fire pit ringed by four gravel/cobble blocks, crafting table, planks crate and bench | Planks floor in temperate/taiga (snow floor in taiga) plus a snow windbreak; sand floor and gravel fire ring in desert |
| Stone circle (`stone_circle`) | 7×7 | Eight cobblestone monoliths of one to three blocks at radius three, 3×3 central platform, four-block plinth capped by coal ore, crafting table and planks offering bench | Sand ring bases in desert; snow bases and snow caps in taiga |
| Mineshaft adit (`mineshaft`) | 9×5, four orientations | Four-step open staircase lined with cobble retaining walls, plank headframe, four-block timbered tunnel with two frames, cobble floor/walls and plank roof; end face exposes selected coal, iron and copper blocks in stone, or opens into a cave if the carvers left air there | None |

Palette: cobblestone, planks, crafting table, coal/iron/copper ore, sand,
gravel and snow. No new block ids or textures, and deliberately no logs,
leaves, chests or furnaces.

Rarity comes from region rejection, not a second density knob. In 96×96 chunks
(9,216) the observed densities were:

| Seed | Structures | Overall | Tower | Camp | Circle | Mineshaft |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 87 | 1 per 106 | 1 per 318 | 1 per 297 | 1 per 485 | 1 per 1,152 |
| 42 | 111 | 1 per 83 | 1 per 297 | 1 per 243 | 1 per 384 | 1 per 512 |
| `0xc0ffee0012345678` | 79 | 1 per 117 | 1 per 318 | 1 per 512 | 1 per 401 | 1 per 1,024 |

The exact structural delta (chunks generated with the structure pass vs the
same chunks without it; features and ores disabled in both) was 127–215 blocks
for a tower (mean 155–167), 31–94 for a camp (mean 44–46), 27–76 for a circle
(mean 42–43) and 144–176 for a mineshaft (mean 159–166). A structured chunk's
heap grew by at most **10,254 B** when a section palette stepped up a width;
there are no new columns, sections, sidecars or other persistent state. Averaged
over the observed density this is roughly 90–125 B per generated chunk.

### Placement and determinism

- An 8×8-chunk region (128×128 blocks) derives one 64-bit hash from the world
  seed and region coordinate (`derive_seed(seed, region_x, region_z, "STRU")`).
  A counter-based `Rng` from that hash picks, in order: the candidate chunk
  (0..7 on each axis), the weighted kind (tower 3, camp 3, circle 2, mineshaft 2
  of 10) and the local anchor (4..=11 on each axis).
- A chunk that is not its region's candidate does no height-field work and
  returns no plan. The candidate is accepted only if every footprint column is
  dry (`height >= sea + 3`, i.e. at least 65), the footprint's maximum minus
  minimum terrain height is at most 4, no column lies in the 5×5-chunk
  spawn-protection square, and no ravine candidate from the surrounding 3×3
  ravine regions conservatively intersects the chunk. A failed site produces no
  structure for that region; there is no retry or fallback, which keeps the
  decision seed-local.
- The anchor range plus the largest half-extent (4) keeps every write inside
  the owning chunk (`4 - 4 = 0`, `11 + 4 = 15`). Structures therefore never need
  neighbour reads, are independent of chunk generation order, and cannot share a
  block with another structure or straddle a chunk boundary.
- `floor_y` is the highest footprint column. Towers get a solid 5×5 cobblestone
  foundation, a porch and a cleared volume above the floor. The camp and circle
  fill only the columns under floors, props and ring stones via
  `place_supported`, leaving the rest of the clearing natural. The mineshaft
  carves its four stair steps and tunnel below `floor_y`, fills retaining walls
  and floors down to terrain, and lays a plank roof carried by the side walls.
  `set` and `clear` both refuse to replace water or bedrock as a final safety
  net.
- Ordering is: terrain → caves → surface entrances → ravines → **structures** →
  trees → boulders → snow drifts → ores. Trees and boulders require natural
  grass/snow and air, so they skip structure floors and cannot grow through a
  roof; structures place no `stone`, so the later ore pass cannot speckle their
  walls (the mineshaft's natural end face is the one intentional ore exposure,
  and the ore pass can also seed that face). Clearing happens only above the
  floor, never below it, and never touches water or bedrock.
- The plan is a pure function of seed and chunk: `Generator::structure_in_chunk`
  returns `Option<StructureInstance>` with kind, anchor, floor, biome and region
  hash. It is the interim hook for tests, diagnostics and a future `/locate`;
  nothing is persisted.

### Throughput

One representative 200-chunk run per seed, two interleaved rounds, test profile:

| Seed | With structures (ms/chunk) | Without (ms/chunk) | Added |
| --- | ---: | ---: | ---: |
| `0x1` | 2.030 | 2.043 | **−0.013** |
| `0x2a` (42) | 1.982 | 1.984 | **−0.002** |
| `0xc0ffee0012345678` | 1.963 | 1.958 | **+0.004** |

Across three repeat runs (nine seed/round pairs) the added cost ranged from
−0.020 to +0.015 ms/chunk; the worst observed added cost is **+0.015**, far
inside the ≤ 0.2 ms/chunk target. Total generation stays around 1.94–2.06
ms/chunk, matching the cave/ore entries. The non-candidate path is a region
hash and two RNG draws; only one chunk in 64 performs the footprint height
probe, and only one in roughly 85–117 chunks writes 27–215 blocks, so the cost
is amortised to noise.

### Determinism and hashes

`hash(0,0)` is unchanged from the 1.0.2 entries for all three seeds because the
5×5 spawn-protection square never receives a structure:

| Seed | `hash(0,0)` |
| --- | --- |
| 1 | `0x25d12c39c22015fd` |
| 42 | `0x6716df9f5cf776b2` |
| `0xc0ffee0012345678` | `0x0169a0c9bbc20e12` |

Representative structured chunks change as expected (full chunk hash with
structures vs the same chunk generated without structures; seed 42):

| Kind | Anchor | Full hash | No-structure hash |
| --- | --- | --- | --- |
| ruined tower | (−327, −697) | `0x3b789e47cceb0017` | `0x60851f81c8461ea3` |
| abandoned camp | (694, −696) | `0xe10b2ef30fe3e9a0` | `0x29c6fe6a98d92bff` |
| stone circle | (−71, −631) | `0x73ad87ac8c47c633` | `0xdf1ce50df5197556` |
| mineshaft | (484, −746) | `0x70c8f00e94d321cf` | `0x4f4a3c3a64feb5df` |

Same-seed regeneration, reverse-order generation with unrelated neighbours
generated between samples, and repeated `structure_in_chunk` calls all produce
identical plans and blocks. The structure RNG shares no state with terrain,
caves, ravines, trees or ores.

### Tests

`cargo test -p ob_worldgen --lib`: **30 passed, 0 failed, 7 ignored**. New
non-ignored tests:

- `structures_are_deterministic_and_order_independent` — plan and chunk hashes
  in forward/reverse order with intervening neighbours and regeneration.
- `structures_use_only_existing_blocks_and_are_supported` — one of each kind;
  exact structural delta against a terrain-only chunk, palette, water/bedrock
  protection, 6-neighbour support from natural ground, no floating water.
- `sampled_structure_layouts_are_supported` — the same checks over every
  structure in a 48×48-chunk window for seeds 1 and 42 (all orientations and
  biome variants).
- `structures_avoid_spawn_protection` — no plans in the 5×5 spawn square for
  three seeds, and spawn-square chunks are block-identical with and without the
  structure pass.
- `structure_sites_are_dry_flat_and_ravine_free` — site contract over a
  64×64-chunk window.
- `structures_never_overlap_and_stay_in_their_chunk` — one plan per chunk,
  conservative bounds inside the owning chunk, pairwise-disjoint structure
  boxes.
- `structure_types_appear_at_a_bounded_density` — all four kinds present in
  96×96 chunks for three seeds, density between one per 512 and one per 16
  chunks.

New ignored reports: `structure_throughput_report` (the table above, asserts
≤ 0.2 ms/chunk) and `structure_distribution_report` (counts, per-kind density,
block deltas, heap delta, representative hashes). Existing terrain tests now
generate without structures where they validate natural surfaces, snow, cave
reports, tree density and ore streams; the ore determinism test is confined to
spawn-protected chunks so the ore stream is still isolated. `cargo fmt
-p ob_worldgen --check` and `cargo clippy -p ob_worldgen --all-targets -- -D
warnings` are clean.

### Deviations, limits and integration needed

- **No chests or furnaces.** The host creates `ContainerState`/`FurnaceState`
  only when a player or network edit places the block
  (`Host::register_block_entity`); worldgen output is not scanned, and
  `block_entities::load_block_entities` only loads records from the sidecar and
  prunes missing blocks. A worldgen chest therefore has no record:
  `Host::chest_at` returns `None`, the network server's `handle_use_block`
  rejects it as `not_container`, and slot operations would return
  `container_missing`. Loot is consequently absent. A future loot pass needs
  the server to register natural block entities as a column is streamed/loaded
  (or consume a worldgen block-entity descriptor), persist them in
  `block_entities.txt`, and generate contents on first open from a loot table
  and seed. This slice deliberately ships only crafting tables (usable through
  `near_crafting_table` without a block entity) and real, mineable ore blocks.
- **No `/locate` or persistence.** Starts are recomputed from the seed; there is
  no structure sidecar, no `structure_set`, no jigsaw, no `structure_void`, no
  mob-spawn override and no 3×3 separation pass. Spacing/separation/salt/spread
  and persisted starts remain spec/06 §3.8 work.
- **Whole-chunk structures.** Footprints are deliberately small (5×5, 7×7,
  9×5) and anchor-margined so no cross-chunk writes are needed. This is simpler
  and stronger than overlap-and-clip for 1.1, but it cannot express multi-chunk
  villages or dungeons.
- **Terrain interaction.** Structures are rejected in ravine chunks but do not
  test cave air. A foundation can bridge or seal a rare 1×1 cave entrance, and a
  mineshaft end face intentionally opens into a cave when the carvers left air
  there. Trees and ores are placed after structures, so the ordering is
  deterministic; no floating blocks or water are created in the sampled corpus.
- **No underground dungeon.** A buried room was considered and deferred: a room
  without usable chests/furnaces has little reward, and a stair shaft would
  compete with the mineshaft, which already provides the underground payoff.
- **Spawn search is not structure-aware.** Worldgen excludes structures from the
  5×5 spawn-protection chunks, and the host's existing solid/dry/clear-column
  checks reject structure walls and floors, but an open camp clearing just
  outside that square could still pass as a spawn column. If strict "outside
  every structure bounding box" spawn exclusion is wanted before the data-driven
  spawn search lands, `Host::resolve_spawn` should consult
  `Generator::structure_in_chunk`.
- **Scale.** Plinths on sloped sites can be up to four blocks high. Structures
  are landmarks, not set-pieces; they use the existing base block set and add
  no textures, items or recipes.

### Proposed spec/06 wording (for the Lead)

> **1.1 interim structure layer.** Until the data-driven `structure_set`
> pipeline lands, the generator MAY place engine-defined, chunk-local
> structures. One candidate is derived per 8×8-chunk region from a region hash;
> the hash selects the candidate chunk, weighted kind and local anchor, and the
> whole footprint must remain inside that chunk. A candidate MUST be rejected on
> wet ground (surface below sea + 3), on a footprint with more than four blocks
> of relief, inside the spawn-protection square, or in a chunk touched by a
> ravine candidate. Structures run after carvers and before features and ores,
> MUST use only existing block ids, MUST NOT replace water or bedrock, and MUST
> be supported by terrain or their own connected foundation. The interim API is
> `Generator::structure_in_chunk(ChunkPos) -> Option<StructureInstance>`; starts
> are not persisted. The full structure/template/sidecar/loot requirements of
> §3.8 remain the target for MS-08 and later.

## 2026-11-25 — v1.1 distance-LOD / 512-block render distance

### Environment and methodology

| | |
| --- | --- |
| OS | Fedora Linux 45, kernel 7.3.4 |
| CPU/GPU | AMD Ryzen 7 9800X3D, integrated RADV "RAPHAEL_MENDOCINO" (Vulkan) |
| Renderer | wgpu 30.0.2, offscreen `Rgba8UnormSrgb` + `Depth32Float`, 1280×720 (also 1920×1080 run) |
| Build | `./dev-cargo build --release -p ob_tools --bin render_distance_bench` |
| World | seed 42, 65×65 = 4,225 columns, 101,400 sections, 322.5 MiB world heap |
| Camera | spawn column, eye +18 blocks, spawn yaw, pitch −0.10 rad; far 560 blocks |
| Scene | near ≤10 chunks as 16³ sections; far 2×2-chunk surface-shell batches (top 48 blocks) |
| Frames | 10 warm-up + 120 measured; every frame includes GPU→CPU readback (no timestamp queries) |
| Command | `render-distance-bench --render-distance 32 --frames 120 --warmup 10 --png /tmp/openblock-512.png` |

The new `crates/tools/src/bin/render_distance_bench.rs` opens a real
`ob_server::Host`, drains streaming, builds the scene with rayon (matching the
client's worker pool), uploads it to `OffscreenRenderer`, and times cached
frames. The measured frame time therefore includes CPU submission,
rasterisation and the readback that forces GPU completion. It excludes world
generation, meshing, buffer upload and window presentation. Absolute numbers
are specific to the integrated RADV GPU; the comparison between full detail and
LOD uses the same world, camera and buffer path.

### Before / after at the same 512-block radius

| Metric | Full detail (pre-LOD path) | Distance LOD (v1.1) |
| --- | ---: | ---: |
| Cached geometry | 35,661 near sections | 2,620 near sections + 1,010 far batches |
| Cached vertices | 47,166,642 | 22,772,640 |
| Mesh + upload (parallel) | 656 ms | 326 ms |
| Last-frame draws | 8,917 | **716** (433 sections + 283 batches) |
| Last-frame submitted vertices | 12,069,534 | 6,346,092 |
| Frame p50 / p95 / max | 14.08 / 15.76 / 16.29 ms | **5.22 / 5.34 / 5.42 ms** |
| Mean FPS | 69.4 | **191.0** |
| Vertex-data estimate (CPU+GPU) | 2,519 MiB | 1,216 MiB |
| Process RSS after build / peak | 491 / 492 MiB | 454 / 457 MiB |
| Shell sections retained / skipped | 0 / 0 | 15,337 / 78,479 |

Streaming is identical in both modes: 4,225 columns in 478–501 ms (66 calls,
4,220 generated by the drain; the spawn column arrived at start). The pre-change
binary (full detail, sequential cache build, partial 4,000-column world) measured
33,741 sections, 15.0 M triangles, 4.27 s mesh and p50 14.0 ms; it is not a
like-for-like row because the world was partial, but it shows the old cached
path was already near the 60 FPS line with 4,225 columns.

At 1920×1080 the same LOD scene measured 716 draws and p50 7.94 ms
(p95 8.23 ms, p99 8.84 ms, 127.0 FPS); capture at
`/tmp/openblock-512-1080p.png`. The resolution scaling is raster-bound, not
draw-call-bound, which is the expected shape for 716 small draws.

### Streaming and memory

- The client now raises the generation budget while the near ring is missing
  (64 columns/frame during the loading overlay, 32 while >1,024 columns are
  missing, 16 above 256, 8 otherwise). Generation stays rayon-parallel inside
  `Host::ensure_streaming`.
- The ignored server probe
  `streaming_unload_plateau_at_512_blocks` generated 4,220 columns in 467.7 ms
  on 16 threads (314 MiB heap), reported a zero-growth plateau on the next call,
  then unloaded 3,936 columns when the radius dropped to 6, leaving exactly the
  17×17 ring and a second zero-growth unload plateau.
- Far batches are built from a cloned group + one-chunk border on the shared
  mesh worker pool, never from live world memory. At most four batches are in
  flight and one is uploaded per frame. A batch whose group leaves the far tier
  or changes loaded membership is dropped and rebuilt; an edit inside a far
  group invalidates the batch rather than meshing a full section.
- The save caps were raised to 512 MiB uncompressed / 1 GiB on disk: a full
  65×65 world serialised to 338 MiB, so the previous 256 MiB cap rejected every
  autosave at the 512-block radius. The 2 s flush gate remains scoped to 256 MiB.

### Multiplayer loopback join (4,225 columns)

Ignored probe `join_512_blocks_loopback_transfer` (release):
`DedicatedServer` with interest radius 32 and 16 columns/tick, real TCP loopback,
`NetworkClient` requesting 16/frame up to its 128/s token bucket. Host world was
pre-generated so the number measures protocol transfer plus client request
pacing; server generation was separately measured above.

| Metric | Value |
| --- | ---: |
| Join completion | **4,225 / 4,225 columns in 12.61 s** |
| Server bytes out | 13.23 MiB (3.2 KiB/column compressed) |
| Mean bandwidth | 8.81 Mbit/s |
| Server tick p95 / p99 / max | 1.28 / 1.46 / 2.78 ms |
| Server save on stop | succeeded after the cap raise |

The client requests 128 chunk columns/s (burst 64); the server tick budget can
supply 16/tick. Client-side eviction keeps the replicated world inside the
interest ring plus two chunks and releases section meshes and far batches when
columns leave.

### Interpretation and remaining limits

- The 60 FPS target is met with a wide margin at 1280×720 on the integrated GPU
  (191 FPS measured; the readback inflates measured frame time, so GPU-only time
  is lower). The 716-draw scene is within the 1,000-draw interim target even
  before any GPU-driven batching.
- The far shell deliberately drops everything more than 48 blocks below a
  column's highest block. Caves, mines and deep underground geometry are absent
  beyond 10 chunks; this is invisible from normal terrain-level views but can be
  seen at extreme grazing angles at a tier boundary. No cross-fade is applied;
  fog is the only blend.
- Far batches are opaque draws. Water in the shell stays in the normal material,
  but the future translucent pass must keep far water out of the opaque batch.
- Standard `Depth32Float` (0..1, no reverse-Z) is retained: at 512 m with the
  0.05 m near plane the depth quantum is about 0.31 m, below the sub-pixel size
  of far terrain. No z-fighting was observed in the captures. A reverse-Z change
  is not justified at a 560-block far plane.
- The dedicated server still retains the union of every player's explored
  columns; client eviction does not free server memory. A save-safe server
  unload policy remains open.
- No per-frame GPU timestamp queries; CPU submission, GPU and readback are one
  blended number. Windowed numbers (vsync/compositor) were not measured in this
  pass.
- The settings screen offers 2–32 chunks and displays both values
  (`32 chunks / 512 blocks`). F3 reports loaded chunks, full/far sections, draw
  calls, triangles, LOD batches, shell-skipped sections, mesh backlog, worker
  threads and a CPU+GPU mesh-memory estimate.
- The render-distance ceiling is now one shared contract: core
  `MAX_RENDER_DISTANCE = 32`, client settings `SETTINGS_MAX_RENDER_DISTANCE = 32`,
  client network `MAX_RENDER_RADIUS = 32`, server
  `MAX_INTEREST_RADIUS_CHUNKS = 32`, CLI 2–32. The minimum remains 2 chunks.

Artifacts: `/tmp/openblock-512.png` (1280×720, Classic textures) and
`/tmp/openblock-512-1080p.png` (1920×1080). The ignored tests are
`streaming_unload_plateau_at_512_blocks` and
`join_512_blocks_loopback_transfer`.

---

## 2026-11-25 — v1.1 persistence hardening: async saves, append-only saves, structure-aware spawn

### Environment and methodology

| | |
| --- | --- |
| OS | Fedora Linux 45, kernel 7.3.4 |
| CPU | AMD Ryzen 7 9800X3D (16 threads) |
| Build | `./dev-cargo build --release -p ob_tools --bin save_bench` |
| Tool | `save_bench --radius {8,22,32} --no-migration`, release, fresh temporary world, seed 42 |
| World sizes | 289 / 2,025 / 4,225 columns (17×17 / 45×45 / 65×65 interest rings) |
| Samples | 5 async + 5 sync saves at 289; 3 + 3 at 2,025; 3 + 3 at 4,225 |
| Content | 256 item entities, 18 mobs, 32 furnaces, 16 chests in every run |
| Peak RSS | per-sample `VmHWM` after writing `5` to `/proc/self/clear_refs`, so transient encode/compress buffers are included |

The benchmark runs the async series first, while no previous full-payload
allocation is resident, then the synchronous series. "Sync pause" is the whole
`Host::save()` call on the tick thread: sidecar serialization, full in-memory v4
encode, zstd compression, staged writes, `fsync`s and renames. "Async start" is
the part of `Host::save_async` that runs on the caller's thread: sidecar
serialization plus the `Arc` world snapshot. The worker wall time is measured
separately. The process also holds the world heap (322.5 MiB at 4,225 columns),
so the peak-RSS table reports the delta over the RSS immediately before the
series.

```text
save_bench --radius 8  --saves 5 --loads 2 --no-migration
save_bench --radius 22 --saves 3 --loads 2 --no-migration
save_bench --radius 32 --saves 3 --loads 1 --no-migration
```

### Architecture

- `World` now stores each column as `Arc<ChunkColumn>`. `World::snapshot()`
  copies the pointer map, sorts by `(cx, cz)` and freezes seed and registry
  fingerprint in O(columns); no block data is copied. A later edit
  copy-on-writes only the touched column, so the worker sees a consistent
  pre-snapshot world.
- `Host::save_async()` serializes the sidecars on the tick thread, captures the
  snapshot, and spawns one `ob-save` worker. The worker writes the v4 chunk
  stream section-by-section into a streaming zstd level-3 encoder, then stages
  every payload as `.tmp`, `fsync`s it, renames sidecars first and `world.obs`
  last, and best-effort `fsync`s the directory. The 300+ MiB uncompressed buffer
  never exists on a normal async save; if streaming compression errors, the
  encoder falls back to the historical buffered path.
- At most one writer exists. `save_async()` returns `Ok(false)` while a save is
  in flight; the network autosave coalesces by skipping its interval. The tick
  loop reaps completion with `poll_save()`. A worker error or panic is surfaced,
  marks the host dirty and leaves the previous commit intact; the next autosave
  retries. `save()` remains the synchronous compatibility path and
  `save_and_wait()` drains an in-flight worker before writing current state.
  Shutdown waits up to 60 s for the worker, reaps its result, and uses the
  synchronous fallback if the worker failed or stopped; a worker that is merely
  still running after the timeout yields `SaveTimeout` instead of starting a
  second writer over the same files.

### Results (seed 42, release)

| Metric | 289 columns | 2,025 columns | 4,225 columns |
| --- | ---: | ---: | ---: |
| World heap | 21.59 MiB | 155.06 MiB | 322.45 MiB |
| Sync save pause (mean) | 22.44 ms | 154.92 ms | 317.63 ms |
| Async start pause (mean / max) | **0.28 / 0.35 ms** | **0.31 / 0.33 ms** | **0.41 / 0.50 ms** |
| Async worker wall (mean) | 22.58 ms | 140.22 ms | 288.63 ms |
| Worker encode / write (means) | 21.22 / 0.22 ms | 138.19 / 0.90 ms | 285.16 / 1.76 ms |
| Peak RSS delta, sync | +47.8 MiB | +169.5 MiB | +350.8 MiB |
| Peak RSS delta, async | **+10.4 MiB** | **+16.2 MiB** | **+35.8 MiB** |
| `world.obs` on disk | 655.3 KiB | 4.63 MiB | 9.60 MiB |
| Uncompressed v4 payload | 21.63 MiB | 155.32 MiB | 322.99 MiB |
| Save/reload state hash | PASS | PASS | PASS |

Every async run produced byte-identical `world.obs` bytes, and all reloads
reproduced the pre-save state hash. The synchronous numbers reproduce the
1.0.1 audit within noise at 2,025 columns (154.9 ms here vs 154.5 ms then); at
4,225 columns the old path costs 317.6 ms. The 30 ms tick-pause budget
(PERF-IO-004) is met with a ~0.41 ms attributable cost, about 73× under the
30 ms target and ~775× below the synchronous call. The async worker wall time
is also lower than the synchronous call at 4,225 columns (288.6 vs 317.6 ms)
because streaming avoids materialising and copying the 323 MiB payload. Across
repeated runs the 4,225-column async start stayed between 0.35 and 0.50 ms and
the async peak delta between 26 and 36 MiB, so the margin is robust.

### Save-format compatibility for 1.2 content appends

v4 is unchanged. The loader now accepts a saved registry fingerprint when it
equals the current registry's fingerprint or the fingerprint of one of its
strict prefixes. Appending blocks at the end keeps every saved id valid;
removing or reordering any saved block changes the saved prefix and fails with
`RegistryMismatch { saved, current }` naming both fingerprints. v1/v2 carry no
fingerprint and load ids unchanged by construction; a v3 prefix mismatch is
accepted with a warning, while a v3 reorder is now rejected rather than silently
reinterpreted. The fingerprint covers the ordered block registry, so appending
item types does not affect save compatibility.

Tests generate the 1.0.2-era fixture in code (air through the crafting table,
before the five wave-1 blocks, saved as v4), load it with today's registry,
re-save it with the full fingerprint and prove the state hash is unchanged.
Further tests cover every current prefix, reordered and removed middle blocks,
a mutated saved prefix plus appended blocks, v3 reorder rejection, corrupt
payloads and deterministic snapshot encoding. No large binary fixture is
committed.

### Spawn and structures

`Host::resolve_spawn` now rejects a candidate column that lies inside a
generated structure footprint before the solid/dry/clear/heightmap checks.
Structures are wholly inside their owning chunk, so one
`Generator::structure_in_chunk(candidate_chunk)` lookup suffices. The host uses
the conservative wave-1 maximum half-extent of 4 blocks (the mineshaft adit);
the private per-kind table in worldgen is untouched. A host test samples seeds
42, 3, 29, 94, 7 and 0xA11CE and asserts the stored spawn is outside every
sampled structure bbox and that the helper agrees with generated plans at the
anchor, footprint edge and one block beyond.

Seed 42 is unchanged: spawn remains `(0.5, 81.0, 0.5)`, and the smoke state hash
is `eba0c42219691190` on both this build and a clean HEAD (`66ffde7`) build. The
older `1dba6105028ba6e5` hash in the 1.0.0 section predates the wave-1 structure
generator, not this slice.

### Dedicated-server chunk memory: verdict and recommendation

Not implemented in this slice. The server still retains the union of every
explored column (the 4,225-column world above is ~322 MiB); client eviction does
not free server memory. A save-safe unload needs per-column save provenance
that the current whole-world snapshot does not track:

1. Track a `saved_revision`/dirty epoch per column (or a world generation plus a
   dirty-column set) updated whenever a block is written and cleared only after a
   commit that included that column.
2. Evict only columns outside the union of all players' interest plus a
   two-chunk margin, and only after they are clean. Dirty columns need a small
   forced async save and completion before eviction; never evict while an
   in-flight snapshot or the current tick may still reference the column.
3. Reload deterministically from `world.obs` (generation is a pure function), and
   cover the policy with unload/re-save/reload state-hash tests plus an RSS bound
   at 4,225 columns.

Rushing this without the dirty-epoch proof risks silently dropping edits between
an autosave and an unload; the open item should stay visible until the region
file/IO-thread design in spec 17 §3.5 lands. TODO(1.2): implement the dirty-epoch
unloader with save-before-unload batching and prove no data loss and a bounded
peak RSS.

### Limits and remaining risks

- A save still snapshots and writes every loaded column; there is no incremental
  region file yet. The O(columns) capture and sidecar serialization are
  sub-millisecond at 4,225 columns, but worker wall time grows linearly (~277 ms).
- Peak compressed-body memory is bounded by the encoded file size; incompressible
  data could approach the 323 MiB payload plus the world heap, still below the old
  synchronous full-payload path.
- A save that starts before an edit does not contain that edit. The network
  autosave marks the tick dirty and saves again on the next interval; the
  single-player client still calls the synchronous `Host::save()` path and was
  left to the client/graphics slice.
- The multi-file commit is still not crash-transactional: a crash between two
  renames can mix one new sidecar with the previous world. The previous
  `world.obs` is always intact because it is renamed last.
- `save_bench` async numbers exclude concurrent tick/copy-on-write traffic. The
  start pause is the measured tick-side cost; a sustained edit workload during a
  save pays one column copy per touched column.

## 2026-11-25 — v1.1 high-quality built-in shaders (HDR, sun shadows, water, clouds, post)

### Environment and method

| | |
| --- | --- |
| OS | Fedora Linux 45 (Wayland session not used; headless Vulkan) |
| CPU/GPU | AMD Ryzen 7 9800X3D, integrated RADV "RAPHAEL_MENDOCINO" (Vulkan) |
| Renderer | wgpu 30.0.2, Vulkan/RADV, offscreen `Rgba8UnormSrgb`, seed 42 |
| Build | release (thin LTO, `codegen-units = 1`), `CARGO_TARGET_DIR=/home/martintondel/Projects/openblock/target-shaders ./dev-cargo build --release -p ob_tools --bin render_distance_bench` |
| Scene | 512-block radius (32 chunks, 4,225 columns), cached LOD scene: 2,620 near sections + 1,010 far batches, seed 42 |
| Camera | spawn eye + 18 m, pitch −0.25, time of day 0.42, cloud/water clock 12 s |
| Harness | `render_distance_bench --frames 120 --warmup 10 --no-png`; every measured frame copies the colour target back to the CPU, so the sample includes submission, rasterisation and the blocking readback |

The harness is the same one used for the Wave 1 512-block numbers (p50 5.22 ms at
1280×720 and 7.94 ms at 1920×1080). It has no GPU timestamp queries and includes the
readback, so it is comparable across waves but is not a windowed present-to-present
measurement. Percentiles are nearest-rank over 120 frames. The p50 figures below are the
ones to compare with the earlier p50 baseline; p95/p99 are included because the visual
features add tail work.

### 512-block frame time by preset

| Preset | Resolution | p50 ms | p95 ms | p99 ms | p50 FPS | Passes | Shadow draws | Water draws |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Low (shadows off, clouds off, bloom off, FXAA off, water low) | 1280×720 | 6.80 | 7.53 | 8.26 | 147.1 | 4 | 0 | 16 |
| Low | 1920×1080 | 10.03 | 11.45 | 11.97 | 99.7 | 4 | 0 | 16 |
| Medium (2×1024² shadows, clouds low, water low, bloom, FXAA) | 1280×720 | 9.86 | 10.79 | 11.73 | 101.4 | 9 | 411 | 16 |
| Medium | 1920×1080 | 15.36 | 16.58 | 17.58 | 65.1 | 9 | 411 | 16 |
| **High** (2×2048² shadows, clouds high, water high, bloom, FXAA) | 1280×720 | **11.04** | 11.86 | 12.28 | **90.6** | 9 | 303 | 16 |
| **High** | 1920×1080 | **16.20** | 16.93 | 17.42 | **61.7** | 9 | 303 | 16 |
| **Ultra** (High + 5×5 PCF within 16 m) | 1280×720 | **11.64** | 12.70 | 14.40 | **85.9** | 9 | 303 | 16 |
| **Ultra** | 1920×1080 | **16.21** | 17.21 | 17.45 | **61.7** | 9 | 303 | 16 |

Common frame counts: 861 opaque draws (577 sections + 284 far batches), 6,528,426 opaque
vertices and 1,122 water vertices in the measured view. Passes: Low = main + sky + water +
post; the other presets add two shadow cascades and three bloom passes. Shadow cascades for
High/Ultra are two 2048² `Depth32Float` layers (32 MiB); each shadow pass culls casters to
the cascade and replaces section geometry beyond 64 m with one instanced AABB per section.
`RSS after build` was 452–455 MiB and peak RSS 457–463 MiB in every preset (Wave 1 baseline:
~455 MiB). The reported `mesh estimate` of ~1,216 MiB deliberately counts the same vertex
data twice (CPU cache + GPU buffers); it is not process memory.

### Baseline comparison and targets

| | Wave 1 baseline p50 | 1.1 Low p50 | 1.1 High p50 | 1.1 Ultra p50 |
| --- | ---: | ---: | ---: | ---: |
| 1280×720 | 5.22 ms (191.6 FPS) | 6.80 ms (147.1 FPS) | 11.04 ms (90.6 FPS) | 11.64 ms (85.9 FPS) |
| 1920×1080 | 7.94 ms (125.9 FPS) | 10.03 ms (99.7 FPS) | 16.20 ms (61.7 FPS) | 16.21 ms (61.7 FPS) |

- **Ultra 512 @1080p ≥ 60 FPS: met at p50** (61.7 FPS). It is **not met at p95**
  (17.21 ms ≈ 58.1 FPS); the target statement should quote p50 and the p95 in the same
  sentence.
- **High ≥ 90 FPS**: met at 720p (90.6 FPS p50), **not** at 1080p (61.7 FPS p50). The
  aspirational 1080p/90 target would need roughly half the current shadow-sampling cost.
- **Low close to baseline**: +30% at 720p and +26% at 1080p. The difference is the HDR
  `Rgba16Float` scene target, ACES/vignette post pass and the separate water pass; the
  renderer no longer writes the 1.0 forward pass directly to the sRGB target.

### Per-feature cost (1080p, diagnostic toggles)

These runs start from the High preset and switch individual features off; the base row has
shadows, clouds, water, bloom and FXAA off. GPU clock drift between runs is up to ~0.8 ms,
so read the deltas, not the absolute values (the preset table above is the authoritative
measurement).

| Configuration | p50 ms | Approx. delta |
| --- | ---: | ---: |
| Base: no shadows/clouds/water/bloom/FXAA | 10.0–10.8 | — |
| + Low shadows (2×1024², 3×3 PCF near) | 13.0–14.0 | +3.0–3.9 |
| + High shadows (2×2048², 3×3 PCF near) | 13.8–16.9 | +3.0–3.9 |
| + clouds high + water high (no shadows) | 10.7–11.1 | +0.2–0.8 |
| + bloom only | 10.0–10.2 | +0.1–0.2 |
| + FXAA only | 10.4–10.7 | +0.2–0.6 |

Shadow sampling dominates the added cost. Reducing the map from 2048² to 1024² or the
cascade count from two to one changes the shadow pass itself, but the shader still pays for
one cascade lookup per lit fragment; the two-cascade High/Ultra sets therefore trade the
third cascade pass for a full-resolution far cascade and still deliver 160 m coverage.

### Visual verification

| Capture | Path | Notes |
| --- | --- | --- |
| Day, 512 blocks, High | `/tmp/openblock-shaders-day.png` | tree/terrain shadows, cloud layer, ACES highlight roll-off |
| Dusk, 512 blocks, High | `/tmp/openblock-shaders-dusk.png` | warm horizon, long soft shadows, cloud tint |
| Night, 512 blocks, High | `/tmp/openblock-shaders-night.png` | stars, moonlit palette, dark cloud layer, no visible acne |
| Sky/clouds, 512 blocks, High | `/tmp/openblock-shaders-sky.png` | world-space cloud layer and horizon haze |
| Water scene, High | `/tmp/openblock-shaders-water.png` | transparent basin, Fresnel sky reflection, sun glints, pillar shadows on the water |
| Graphics settings tab (1280×720) | `/tmp/openblock-shaders-settings.png` | preset, shadows, clouds, water, coverage, bloom and FXAA rows |
| F3 overlay with the new counters | `/tmp/openblock-shaders-f3.png` | Graphics/Shadows lines plus the existing pass, mesh and memory counters |

A GPU test (`water_receives_sun_shadows`) isolates blue water pixels and proves that
disabling shadows changes more than 100 of them; the water screenshot difference is 5,918
pixels across the water and tinted rim. Shadow-only A/B captures also prove that shadows
darken more than 200 geometry pixels and that the first cascade contains over 1,000
rasterised casters, so the visual result is not a cleared map.

### 1.1 shader limitations (honest)

- The far shadow cascade skips manual PCF: 3×3 (5×5 within 16 m on Ultra) is used near,
  while 32 m and beyond use the comparison sampler's hardware 2×2 footprint. This is a
  deliberate 4–8× reduction in per-pixel comparisons at 1080p.
- Beyond 64 m in the shadow cascade, sections are represented by their AABBs (one instanced
  draw). Tree silhouettes lose their crowns at that distance, but PCF/distance hides most of
  it; the near 32 m cascade keeps full-detail tree shadows.
- There is no screen-space water reflection/refraction, foam or underwater volume; the water
  surface is Fresnel-blended against the sky and the underlying scene colour, with shadows.
  Far water is carried in the far-batch water buffer and drawn in the same sorted pass.
- Clouds are a 2.5D world-space noise layer, not a volumetric raymarch. Exposure is fixed
  (0.8) rather than histogram-adapted. FXAA-lite, not TAA. Sample count stays 1, so block
  edges rely on the post filter.
- Bloom uses three mips, but the post pass samples the first two; the third is kept for the
  documented threshold/downsample structure and future wide-glow use.
- The offscreen numbers include a GPU→CPU readback and no present; a windowed run will
  differ, especially in the tail percentiles.

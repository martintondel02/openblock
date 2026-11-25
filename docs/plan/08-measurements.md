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

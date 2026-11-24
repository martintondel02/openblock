# Measurements log

> Status: living record · Last updated: 2026-11-24
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

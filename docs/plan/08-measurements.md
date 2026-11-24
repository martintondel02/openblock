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

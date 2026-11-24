# 25 — Performance Budgets and Their Enforcement

> Status: normative - Last updated: 2026-11-24
> Depends on: [DEC-004](../decisions.md), [DEC-007](../decisions.md), [DEC-013](../decisions.md), [DEC-018](../decisions.md), [DEC-025](../decisions.md), [DEC-035](../decisions.md), [DEC-038](../decisions.md), [DEC-039](../decisions.md), [DEC-048](../decisions.md), [DEC-049](../decisions.md), [DEC-051](../decisions.md), [DEC-052](../decisions.md); [conventions](00-conventions.md), [technical architecture](01-technical-architecture.md), [rendering](03-rendering.md), [shaders and visuals](04-shaders-and-visuals.md), [world model](05-world-model.md), [persistence](17-persistence.md)
> Implements: MS-00 onward; gate owner for the MS-04, MS-06, MS-07, MS-15 and MS-16 performance exits

## 1. Purpose and scope

This chapter is the **single authoritative budget table** for OpenBlock. A budget stated
anywhere else in `docs/` is a subsystem allocation that this chapter envelopes; when the two
disagree, this chapter wins and the disagreement is recorded in §5.7. The chapter exists because
[DEC-035](../decisions.md) makes performance normative: a claim without a percentile, a machine,
a scene and a gate is not a budget.

In scope: frame and tick envelopes, hardware tiers, CPU/GPU/memory/IO/network/start-up/tooling
budgets, the measurement protocol, the gate policy and the milestone at which each budget must be
demonstrated. Out of scope: subsystem design (owned by the chapter in the index), gameplay balance
and art direction. Every metric states its direction, owner and machine; a metric that can be
satisfied either way is two metrics, and a budget with no chapter and no milestone is deleted.
`Gate` blocks merge and release, `Warn` files an issue, `Report` is dashboard-only, and the class is
part of the record (§4.1) rather than a judgement call at review.

Render distance is expressed as a **block radius** with the chunk equivalent in parentheses:
512 blocks = a 32-chunk radius = 65×65 = 4,225 columns, never a 512-block diameter
([conventions §4](00-conventions.md#4-units-and-coordinate-conventions)). The client default
is 512 blocks; a dedicated server defaults to a 256-block render distance equal to its
256-block simulation distance, and 512 blocks is an opt-in server setting with a published
cost (DEC-052). Sections are 16³ and a mesh cluster is 2×2×2 sections (32³, visual only,
default from MS-04, [DEC-048](../decisions.md)); the 16³ section remains the storage, edit,
light and network unit. 1024 blocks is a post-1.0 far-terrain target (MS-20, DEC-018) and is
reported, not gated at 1.0. The simulation tick is 20 Hz, fixed
([DEC-038](../decisions.md)); a "30 Hz" server setting raises only the snapshot rate.

## 2. Requirements

| ID | Requirement | Verification |
| --- | --- | --- |
| REQ-PERF-001 | Every budget MUST exist in `perf/budgets.toml` with an ID, metric, unit, percentile, limit, scene, preset, tier, gate class, owner milestone and status; a row missing any field MUST fail CI. | manifest schema test |
| REQ-PERF-002 | Only this chapter may define whole-frame, whole-tick, process-memory, per-player network and start-up budgets; other chapters own subsystem budgets and reference this envelope. | duplicate-ID lint, review |
| REQ-PERF-003 | Budgets MUST be asserted at p95 and p99 on the reference machine, never as a mean alone ([DEC-035](../decisions.md)); every gated scene runs on the bench box with the recorded adapter and driver, a 10 s warm-up and three runs, and the median p95 is the result. | bench report schema, run metadata |
| REQ-PERF-004 | A PR that regresses a gated p95 by more than 10% MUST fail; 5–10% MUST file a warning issue. A memory increase above 10%, a new hitch above 33 ms, or a lost preset row MUST fail. | gate job |
| REQ-PERF-005 | Release builds MUST count violations per budget and log once per 10 s; debug builds MUST assert on the first violation of a gated scene. | log-fires test |
| REQ-PERF-006 | A budget value, gate class or scene definition MUST NOT change without a DEC and a recorded baseline, and the change appears in the release notes; budget IDs are append-only, and a withdrawn budget keeps its ID with a reason. | register diff |
| REQ-PERF-007 | Benchmark scenes MUST be fixed-seed, content-hashed and identical between baseline and candidate runs; a changed scene hash MUST re-baseline rather than fail. | scene hash in report |
| REQ-PERF-008 | Every release MUST publish frame, tick, memory, IO and network results for the minimum, recommended and reference tiers (§3.1); every frame-rate claim MUST state preset, resolution, render distance and scene. | release checklist, docs review |
| REQ-PERF-009 | Memory budgets MUST be measured as process RSS plus GPU allocation counters, with the content distribution named (§5.4); estimates MUST be labelled as estimates. | allocator + RSS probe |
| REQ-PERF-010 | Render distances 256, 512 and 1024 blocks (radii) MUST be reported separately; interpolating between them is forbidden. The 1024 rows are post-1.0 (MS-20) Report measurements, never 1.0 gates. | report table |
| REQ-PERF-011 | Client and server mod costs MUST be attributed per mod and bounded in aggregate (PERF-MOD-001…004), with a mod that exceeds its quota disabled rather than degrading the host. | profiler attribution |
| REQ-PERF-012 | A budget MUST NOT be met by disabling correctness, culling required geometry or dropping a preset row; scene and preset are part of the result and the report MUST name both. | report diff, review |
| REQ-PERF-013 | Bench artifacts (JSON plus environment record) MUST be written to `perf/reports/` and retained for 400 days, linked from the milestone issue and summarised in `benchmarks/<milestone>.md`. | artifact retention |
| REQ-PERF-014 | A milestone whose performance exit criteria fail MUST NOT ship its release label, and a gated regression MUST be bisected within two working days and fixed or reverted, not carried. | milestone audit, bisect log |
| REQ-PERF-015 | The harness MUST pass a negative control: an injected 15% slowdown fails the gate and an unchanged rerun passes, in the same CI run. | harness self-test |
| REQ-PERF-016 | Server capacity numbers MUST come from the bot harness with the published config and the player distribution (co-located or distributed); extrapolation from a smaller run is a Report row only. | capacity report |

## 3. Design

### 3.1 Hardware tiers

The tiers below are the only machines a gate number may name; anything else is a Report row.

| Tier | CPU | GPU / memory | RAM | Storage | Display | Role |
| --- | --- | --- | --- | --- | --- | --- |
| **Minimum** (1.0 floor) | 4 cores / 8 threads, Ryzen 3 3100 or i3-10100 class | GTX 1060 6 GB or RX 580 8 GB | 8 GiB | SATA SSD | 720p, 60 Hz | 60 FPS at a 128–192-block radius, `potato`/`low` |
| **Recommended** | 6 cores / 12 threads, Ryzen 5 5600 or i5-12400 | RTX 2060 / RX 6600, 8 GB | 16 GiB | NVMe | 1080p, 120–144 Hz | 120 FPS at a 384-block radius, `medium` |
| **Reference** (all gates), **Fedora Workstation (44/45 now; then-current + previous at ship), Wayland, Vulkan** | 8 cores / 16 threads, Ryzen 7 5800X3D or i7-12700 | RTX 3060 12 GB or RX 6600 XT | 32 GiB | NVMe | 1080p, 144 Hz | the numbers in §5 |
| **Bench box** (CI), **Fedora Workstation 45, Wayland, Vulkan** (same OS image as the reference) | Ryzen 7 5800X3D, PBO off, governor `performance` | RTX 3060 12 GB, one pinned driver | 32 GiB DDR4-3600 | 1 TB NVMe, ≥ 20% free | 1080p 144 Hz, no compositor effects | gate measurements |
| **Reference server**, **Fedora Server 45** | 4 vCPU of the reference class, 3.0 GHz+ | none (headless) | 8 GiB | NVMe | — | tick, capacity, join |

The bench box has a fixed UEFI, microcode, driver and OS build (Fedora Workstation 45, Wayland,
Vulkan) and runs nothing else; its GPU drives no display. Linux gate numbers are measured on the
Fedora reference; Windows and macOS are Report rows unless a release gate names them ([DEC-054](../decisions.md)). The macOS gate is a Mac mini (M2 Pro) and a `Warn` tier until MS-13 proves Metal.

### 3.2 The frame envelope

A frame is limited by the slower of the CPU main thread and the GPU: the CPU submits frame *N*
and starts *N+1* while the GPU consumes the previous submit (two frames in flight,
[chapter 03 §3.3](03-rendering.md)). The envelope is therefore

`max(cpu_p95, gpu_p95) + os_compositor_reserve + present_margin ≤ frame_period`

and never the sum. The reserve covers the compositor, driver submission and a possible mailbox
blit; the margin keeps the present ahead of the deadline. Both are estimates until MS-04.

| Mode | Target average | Period | 1% low | p99 frame gate | CPU main p95 | GPU p95 | Reserve | Margin | Budget sum | Headroom |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **60 FPS floor** (`potato`/`low`, min spec) | 60 | 16.67 ms | 45 | 22.0 ms | 3.00 ms | 11.00 ms | 1.50 ms | 0.80 ms | 13.30 ms | 3.37 ms |
| **120 FPS default** (`high`, 512-block radius, reference) | 120 | 8.33 ms | 90 | 11.1 ms | 2.00 ms | 6.00 ms | 0.90 ms | 0.50 ms | 7.40 ms | 0.93 ms |
| **144 FPS performance** (512-block radius, reference) | 144 | 6.94 ms | 120 | 8.3 ms | 2.00 ms | 4.00 ms | 0.90 ms | 0.35 ms | 5.25 ms | 1.69 ms |

The 120 FPS row is the 1.0 ship gate at the default preset; the 144 FPS row is a verified mode
whose preset is `performance` ([03 §3.11](03-rendering.md#311-quality-presets)); the 60 FPS row
is the minimum-spec floor. `1% low` is the 99th-percentile frame time as FPS (`1000 / p99`),
never the mean of instantaneous FPS, so the rows are consistent by construction:
1000/22.0 ≈ 45, 1000/11.1 ≈ 90, 1000/8.3 ≈ 120. Frame time is present-to-present wall clock; CPU
and GPU times come from scopes and timestamp queries ([chapter 03 §3.12](03-rendering.md)).
**Hitch**: any frame above 33 ms, in any mode, per scene. **Missed deadline** (frames above the
period) is a Report metric, because a 120 FPS average with an 11.1 ms p99 tolerates tail frames
(PERF-RENDER-007).

Runtime adaptation is policy, not envelope: above 85% of budget for 2 s the renderer lowers render
scale in 0.05 steps, then sheds effects in the documented order, announcing it once in the HUD.
Build-time enforcement is the §3.8 gate plus compiled-in copies asserted in debug builds.

### 3.3 CPU frame budget

The 2.0 ms main-thread budget is split once, here; [chapter 03](03-rendering.md) owns the
renderer's 1.20 ms, and the remaining 0.80 ms is the non-render frame.

| Band | PERF ID | p95 budget | What it includes | Chapter of record |
| --- | --- | ---: | --- | --- |
| Input, events and simulation handoff | PERF-CPU-001/002 | 0.15 ms | Event drain, action mapping, cursor, IME, tick boundary, snapshot acquire, prediction ack | [02](02-engine-core.md) |
| Streaming and meshing dispatch | PERF-CPU-003 | 0.20 ms | Priority updates, job submit, result integration; excludes worker decode/mesh | [05](05-world-model.md) |
| UI layout and text | PERF-CPU-004 | 0.15 ms | Layout, shaping cache lookup, draw-list build for the gameplay HUD; the full-screen menu ceiling is 0.5 ms p99 ([13 PERF-UI-001](13-ui-and-ux.md#5-performance-budgets)), not additive | [13](13-ui-and-ux.md) |
| Audio event dispatch | PERF-CPU-005 | 0.04 ms | Sound-event enqueue and parameter resolution; never mixes here | [14](14-audio.md) |
| Networking main-thread work | PERF-CPU-006 | 0.06 ms | Snapshot encode/decode handoff, event apply, ack bookkeeping | [18](18-networking-multiplayer.md) |
| Mods (client) — reserved, post-1.0 | PERF-CPU-007 | 0.15 ms (reserved) | 1.0 clients run no mod or WASM code (DEC-049); this envelope is not a gate, and client code is a post-1.0 tier | [16](16-modding.md) |
| Renderer (prepare, cull, upload, encode, submit) | PERF-RENDER-001 | 1.20 ms | [Chapter 03 §3.3](03-rendering.md) table | [03](03-rendering.md) |
| Diagnostics and instrumentation | PERF-CPU-008 | 0.03 ms | Counters, timestamp resolve, overlay; off by default (PERF-CORE-008) | [02](02-engine-core.md) |
| **Main-thread total** | PERF-ARCH-001 | **2.00 ms** | Bands sum to 1.98 ms (1.83 ms gated at 1.0 with the client-mod envelope reserved), leaving 0.02 ms margin plus that reserve | [01](01-technical-architecture.md) |
| Worker aggregate | PERF-CPU-009 | 25 ms/frame | All workers in a 30 blocks/s fly-through (≈3 core-equivalents); idle ≤ 8.3 ms | [02](02-engine-core.md) |

Workers consume bandwidth and LLC, which shows up in main-thread p99 — hence the gate.

### 3.4 GPU frame and pass budgets

Per-pass values are owned by [chapter 03 §3.9](03-rendering.md) and
[chapter 04 §5](04-shaders-and-visuals.md) and consolidated here because both envelopes derive
from their sum. The 144 column is the `performance` preset of
[chapter 03 §3.11](03-rendering.md#311-quality-presets): render scale 0.85, one 1024² cascade at
48 blocks, no GTAO, SSR, volumetric fog or clouds, and an 8,192-particle cap.

| Pass | PERF ID | 120 mode (`high`) | 144 mode (performance) |
| --- | --- | ---: | ---: |
| `upload-drain`, `hiz-build`, `cull` | PERF-RENDER-002/004 | 0.30 ms | 0.25 ms |
| `shadow` (CSM) | PERF-VIS-001 | 0.90 ms (3 × 2048²) | 0.35 ms (1 × 1024²) |
| `depth-prepass` | PERF-RENDER-006 | 0.35 ms | 0.30 ms |
| `opaque` | PERF-VIS-002 | 0.80 ms | 0.70 ms |
| `cutout` | PERF-VIS-002 | 0.40 ms | 0.30 ms |
| `sky` and clouds | PERF-VIS-003 | 0.45 ms | 0.25 ms |
| `translucent` / water | PERF-VIS-004 | 0.70 ms | 0.45 ms |
| `particles` | PERF-VIS-009 | 0.25 ms | 0.12 ms |
| `volumetric-fog` | PERF-VIS-005 | 0.35 ms | — |
| `gtao` | PERF-VIS-006 | 0.35 ms | — |
| `taa` + upscale | PERF-VIS-007 | 0.35 ms | 0.28 ms |
| `bloom`, `tonemap-grade` | PERF-VIS-008 | 0.25 ms | 0.18 ms |
| `ui` | PERF-RENDER-005 | 0.20 ms | 0.15 ms |
| **Sum** | PERF-GPU-001 / PERF-GPU-002 | **5.65 ms** (envelope 6.00) | **3.33 ms** (envelope 4.00) |

Draw calls are ≤ 12 geometry and ≤ 32 total in the 120 mode (PERF-RENDER-005); GPU memory is
≤ 768 MB at a 512-block radius (PERF-RENDER-013).

Preset scaling, from [chapter 03 §3.11](03-rendering.md); estimates are ±15%, not gates.

| Preset | Distance (block radius) | Scale | GPU p95 (est.) | Envelope | Tier |
| --- | ---: | ---: | ---: | --- | --- |
| `potato` / `low` | 128 / 256 | 0.60 / 0.75 | 1.9 / 2.2 ms | 4.2 / 4.5 of 16.67 | minimum / iGPU |
| `medium` | 384 | 0.85 | 3.4 ms | 5.7 of 16.67 | recommended |
| `high` | 512 | 1.00 | 5.7 ms | 7.4 of 8.33 | reference gate |
| `performance` | 512 | 0.85 | 3.33 ms | 5.25 of 6.94 | reference mode gate |
| `ultra` | 768 | 1.00 | 7.6 ms | > 8.33; 60 FPS | not a gate |

### 3.5 Tick budget at 20 Hz

The period is 50 ms and the p99 gate is 35 ms ([PERF-ARCH-002](01-technical-architecture.md)),
leaving 15 ms for OS jitter, network bursts, save cycles and deferred work, measured on the
reference server with 2,000 loaded columns and the co-located 50-player case at the default
256-block render distance equal to the 256-block simulation distance (DEC-052; §3.6). A 30 Hz
replication setting (DEC-038) changes none of these numbers.

| Band | PERF ID | p99 budget | Notes |
| --- | --- | ---: | --- |
| World tick | PERF-TICK-001 | 12.0 ms | Random ticks, fluids, growth, scheduled ticks, weather per dimension |
| Entities and AI | PERF-TICK-002 | 10.0 ms | 500 mobs incl. pathfinding and sensing; AI LOD beyond 48 blocks |
| Redstone and block updates | PERF-TICK-003 | 2.0 ms | Deterministic queues and cross-region deferral |
| Lighting | PERF-TICK-004 | 2.0 ms | Matches PERF-LIGHT-002's server cap; overflow deferred in key order |
| Replication and interest | PERF-TICK-005 | 4.0 ms | Per-player interest sets, delta encode, chunk send scheduling |
| Saves (amortised) | PERF-TICK-006 | 2.0 ms | Mean per tick; a save cycle may spike per PERF-SAVE-001 once per 60 s |
| Mods (WASM, reference set) | PERF-TICK-007 | 1.5 ms | Aggregate fuel and host-call time; matches MS-09's exit budget |
| Players, commands, weather, events | PERF-TICK-008 | 1.5 ms | 50 player ticks ≤ 0.3 ms (PERF-GAME-001) plus command/event dispatch |
| **Total** | PERF-TICK-009 | **35.0 ms** | p99; period 50 ms |
| Overruns | PERF-TICK-010 | 0 per hour | A tick above 50 ms is an overrun; five in 10 min pages the operator |

**Overrun policy.** When a band exceeds its budget by 20% in one tick, optional work is deferred
in this order: ambient simulation, random ticks, AI path replans, light cascade, entity spawns.
Player movement, combat, block-edit acknowledgement, inventory transactions and player-state
replication are never deferred; deferrals are counted in `/tps` and logged. Silent shedding is a
bug.

### 3.6 Server capacity model

Capacity is a curve, not a number, and it depends on where the players are. This is an
**estimate** to be replaced by the MS-15 harness (REQ-PERF-016); a published claim MUST cite
the measured row, config and version. The replicated interest set is the union of the columns
inside each player's **render** distance, not the simulation distance alone: a client cannot
render a column the server has not sent (DEC-052). A dedicated server defaults to a 256-block
render distance equal to its 256-block simulation distance, so one player's interest set is
33×33 = 1,089 columns. Fifty co-located players share one chunk cache. In the tight 256-block
case the cache is 37×37 = 1,369 columns (the 1,089 interest set plus the 2-chunk unload ring), of
which about 1,089 tick; the capacity row also budgets the conservative 65×65 = 4,225-column union
bound for a group spread across the 512-block-radius area, not 50 × 1,089 = 54,450. A
512-block-radius render distance is opt-in: each player's interest set becomes 4,225 columns and
the 512-block bandwidth row of PERF-NET-001 applies, while a co-located group's shared cache stays
at the 4,225-column bound. The cost is 4× the per-player interest set and bandwidth; it is a
Report row until MS-15 measures it.

| Case | Render / sim distance | Replicated interest set | Ticking columns | Players / hardware | p99 tick | RSS | Class |
| --- | --- | --- | --- | --- | ---: | ---: | --- |
| Co-located, default | 256 / 256 blocks | 1,089/player; 1,369 tight, 4,225 bound | ~1,089 | 50 / 4 vCPU | ≤ 35 ms | ≤ 3.0 GB | Gate (PERF-SRV-002/003) |
| Co-located, opt-in | 512 / 256 blocks | 4,225/player; 4,225 shared | ~1,089 | 50 / 4 vCPU | ≤ 35 ms (est.) | ≤ 3.0 GB (est.) | Report |
| Distributed (spread over 100k × 100k blocks) | 64 / 64 blocks (4 chunks) | 50 × 81 = 4,050, no sharing | 50 × 81 = 4,050 | 50 / 8 vCPU | ≤ 35 ms (est.) | ≤ 3.5 GB (est.) | Report; lower row |

MS-15 measures both the tight 1,369-column case ([19 §3.9](19-server-administration.md#39-capacity-planning))
and the 4,225-column bound, and a capacity claim that names neither the distribution nor the
distance is not publishable.

The distributed row is the separate, lower-capacity case DEC-052 requires: it needs eight vCPU
rather than four, no cache is shared, every player's columns are loaded separately, and the
published distance is 64 blocks because a 50-player distributed 256-block claim would need
68,450 columns and is not a product. It is Report only until MS-15 measures it; the 4 vCPU /
8 GiB, 50-player claim in [vision §8](../vision.md#8-success-metrics) and
[MS-15](../plan/01-milestones.md) is the co-located default row, not a distributed deployment.
PERF-ARCH-002's 2,000-column baseline is the minimum tick-harness load; MS-15 also runs the
4,225-column co-located cache and the 4,050-column distributed case. Capacity scales
sub-linearly because replication and compression are per-player: 8 vCPU is the step for 100
co-located players.

### 3.7 Measurement methodology

1. **Warm-up.** Discard 10 s: caches, arenas, mesher queues and the page cache change fastest
   early. Warm-up is recorded in the artifact but excluded from samples.
2. **Runs.** Three consecutive runs per scene and machine; report median p95 and p99. If any p95
   differs from the median by more than 2%, re-run once; two invalid sets escalate to a
   machine-health check.
3. **Percentiles and split.** Nearest-rank on per-frame samples; mean FPS is `1000 / mean(frame
   time)`, never the mean of instantaneous FPS. Report CPU main, CPU workers, GPU total and GPU per
   pass separately; CPU versus GPU bound comes from the fence-wait counter.
4. **Noise floor and statistics.** Re-run the same binary ten times after a driver change and
   record the floor, driver and room temperature: a delta below twice the floor is not a
   regression. A regression needs a bootstrap 95% CI of the p95 delta (10,000 resamples) that
   excludes zero **and** a median delta above tolerance. One run never decides.
5. **Environment and scenes.** Performance governor, SMT on, background below 2% of a core,
   ambient below 24 °C, no throttling, vsync off ([chapter 03 §3.10](03-rendering.md)). The
   single scene catalogue is `perf/scenes.toml`: `empty` (10 s), `flythrough` (120 s at
   30 blocks/s), `tnt` (30 s, 64 explosions), `water` (30 s), `cave` (30 s), `megabuild`
   (60 s), `ui_stress` (30 s), and headless `tick50` (600 ticks), `tick16` (3,600 s), `join`,
   `save` (60 s), `gen` (1,000 seeds), `relight` (200 edits), `redstone_stress` (10 min), `pack`
   and the server-side `mods` scene (120 s), each fixed-seed and hashed, plus the harness self-test
   entry `ci`. A budget that names a scene absent from that catalogue fails the manifest load
   (§4.1).
6. **Reports.** JSON per run (§4.2), a committed summary per milestone and a dashboard plotting
   each budget against its limit; comparison in §4.3.

### 3.8 Enforcement, gates and the change process

| Class | Examples | Effect |
| --- | --- | --- |
| **Gate** | Frame envelope, CPU total, GPU total, tick total, client/server memory totals, join to playable, save pause and durability, pack validation, mod aggregate | PR fails; release blocked |
| **Warn** | Criterion microbenchmarks, tooling wall times, macOS Metal p95 | Issue filed with the report; no merge block |
| **Report** | Capacity estimates (including every distributed row), post-1.0 1024-block behaviour and memory (MS-20), mobile tier, power draw | Dashboard only; cited when it changes |

The pipeline is PR → build and unit tests → gated scenes → baseline comparison → merge, with a
`Warn` branch that files an issue and a `Gate` branch that fails, labels `perf-regression` and
bisects. No third outcome merges a failing gate quietly.

**Thresholds and quarantine.** 10% on a gated p95, 20% on p99, 10% on gated memory, any new frame
above 33 ms, 5% on binary size and tool time (`Warn`). A gate failing twice for infrastructure
reasons is quarantined, not silenced: `Warn` for seven days with a named owner and repair date. The
perf bot bisects the labelled range and files a `perf-regression` issue; the author has two working
days to fix or revert. A limit, percentile, scene or class change needs a DEC, a baseline and a
release-notes entry, and MUST NOT land in the PR that regresses it; a 14-day lead-approved waiver
may cover a security fix but never memory or save durability.

## 4. Data structures and formats

### 4.1 Budget manifest

`perf/budgets.toml` is reviewed like code; the schema is generated from these types, so a typo in
an ID or a missing unit is a load error rather than a silent default.

```rust
pub enum Gate { Gate, Warn, Report }
pub enum Tier { Minimum, Recommended, Reference, BenchBox, Server, Mobile }
pub enum Direction { LowerIsBetter, HigherIsBetter }
pub enum Metric { FrameP95, FrameP99, Fps, InputToPhoton, TickP99, CpuTime, GpuTime,
                  PassTime(PassId), Rss, GpuAllocated, ConnectionBytes, DiskThroughput, JoinBytes,
                  BytesPerSecond, StartupTime, WallTime, BinarySize, Fragmentation }

pub struct Budget {
    pub id: BudgetId,            // "PERF-FRAME-002", append-only
    pub area: Area,              // Frame, Cpu, Gpu, Tick, Server, Mem, Io, Net, Load, Tool, Mod
    pub metric: Metric, pub direction: Direction, pub limit: f64, pub unit: Unit,
    pub percentile: Option<u8>,  // None = max, Some(0) = mean, Some(95), Some(99)
    pub scene: SceneId,          // "flythrough", "tick50", ...
    pub preset: Preset,          // "potato", "low", "medium", "high", "performance", "ultra", "n/a"
    pub tier: Tier, pub gate: Gate,
    pub tolerance_pct: f32,      // default 10.0
    pub owner_ms: Ms,            // "MS-04"
    pub status: BudgetStatus,    // Active | Withdrawn(reason)
}
```

Scene definitions live in `perf/scenes.toml` (§3.7;
[24 §3.11](24-testing-benchmarks.md#311-benchmark-scenes-and-the-runner)), and a budget whose
scene is absent there fails the same load check.

### 4.2 Bench report

One JSON document per run, written to `perf/reports/` and content-addressed by
`(commit, scene, preset, scene_hash, machine)`.
It carries enough environment data to reproduce or reject the result, and no personal data: the
machine is a bench-box id, not a hostname.

```json
{"schema":"openblock.bench/1","commit":"9f3c1ab","scene":"flythrough","preset":"high",
 "scene_hash":"blake3:41c2...","tier":"bench_box","warmup_s":10,"runs":3,
 "machine":{"id":"bench-01","gpu":"RTX 3060","driver":"580.12","os":"linux-6.14",
            "governor":"performance","ambient_c":21},
 "frame_ms":{"p50":6.91,"p95":8.02,"p99":10.80,"max":14.90,"samples":36000},
 "cpu_ms":{"main_p95":1.83,"main_p99":2.41,"workers_p95":18.9},
 "gpu_ms":{"total_p95":5.62,"passes":{"shadow":0.88,"opaque":0.79,"taa":0.34}},
 "counters":{"draw_calls":9,"upload_bytes":6144000,"hitches_over_33ms":0,
             "fragmentation":0.11,"pipeline_compiles":0},"memory":{"rss_mb":1642,"gpu_mb":690},
 "verdicts":[{"id":"PERF-FRAME-002","value":121.2,"limit":120.0,"delta_pct":0.3,"verdict":"pass"}]}
```

### 4.3 Comparison algorithm

The harness compares a candidate against the last green run of the same scene on `main`, refuses
different scene hashes, presets or adapters, and treats a missing baseline as "record, do not
gate", so a new scene cannot fail a PR.

```text
base = last_green(scene, preset, tier, scene_hash); cand = current_run(...)
if base is None: record_baseline(cand); return Pass

# d is signed improvement over the baseline: positive = better, negative = regression.
# A regression is negative for both directions, so the matching tail is the upper CI bound.
for b in budgets(scene, preset, tier) where b.gate != Report:
    x = percentile(cand.samples(b.metric), b.percentile)
    y = percentile(base.samples(b.metric),  b.percentile)
    d = (y - x) / y    if b.direction == LowerIsBetter
    d = (x - y) / y    if b.direction == HigherIsBetter
    t = b.tolerance_pct / 100
    ci = bootstrap_ci(cand, base, n = 10_000, stat = (b.metric, b.percentile), tail = upper)
    if b.gate == Gate and d < -t and ci.high < 0: Fail(b, d, ci)
    if b.gate == Gate and b.metric is memory and d < -0.10: Fail(b, d, ci)
    if b.gate == Warn and d < -t: Warn(b, d, ci)
return Pass
```

The direction normalisation is the fix for the old `(x - y) / y` bug: with a single formula a
15% FPS drop on a `HigherIsBetter` budget produced a negative number and passed the old
`d > tolerance` test. Here a drop on either direction is negative and is compared against the
upper tail of the bootstrap distribution. **Negative control:** on `PERF-FRAME-002` an injected
15% FPS drop yields d = −15%, its 95% CI upper bound is below zero, and the gate fails; an
unchanged rerun yields d ≈ 0, its interval straddles zero, and it passes. The same pair runs on
a `LowerIsBetter` frame-time budget by injecting a 15% slowdown (REQ-PERF-015).

## 5. Performance budgets

Unless a row says otherwise: p95, reference machine, 1080p, 512-block-radius render distance, `high`
preset, two frames in flight, vsync off. `MB` is 10⁶ bytes.

### 5.1 Frame and CPU

| ID | Budget | Class | Demonstrated |
| --- | --- | --- | --- |
| PERF-FRAME-001 | 60 FPS floor: ≥ 60 average, 1% low ≥ 45, p99 ≤ 22 ms at a 128–192-block radius on minimum spec, `potato`/`low` | Gate | MS-03 |
| PERF-FRAME-002 | 120 FPS default: ≥ 120 average, 1% low ≥ 90, p99 ≤ 11.1 ms, no frame > 33 ms, 512-block radius, reference | Gate | MS-04 |
| PERF-FRAME-003 | 144 FPS mode: ≥ 144 average, 1% low ≥ 120, p99 ≤ 8.3 ms, 512-block radius, `performance` preset | Gate (mode) | MS-04, MS-12 |
| PERF-FRAME-004 | Hitch: zero frames > 33 ms in `flythrough`, `cave`, `water`; `tnt` allows ≤ 1 per scene per run | Gate | MS-04 |
| PERF-FRAME-005 | Input-to-photon ≤ 3 frame periods p99, ≤ 1 additional frame versus single-player at 80 ms RTT | Gate | MS-04, MS-06 |
| PERF-CPU-001…009 | The bands and values of §3.3, each a Gate except PERF-CPU-007 (client mods, reserved post-1.0 per DEC-049); UI at MS-05, audio at MS-09, server mods at MS-09, diagnostics at MS-02, the rest at MS-04 | Gate | §3.3 |
| PERF-GPU-001 | All GPU passes ≤ 6.00 ms p95 at 120 mode; pass sum ≤ 5.65 ms (§3.4) | Gate | MS-04 |
| PERF-GPU-002 | All GPU passes ≤ 4.00 ms p95 at 144 mode; pass sum ≤ 3.33 ms | Gate | MS-04, MS-12 |
| PERF-GPU-003 | No pass exceeds its §3.4 budget by more than 10% in any gated scene | Gate | MS-04 |

### 5.2 Simulation tick

| ID | Budget | Class | Demonstrated |
| --- | --- | --- | --- |
| PERF-TICK-001 | World tick ≤ 12.0 ms p99 at 2,000 loaded columns | Gate | MS-08 |
| PERF-TICK-002 | Entities and AI ≤ 10.0 ms p99 with 500 mobs; 25% of the 50 ms period (MS-11 exit) | Gate | MS-11 |
| PERF-TICK-003 | Redstone and block updates ≤ 2.0 ms p99 in the `redstone_stress` reference circuit scene | Gate | MS-11 |
| PERF-TICK-004 | Lighting ≤ 2.0 ms p99 per tick; overflow deferred, never unbounded | Gate | MS-05 |
| PERF-TICK-005 | Replication and interest ≤ 4.0 ms p99 at 50 co-located players on the default 256-block render/simulation distance | Gate | MS-15 |
| PERF-TICK-006 | Saves ≤ 2.0 ms p99 amortised; a save cycle is separately bounded by PERF-SAVE-001 | Gate | MS-13 |
| PERF-TICK-007 | Server-side WASM mods ≤ 1.5 ms p99 aggregate over the reference set; client mods are post-1.0 (DEC-049) | Gate | MS-09 |
| PERF-TICK-008 | Players, commands, weather and events ≤ 1.5 ms p99 at 50 co-located players | Gate | MS-15 |
| PERF-TICK-009 | Total tick ≤ 35.0 ms p99, 2,000 loaded columns, 50 co-located players (PERF-ARCH-002) | Gate | MS-02, MS-15 |
| PERF-TICK-010 | Zero overruns above 50 ms in a one-hour soak; five in 10 minutes pages the operator | Gate | MS-07, MS-15 |

### 5.3 Server, network and join

| ID | Budget | Class | Demonstrated |
| --- | --- | --- | --- |
| PERF-SRV-001 | 16 co-located bots, 4 vCPU / 8 GiB, 20 TPS, p99 ≤ 35 ms, RSS ≤ 2 GiB + 20 MB/player (MS-07) | Gate | MS-07 |
| PERF-SRV-002 | 50 co-located bots, 256-block render and simulation distance, 1 h, 20 TPS, p99 ≤ 35 ms, RSS flat | Gate | MS-15 |
| PERF-SRV-003 | Server RSS ≤ 2 GiB + 20 MB/player at 50 co-located players on the 256-block default (PERF-ARCH-007); distributed capacity is a separate Report row | Gate | MS-07, MS-15 |
| PERF-SRV-004 | Server start to accepting connections ≤ 5 s with an existing world; ≤ 30 s for a 10 GiB world; graceful drain within 30 s of `SIGTERM` | Gate | MS-07 |
| PERF-NET-001 | Steady down per player: ≤ 32 KB/s p95 and ≤ 64 KB/s peak at a 256-block radius; ≤ 64 KB/s p95 and ≤ 128 KB/s peak at a 512-block radius | Gate | MS-07, MS-15 |
| PERF-NET-003 | Up per player: ≤ 4 KB/s p95 (three redundant 12 B input frames at 20 Hz plus acks), ≤ 64 KB/s burst | Gate | MS-06 |
| PERF-NET-004 | Join to playable (spawn 3×3 columns plus a 96-block ring) ≤ 5 s p95 at 10 Mbit/s | Gate | MS-06 |
| PERF-NET-005 | Full 512-block-radius interest set (4,225 columns) ≤ 90 s at 10 Mbit/s fresh terrain, ≤ 150 s built; background, cancellable | Report | MS-06 |
| PERF-NET-006 | Join burst cap 2 MB/s per player; a join never starves the tick (p99 ≤ 35 ms) | Gate | MS-06 |
| PERF-NET-007 | 200 visible entities ≤ 20 KB/s per player at 20 Hz (MS-11 exit) | Gate | MS-11 |
| PERF-NET-008 | Tick snapshot ≤ 1,200 B per datagram; ≤ 16 KB per player per tick across datagrams; block edit ≤ 5 B | Gate | MS-06 |
| PERF-NET-009 | Connection buffers: ≤ 512 KB per player, ≤ 8 MiB server-wide plus the per-player share | Gate | MS-07 |

### 5.4 Memory

Client memory uses the [chapter 05 §4.2](05-world-model.md) distribution: a 384-block column has
24 sections, 18 uniform, 5 at 4 bits and 1 at 8 bits; 6 non-empty terrain-like sections per
column is the planning case. GPU figures are allocator totals, not driver reservations. The
1024-block column is the post-1.0 (MS-20, DEC-018) extrapolation and is Report only; the 1.0
Gates are the 256- and 512-block columns.

| ID | Budget | 256-block radius | 512-block radius | 1024-block radius (post-1.0, Report) | Class |
| --- | --- | ---: | ---: | ---: | --- |
| PERF-MEM-001 | World data: blocks, light, column metadata (PERF-WORLD-007) | 90 MB | 350 MB | 1,400 MB | Gate (256/512) |
| PERF-MEM-002 | Mesh geometry in GPU arenas | 96 MB | 192 MB | 768 MB | Gate (256/512) |
| PERF-MEM-003 | Textures: arrays, mips, BC7/BC5 (PERF-VIS-015) | 192 MB | 192 MB | 192 MB | Gate (256/512) |
| PERF-MEM-004 | Shadow atlas and HDR/post targets (reserved) | 72 MB | 120 MB | 144 MB | Gate (256/512) |
| PERF-MEM-006 | UI, glyph and item atlases (PERF-ITEM-007) | 32 MB | 32 MB | 32 MB | Gate (256/512) |
| PERF-MEM-007 | Argument, draw-data, light and cluster buffers | 8 MB | 16 MB | 48 MB | Gate (256/512) |
| PERF-MEM-008 | Audio: decoded SFX pool, stream buffers, voices | 24 MB | 24 MB | 24 MB | Gate (256/512) |
| PERF-MEM-009 | Content, allocator and process overhead (driver, window, code, stacks) | 416 MB | 448 MB | 544 MB | Report |
| PERF-MEM-010 | **Client RSS target** including 15% fragmentation allowance | ≤ 1,400 MB | ≤ 2,000 MB | ≤ 4,500 MB (Report) | Gate (256/512) |
| PERF-MEM-011 | **Client RSS ceiling** ([vision §8](../vision.md), PERF-MVP-004) | ≤ 4 GiB | ≤ 4 GiB | ≤ 6 GiB (Report) | Gate (256/512) |
| PERF-MEM-012 | GPU allocated total (PERF-RENDER-013) | 288 MB | 768 MB | 1,750 MB (Report) | Gate (256/512) |
| PERF-MEM-013 | Server world at 2,000 loaded columns ≤ 160 MB; ≤ 20 MB per player including connection buffers | — | ≤ 160 MB + 20 MB/player | — | Gate |
| PERF-MEM-015 | Server mods aggregate: WASM linear memory plus host structures | — | ≤ 512 MB | — | Gate |
| PERF-MEM-016 | Server total at 50 co-located players on the 256-block default (PERF-ARCH-007) | — | ≤ 3,000 MB | — | Gate |
| PERF-MEM-017 | Persistence per worker / index cache (PERF-SAVE-009) | — | ≤ 8 MB / ≤ 2 MB | — | Gate |
| PERF-MEM-018 | Tools: asset build ≤ 4 GB, world repair ≤ 1 GB, pregen ≤ 50 MB/worker (PERF-GEN-008) | — | — | — | Report |

### 5.5 IO, loading and tooling

| ID | Budget | Class | Demonstrated |
| --- | --- | --- | --- |
| PERF-IO-001 | Chunk load (read, decompress, decode, 16 KiB payload) ≤ 1.5 ms p99 (PERF-SAVE-003) | Gate | MS-13 |
| PERF-IO-002 | Serialize and compress ≥ 150 MiB/s with 4 workers (PERF-SAVE-002) | Gate | MS-13 |
| PERF-IO-003 | Streaming read ≤ 8 MB/s, write ≤ 4 MB/s sustained in flight; no unbounded queue | Gate | MS-06, MS-13 |
| PERF-IO-004 | Save pause: snapshot ≤ 5 ms p99, total tick impact ≤ 30 ms p99 (PERF-SAVE-001) | Gate | MS-13 |
| PERF-IO-005 | At most two `fsync` per dirty region per cycle and one journal `fsync` per metadata transaction (PERF-SAVE-005) | Gate | MS-13 |
| PERF-IO-006 | Join disk and decode ≤ 2 s p95 on SATA SSD, ≤ 5 s on HDD, for a 512-block-radius world | Gate | MS-13 |
| PERF-IO-007 | Graceful flush of 256 MiB dirty state ≤ 2 s (PERF-SAVE-010) | Gate | MS-13 |
| PERF-IO-008 | Fresh 512-block-radius world ≤ 120 MiB on disk; built world ≤ 1.5 GiB (PERF-SAVE-006/007) | Gate | MS-13 |
| PERF-IO-009 | Migration ≥ 50 MiB/s with zero data loss (PERF-SAVE-011) | Gate | MS-13 |
| PERF-IO-010 | Backup of a 1.5 GiB world ≤ 30 s on NVMe, ≤ 3 min on HDD, verified by manifest | Gate | MS-07, MS-16 |
| PERF-LOAD-001 | Cold start to interactive main menu ≤ 10 s p95 (no pipeline cache, SATA SSD) | Gate | MS-03 |
| PERF-LOAD-002 | Warm start to interactive main menu ≤ 3 s p95 | Gate | MS-04 |
| PERF-LOAD-003 | World load to playable, local, warm cache ≤ 5 s p95 (vision §8) | Gate | MS-03, MS-13 |
| PERF-LOAD-004 | Network join to playable ≤ 5 s at 10 Mbit/s (PERF-NET-004) | Gate | MS-06 |
| PERF-LOAD-005 | Zero shader compiles after the first frame; warm pipeline cache hit rate ≥ 90% (PERF-VIS-013/014) | Gate | MS-04 |
| PERF-LOAD-006 | Base pack validation ≤ 400 ms/pack (PERF-VIS-016); content load ≤ 2 s (PERF-VIS-012) | Gate | MS-05 |
| PERF-TOOL-001 | Full base asset build (KTX2, BC7/BC5, mips) ≤ 8 min on 8 cores; `cargo xtask pack validate` ≤ 400 ms/pack and ≤ 30 s for a 100-pack corpus | Warn / Gate | MS-05, MS-09 |
| PERF-TOOL-003 | PR CI wall time ≤ 25 min p95 excluding queue; full benchmark suite (five scenes × three runs) ≤ 20 min | Warn | MS-00 |
| PERF-TOOL-005 | Pregenerate 2,000×2,000 blocks ≤ 10 min on 8 threads (PERF-GEN-011) | Gate | MS-08 |
| PERF-TOOL-007 | Client binary ≤ 40 MB stripped; server ≤ 40 MB; base assets ≤ 150 MB (research 09 §6.7) | Warn | MS-16 |

### 5.6 Mods

| ID | Budget | Class | Demonstrated |
| --- | --- | --- | --- |
| PERF-MOD-001 | Any single server-side WASM mod ≤ 1.5 ms/tick p99; the client 0.30 ms/frame row is a reserved post-1.0 envelope (DEC-049), not a 1.0 gate | Gate (server) | MS-09 |
| PERF-MOD-002 | Reference 8-mod set ≤ 1.5 ms/tick p99 server aggregate; the client 0.15 ms/frame aggregate is reserved post-1.0 (DEC-049) | Gate (server) | MS-09 |
| PERF-MOD-003 | Mod load, validate and compile ≤ 5 s each; hot reload to first correct tick ≤ 5 s | Gate | MS-09 |
| PERF-MOD-004 | Per-mod memory ≤ 64 MB; a server mod at 2× its tick budget for three consecutive ticks is disabled with a log entry | Gate | MS-09 |

### 5.7 Budget index and reconciliation

Prefixes owned by other chapters are indexed here, not re-stated. `PERF-ARCH` (01: main-thread
total, host tick, server memory), `PERF-CORE` (02: scheduler; `Prepare` ≤ 0.5 ms contains
PERF-CPU-003 plus the renderer's 0.30 ms), `PERF-WORLD` (05: memory and streaming),
`PERF-GEN` (06: worldgen workers), `PERF-LIGHT`/`PERF-WX` (07), `PERF-RENDER` (03: the renderer's
1.20 ms and the GPU envelope), `PERF-VIS` (04: pass rows in §3.4), `PERF-GAME` (08),
`PERF-ITEM` (11), `PERF-SAVE` (17: IO rows) and `PERF-MVP` ([plan/02](../plan/02-mvp.md):
checkpoint copies, no new budgets). [18](18-networking-multiplayer.md) and
[21](21-tooling-and-pipelines.md) add further `PERF-NET` and `PERF-TOOL` rows and index the rows here
rather than redefining them.

**Conflicts in the parallel batch, and their resolution.** `PERF-NET` and `PERF-SRV` are claimed
here; [18](18-networking-multiplayer.md) and [19](19-server-administration.md) own protocol
semantics and admin surfaces and reference these rows.

| Conflict | Resolution | Owner |
| --- | --- | --- |
| [03 §3.3](03-rendering.md) gives the renderer 1.20 ms of a 2.0 ms frame; the non-render split was unspecified | §3.3 allocates the remaining 0.80 ms explicitly; the 2.0 ms total is unchanged | MS-04 |
| PERF-CORE-004 says `Prepare` ≤ 0.5 ms, while the renderer's prepare row is 0.30 ms | `Prepare` is the stage total: 0.30 ms renderer plus 0.20 ms streaming and other prepare systems | MS-02 |
| PERF-WORLD-011 allows 0.5 ms for streaming integration; §3.3 gives the main thread 0.20 ms | Split by thread: 0.20 ms main-thread dispatch, 0.50 ms end-to-end including worker handoff | MS-05 |
| PERF-SAVE-001 allows a 30 ms tick impact per save cycle, but PERF-TICK-006 gives saves 2.0 ms p99 | 2.0 ms is the amortised p99; a save tick may spend up to 30 ms of the 50 ms period once per 60 s, outside the 35 ms p99 gate | MS-13 |
| PERF-VIS-010 sums to 5.35 ms, PERF-RENDER-006 allows 6.0 ms | 5.35 ms excludes `upload-drain`, `hiz-build` and `cull` (0.30 ms), plus 0.35 ms slack | MS-04 |
| [03 §3.9](03-rendering.md) and [04 §3.10](04-shaders-and-visuals.md) state 0.90 ms shadow and 1.20 ms opaque + cutout (0.80 + 0.40) | §3.4 consolidates the same values unchanged; the 120-mode pass sum is 5.65 ms (5.35 + 0.30), leaving 0.35 ms of the 6.00 ms envelope | MS-04 |
| §3.4 paired the 144-mode shadow at 0.35 ms with 1 × 2048² at 64 blocks, which contradicts [04 §3.10](04-shaders-and-visuals.md)'s 0.35 ms (1 × 1024²) and REQ-VIS-009's 1-cascade/48-block rule | The row now reads 1 × 1024² at 48 blocks; the 3.33 ms pass sum and 4.00 ms envelope are unchanged | MS-04 |
| MS-09 allows 0.30 ms/frame per mod, but eight such mods cannot fit 0.80 ms of non-render CPU; DEC-049 removes client mod code from 1.0 | 0.30 ms/frame single-mod and 0.15 ms/frame aggregate (PERF-MOD-001/002) are reserved post-1.0 envelopes; the 1.0 gates are the server 1.5 ms/tick single-mod and aggregate rows, and PERF-CPU-007 is not gated | MS-09 |
| The 120/144 FPS rows paired 1% low ≥ 90/120 with p99 ≤ 12/8.5 ms, while 1% low is defined as 1000/p99 (83.3/117.6 FPS) | The p99 gates tighten to 11.1 ms and 8.3 ms, which give exactly 90 and 120 FPS under the existing definition; the 60 FPS row's 22 ms already gives 45 | MS-04 |
| The old capacity table assumed a shared 1,089-column world for 50 players while the interest set follows render distance (DEC-052) | §3.6 states co-located and distributed rows separately: 1,089 columns per player at the default 256-block render distance, a 4,225-column shared cache for 50 co-located players, and a lower distributed row; the 50-player claim is co-located only | MS-15 |
| MS-06 says a 512-block-radius join completes in ≤ 60 s at 10 Mbit/s; a 4,225-column world at 22 KiB/column is ~93 MB, or ~86 s | Playable is 5 s (PERF-NET-004); full interest completes in ~90 s. The MS-06 wording is reconciled at MS-06 with measured column size | MS-06 |
| PERF-NET-016 in [18 §5](18-networking-multiplayer.md) allows 8 ms p95 per tick for host replication, while PERF-TICK-005 gives the tick band 4.0 ms p99 | PERF-NET-016 is the phase total including worker compression and queued chunk sends; PERF-TICK-005 is the tick-thread share. The two are not charged twice to the 35 ms tick | MS-06 |
| [24](24-testing-benchmarks.md) previously cited `PERF-TOOL-004` for the PR pipeline, but [21 §5](21-tooling-and-pipelines.md) owns TOOL-002/004/006/008 and this chapter owns TOOL-001/003/005/007 | PERF-TOOL-003 covers PR CI and benchmark runtime and is the only pipeline budget; 24 now cites it | MS-00 |

### 5.8 Per-milestone demonstration

A milestone is not done until every row in its column has a linked artifact. Rows accumulate;
MS-16 inherits everything.

| Milestone | Budgets it must demonstrate |
| --- | --- |
| MS-00 | PERF-TOOL-003, PERF-FRAME-002 harness, negative control (REQ-PERF-015) |
| MS-01 | PERF-MEM-001 at 256/512, PERF-WORLD-007/008 |
| MS-02 | PERF-TICK-009, PERF-CPU-008, PERF-SRV-003 baseline, PERF-ARCH-002 |
| MS-03 | PERF-FRAME-001, PERF-LOAD-001/003, PERF-MEM-010 at 256, PERF-IO-001/003/006, PERF-MVP-001/002 |
| MS-04 | PERF-FRAME-002/003/004, PERF-CPU-001…009, PERF-GPU-001…003, PERF-MEM-002…012, PERF-LOAD-002/005 |
| MS-05 | PERF-CPU-004, PERF-MEM-003/006, PERF-LOAD-006, PERF-TOOL-001 |
| MS-06 | PERF-NET-001/003…009, PERF-LOAD-004, PERF-FRAME-005 |
| MS-07 | PERF-SRV-001/003/004, PERF-MEM-013…017, PERF-IO-004/005/007/010, PERF-TICK-010 |
| MS-08 | PERF-TICK-001, PERF-GEN-001…014, PERF-TOOL-005 |
| MS-09 | PERF-MOD-001…004 (server rows; client rows reserved, DEC-049), PERF-MEM-015, PERF-TOOL-001 |
| MS-10, MS-11 | PERF-GAME-001…008, PERF-TICK-002/003/006/008 baseline, PERF-NET-007 |
| MS-12 | PERF-FRAME-003, PERF-GPU-002/003, PERF-VIS-001…018, PERF-CPU-005/006, PERF-MEM-008 |
| MS-13 | PERF-IO-001…010, PERF-MEM-017 |
| MS-14 | PERF-TOOL-003, PERF-LOAD-001/002, full published report (REQ-PERF-008) |
| MS-15 | PERF-SRV-002/003, PERF-NET-001/005/009, PERF-TICK-005/009, PERF-MEM-010/011/016, and the distributed capacity row (Report until this milestone measures it) |
| MS-16 | Every active Gate budget on the release candidate, with the signed report attached; Report rows are published, not blocking |
| MS-20 (post-1.0) | PERF-MEM-010/011/012 at 1024 blocks and the §3.5 1024-block cull row, as Report |

## 6. Failure modes and edge cases

| Failure | Symptom | Mitigation |
| --- | --- | --- |
| **Budget inflation** | The 120 FPS gate quietly becomes 110, then 100 | IDs and limits are append-only (REQ-PERF-006); changes need a DEC and appear in the release notes; the dashboard plots the limit as a flat line |
| **Benchmark gaming** | Scenes that cull hidden work, benchmark-only code paths, averages instead of percentiles, vsync on, warm cache reported as cold | Release build with validation in debug, content-hashed scenes, hidden counters (draw calls, upload bytes, compiles), p95/p99 mandatory, scene and preset named |
| **Hardware variance and throttling** | The same commit passes on one GPU, fails on another, or a run is 8% slower with no code change | One bench box per adapter family and per-adapter baselines; another adapter is a Report row; temperature, throttling flags and background load are recorded and a hot run is invalidated, not averaged |
| **CI flakiness** | A gate fails twice a week for infrastructure reasons | Noise floor after every driver change; two-run confirmation; seven-day quarantine with a named repair owner, never a silent disable |
| **Save spike hides in p99** | Tick p99 is green because the save tick is rare | Overruns counted separately (PERF-TICK-010); saves have their own budget (PERF-SAVE-001) |
| **Preset removed to pass a gate** | `high` silently renders like `medium` | The preset row and its settings are part of the result; a missing row fails the report (REQ-PERF-012) |

**Known unknowns and measurement gaps.** These are honest holes, not future work:

- Compositor reserve per OS: 0.9 ms is an estimate; DWM, WindowServer and Wayland differ. Closes
  with a submit-to-present measurement per platform at MS-04.
- HDD join and save: the 22 KiB/column model assumes an SSD. Closes with PERF-IO-006's HDD tier.
- 30 Hz replication bandwidth: snapshot shrink is analytical and may bind on entity-heavy scenes;
  closes with the MS-06 simulator at 200 entities.
- Integrated-GPU floor and mod attribution: `potato` derives from desktop dGPUs and wasmtime fuel
  is not wall time. Close with minimum-tier pass budgets and the MS-09 server wall-clock profiler.
- 1024-block memory and cull: a post-1.0 (MS-20) extrapolation from 512, and cull cost already
  exceeds PERF-RENDER-002 there; it is Report, never a 1.0 gate. Closes at MS-20 with LOD (DEC-018)
  or with a DEC capping the selectable distance at 768 blocks. Scene success also does not prove
  smooth play, so player stutter reports are triaged against counters.
- Distributed-server capacity: the 50-player distributed row is an estimate with no shared cache
  and a different simulation distance; it is Report until MS-15 measures it (DEC-052).

## 7. Testing strategy

**Harness correctness and controls.** Unit and property tests cover the budget loader (missing
unit, duplicate ID, withdrawn ID never gated, unknown scene), percentiles against a brute-force
reference, the direction-normalised comparison algorithm and a golden JSON round-trip. Every gate
PR runs an injected 15% slowdown and an unchanged rerun: the first MUST fail, the second MUST pass
(REQ-PERF-015). The control is run on both a `HigherIsBetter` budget (a 15% FPS drop on
`PERF-FRAME-002`) and a `LowerIsBetter` budget (a 15% frame-time increase), because a single
direction cannot prove the normalisation; a second control injects a 12% memory increase. Once per
phase a reviewer tries to make a scene pass without doing the work.
Per-scene gates then run on every PR touching the owning crate. `flythrough`, `tnt`, `water`, `cave`,
`megabuild` and `ui_stress` gate every PR touching `ob_render`, `ob_world`, `ob_sim`, `ob_ui` or
`ob_client`; `tick50`, `tick16`, `join`, `save`, `gen`, `relight`, `redstone_stress`, `pack` and the
server-side `mods` scene run on PRs touching their crate and nightly on `main`. Nightly adds the
full scene matrix on Linux and Windows CPU, GPU on the bench box, a macOS Metal smoke, 10,000-tick
determinism hashes, 24 h protocol and save fuzz, and a 6 h memory slope; failures open an issue
with the report attached.

**Weekly and per-milestone.** 24 h soak at 50 co-located players, a post-1.0 1024-block memory run
(Report at 1.0), the capacity matrix on 2/4/8 vCPU including the distributed row, and a published
report with the milestone artifact (REQ-PERF-008/013); minimum-spec hardware is exercised quarterly
and before a release that changes the default preset.

## 8. Open questions

1. **144 FPS: gate or mode?** OPEN-003 decides whether a 512-block radius at 144 FPS is a ship
   gate or a verified `performance` mode. This chapter treats it as a mode (PERF-FRAME-003); a
   different answer re-baselines §3.2 and §3.4. Owner: MS-04.
2. **Bench box hosting and custody.** Ownership, funding and driver-update approval for the
   self-hosted gate machine are undecided. Owner: MS-00 with MS-16.
3. **Minimum-spec floor and statistics standard.** The 1.0 floor is inherited from the vision;
   integrated GPUs need a separately tested floor before store pages claim support, and
   nearest-rank p95 with a bootstrap CI may need replacing past 10⁶ frames. Owners: MS-04 (floor),
   MS-00 (statistics).
4. **1024 blocks on 4 GiB cards, and the Metal gate.** PERF-MEM-012 allows 1.75 GB of GPU memory at
   1024, which does not fit a 4 GiB card; the row is post-1.0 Report (MS-20), and macOS remains a
   `Warn` tier until Metal is stable. Owners: MS-12, MS-13.
5. **Simulation-distance default.** The capacity table assumes a 256-block simulation distance
   equal to the dedicated-server render default (DEC-052,
   [chapter 05 §8](05-world-model.md)); if it changes, every PERF-SRV row changes with it.
   Owner: MS-07 with MS-15.
6. **Join completion wording.** MS-06's "≤ 60 s at 10 Mbit/s" and PERF-NET-005's ~90 s need one
   measured definition of "complete" for the 512-block-radius interest set. Owner: MS-06.
7. **Distributed capacity gate.** The 50-player distributed row is published as Report; making it
   a Gate needs its own player-count/RSS limit and a measured harness row. Register item needed:
   a distributed-server capacity gate with its own limit and distribution statement (owner
   MS-15).
8. **Register items needed:** (a) a time-boxed performance-waiver policy (14 days, lead approval,
   never for memory or save durability), so a security fix is not blocked by a gate; (b) budgets and
   scene definitions as release artifacts whose changes require a DEC, including publication of
   any waiver. Owners: MS-00 (a), MS-16 (b).

## 9. References

Decisions: [DEC-004](../decisions.md), [DEC-007](../decisions.md), [DEC-013](../decisions.md),
[DEC-018](../decisions.md), [DEC-025](../decisions.md), [DEC-035](../decisions.md),
[DEC-038](../decisions.md), [DEC-039](../decisions.md), [DEC-042](../decisions.md),
[DEC-048](../decisions.md), [DEC-049](../decisions.md), [DEC-051](../decisions.md),
[DEC-052](../decisions.md). Chapters: [conventions](00-conventions.md) · [architecture](01-technical-architecture.md) ·
[engine core](02-engine-core.md) · [rendering](03-rendering.md) ·
[shaders and visuals](04-shaders-and-visuals.md) · [world model](05-world-model.md) ·
[world generation](06-worldgen.md) · [light and weather](07-light-and-weather.md) ·
[survival](08-gameplay-survival.md) · [items](11-inventory-crafting-items.md) ·
[persistence](17-persistence.md) · [risks](../plan/05-risks.md) ·
[release and operations](../plan/06-release-and-operations.md) · [milestones](../plan/01-milestones.md)
· [MVP checkpoints](../plan/02-mvp.md) · [roadmap](../plan/00-roadmap.md).

Research: [persistence and packaging](../research/09-persistence-assets-packaging.md) (join, save
and binary sizes) · [networking](../research/04-networking-sota.md) (bandwidth and tick
allocation) · [voxel rendering](../research/03-voxel-rendering-sota.md) (meshing and culling) ·
[Weft teardown](../research/01-weft-teardown.md) (measured reference points only).

External:

- Microsoft, *Windows 11 DWM flip model and frame latency* — <https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model>; Khronos, *VK_EXT_frame_boundary* — <https://registry.khronos.org/vulkan/specs/latest/man/html/VK_EXT_frame_boundary.html>
- Criterion.rs user guide — <https://bheisler.github.io/criterion.rs/book/index.html>; Efron and Tibshirani, *An Introduction to the Bootstrap* (1993), for percentile confidence intervals.

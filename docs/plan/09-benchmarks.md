# 1.0 benchmark and soak tools

> Status: living record · Last updated: 2026-11-25
> Depends on: [Measurements](08-measurements.md), [Testing and benchmarks](../spec/24-testing-benchmarks.md),
> [Performance budgets](../spec/25-performance-budgets.md), [1.0 scope](1.0-scope.md)

This page is the map for the CPU-only benchmark and soak tools added for the
1.0 performance envelope. The numbers themselves are recorded in
[Measurements](08-measurements.md); the normative budgets stay in
[spec/25](../spec/25-performance-budgets.md). Nothing here restates a budget.

## 1. Tools

| Binary | Source | What it measures | Quick profile | Reference profile |
| --- | --- | --- | --- | --- |
| `worldgen_bench` | [worldgen_bench.rs](../../crates/tools/src/bin/worldgen_bench.rs) | Per-chunk generation latency, chunks/s, block/cave/ore densities, determinism | `--quick` (6×6 chunks × 3 seeds) | default 12×12; `--full` 40×40 |
| `server_bench` | [server_bench.rs](../../crates/tools/src/bin/server_bench.rs) | `Host::tick` p50/p95/p99/max for idle, moving player, mobs, items, containers; streaming; RSS; save/reload | `--quick` (300 ticks, radius 5) | `--ticks 1000`, reference `--radius 22` (2,025 columns) |
| `save_bench` | [save_bench.rs](../../crates/tools/src/bin/save_bench.rs) | Save/sidecar size, save and reload latency, state-hash round-trip, v1–v4 migration smoke | `--quick` | `--radius 8` / `--radius 22` |
| `soak` | [soak.rs](../../crates/tools/src/bin/soak.rs) | 60,000-tick scripted survival session, invariant/cap checks, save-size and RSS series, final save/reload hash | `--quick` (4,000 ticks) | `--ticks 60000` |

All four are CPU-only and headless: they never initialize a GPU adapter or open
a window, never touch `crates/client`, and live in the `ob_tools` crate behind
the public `Host`/`World`/`Generator` APIs. The separate offscreen render
benchmark (`openblock-tools bench`) is unchanged.

Shared measurement helpers live in
[measure.rs](../../crates/tools/src/lib.rs) and implement the nearest-rank
percentiles of spec/25 §3.7, a Linux `/proc/self/status` RSS proxy and a
self-cleaning temporary data directory.

## 2. Commands

From the workspace root (the `CARGO_TARGET_DIR` is optional; CI uses its own):

```bash
cargo run --release -p ob_tools --bin worldgen_bench -- --quick
cargo run --release -p ob_tools --bin worldgen_bench -- --full \
  --csv target/worldgen.csv --markdown target/worldgen.md

cargo run --release -p ob_tools --bin server_bench -- --quick
cargo run --release -p ob_tools --bin server_bench -- \
  --ticks 1000 --radius 22 --csv target/server.csv --markdown target/server.md

cargo run --release -p ob_tools --bin save_bench -- --quick
cargo run --release -p ob_tools --bin save_bench -- \
  --radius 8 --items 256 --mobs 18 --furnaces 32 --chests 16 --saves 5

cargo run --release -p ob_tools --bin soak -- --quick
cargo run --release -p ob_tools --bin soak -- --ticks 60000
```

Each binary prints a human-readable report and can additionally write CSV and
Markdown tables. `server_bench` and `soak` exit non-zero when the save/reload
hash differs or an invariant fails; `worldgen_bench` exits non-zero on a
determinism mismatch or an explicitly requested timing gate. Wall-clock gates
are opt-in (`--max-mean-ms`, `--max-p95-ms`) and are not enabled on shared CI.

## 3. What the reference run measures

The reference profile in [Measurements](08-measurements.md) records:

- worldgen: mean/p50/p95/p99/max ms per chunk, chunks/s, non-air/carved/ore
  counts, and sampled-chunk determinism for seeds 1, 42 and
  `0xc0ffee0012345678`;
- server: 1,000-tick phases for idle, one moving player, natural passive and
  hostile mob caps, break-drop plus stress item entities, and lit
  furnace/full-chest containers, plus cold streaming and save/reload;
- persistence: save p50/p95/p99/max, world directory and sidecar breakdown,
  warm reload latency, state-hash round-trip, and v1/v2/v3 migration readers;
- soak: 60,000 ticks (50 game minutes) with breaks/places, combat, meals,
  deaths, streaming churn, periodic saves, RSS and save-size series, and a
  final save/reload hash.

## 4. CI

The `bench` job in `.github/workflows/ci.yml` builds the release tools and runs
the four `--quick` profiles on Ubuntu. The quick profiles assert correctness
(determinism, invariants, save/reload hash), not wall-clock thresholds; a
shared hosted runner is not a bench box. Their measured local run time is
recorded in [Measurements](08-measurements.md).

The pre-existing offscreen render gate, save-size guard and Fedora/Windows/macOS
matrix are unchanged.

## 5. Known gaps

- **Multiplayer capacity is not in this lane.** The tools measure the
  single-player `Host`; raw protocol-v2 loopback bots and the server capacity
  matrix (PERF-SRV-002/003, MS-15) remain a follow-up. The protocol load script
  is owned by the server/networking slice.
- **The 35 ms tick gate is not a 50-player claim.** Measuring 1,000 ticks on a
  single-player host with 18 mobs is a baseline, not the 2,000-column
  50-player co-located case in spec/25 §3.5. The reference `--radius 22` run
  exercises the 2,025-column resident set but still ticks one host player.
- **The mob population is the natural cap (10 passive + 8 hostile).** The
  spec's 500-mob AI budget (PERF-TICK-002) needs a separate stress scene; it is
  not claimed by `server_bench`.
- **No GPU or frame-time coverage.** The offscreen render benchmark is the
  existing gate; this lane is CPU/simulation only.
- **World heap is at the 160 MB limit.** The reference 2,025-column world uses
  162.54 MB decimal, ≈160.5 MB scaled to the 2,000-column budget (≈0.3% over);
  see [Measurements](08-measurements.md).
- **Migration fixtures are synthesized.** `save_bench` writes v1/v2/v3 layouts
  from the current world and loads them through the migration readers; the
  historical fixture bytes themselves are covered by `ob_world::save` tests,
  because no released fixture files are committed to the repository.
- **RSS is a process proxy.** It reads `/proc/self/status` `VmRSS`; it includes
  allocator retention and the test harness, not only the host world heap.
- **Soak wall-clock is much faster than real time.** 60,000 ticks run in a few
  seconds on the reference host; it is a 50-minute *simulation* soak, not a
  50-minute wall-clock leak test. The 2 h/24 h bot soak in spec/24 §3.13 remains
  the long-running gate.
- **Whole-world save pause is over budget at scale.** The measured 2,025-column
  save takes ≈156 ms, against the 30 ms PERF-IO-004 tick-impact budget; the
  current snapshot writer is synchronous. The measurement and the 289-column
  (21–24 ms) result are in [Measurements](08-measurements.md).

## Open questions

1. **Protocol bot harness** (owner MS-15): should the in-process
   `DedicatedServer` + raw v2 client loopback benchmark live in `ob_tools` or in
   the server/networking test scripts? It needs its own budget row before it can
   gate.
2. **Quick-lane budget** (owner MS-00): the four quick profiles measured
   1.31 s total locally (0.31 + 0.21 + 0.34 + 0.45 s), but the hosted-runner
   variance should be measured before adding a hard time limit or enabling
   `--max-mean-ms`.
3. **500-mob stress scene** (owner MS-11): the natural cap is 18 mobs; a
   dedicated AI stress scene is needed to demonstrate PERF-TICK-002.
4. **Reference 2,000-column save run** (owner MS-13): the save benchmark can be
   run at `--radius 22`; a published number should name the exact column count
   and content pack, as spec/25 requires.

## References

- [Measurements](08-measurements.md) — the recorded results and environment
- [Testing and benchmarks](../spec/24-testing-benchmarks.md) — harness policy,
  scene catalogue and soak tiers
- [Performance budgets](../spec/25-performance-budgets.md) — normative tick,
  IO and memory budgets
- [Persistence](../spec/17-persistence.md) — save format, sidecars and
  migration chain
- [World generation](../spec/06-worldgen.md) — generation determinism and
  density contracts
- [Tools crate](../../crates/tools/Cargo.toml) — binaries and dependencies
- [CI workflow](../../.github/workflows/ci.yml) — quick benchmark job

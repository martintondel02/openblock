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
| `save_bench` | [save_bench.rs](../../crates/tools/src/bin/save_bench.rs) | Save/sidecar size, save and reload latency, state-hash round-trip, v1–v4 migration smoke; `--phases` adds the serialize/compress/write/fsync breakdown | `--quick` (and `--quick --phases` in CI) | `--radius 8` / `--radius 22`, `--phases` |
| `soak` | [soak.rs](../../crates/tools/src/bin/soak.rs) | 60,000-tick scripted survival session, invariant/cap checks, save-size and RSS series, final save/reload hash | `--quick` (4,000 ticks) | `--ticks 60000` |
| `load-gate.py` | [load-gate.py](../../scripts/load-gate.py) | Spawns the real `openblock-server`, drives protocol-v2 bots, waits for the interest set, exercises Move/edit/chat/ping and reports server tick percentiles, TPS, RSS, bandwidth, chunks, edits, a save/reload soak check and autosave/overrun correlation from the server log | `--quick` (4 bots, 12 s; target advisory) | 8 bots / 60 s at radius 6; `--soak` 8 bots / 600 s; `--wait-interest`, `--save-interval 0` diagnostics |
| `reliability-audit.py` | [reliability-audit.py](../../scripts/reliability-audit.py) | Restart/save/reload cycles, 8-client churn, persistent-bot soak with `/proc` RSS/fd/thread sampling, truncated sidecars/`world.obs`, SIGTERM under save, save+stop race and read-only/file-size-limit disk proxies | n/a (20 cycles, 60 s churn, 300 s soak) | `--soak-seconds 1800` / 24 h soak for the long gate |

The four Rust tools are CPU-only and headless: they never initialize a GPU
adapter or open a window, never touch `crates/client`, and live in the
`ob_tools` crate behind the public `Host`/`World`/`Generator` APIs. The Python
load gate spawns the real headless server binary on loopback with a temporary
data directory and ephemeral port; it never opens a window or a GPU adapter.
The separate offscreen render benchmark (`openblock-tools bench`) is unchanged.

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
cargo run --release -p ob_tools --bin save_bench -- \
  --radius 22 --saves 5 --no-migration --phases

cargo run --release -p ob_tools --bin soak -- --quick
cargo run --release -p ob_tools --bin soak -- --ticks 60000

# Multiplayer gate: build the headless server, then run 4 or 8 loopback bots.
cargo build --release -p ob_server --bin openblock-server
python3 scripts/load-gate.py --quick \
  --binary target/release/openblock-server \
  --json target/load-gate.json --markdown target/load-gate.md
python3 scripts/load-gate.py --clients 8 --seconds 60 \
  --binary target/release/openblock-server \
  --json target/load-gate-full.json --markdown target/load-gate-full.md
python3 scripts/load-gate.py --soak \
  --binary target/release/openblock-server \
  --json target/soak.json --markdown target/soak.md

# Save-pause diagnostics: steady-state window after interest sets complete,
# 1 Hz autosave; the report correlates overrun warnings with autosave ticks.
python3 scripts/load-gate.py --clients 8 --seconds 60 --save-interval 1 \
  --wait-interest --no-gate --binary target/release/openblock-server \
  --json target/save-pause.json --markdown target/save-pause.md

# Reliability lane: restart/churn/soak/corruption/SIGTERM/disk proxies.
python3 scripts/reliability-audit.py \
  --binary target/release/openblock-server \
  --cycles 20 --churn-seconds 60 --soak-seconds 300 \
  --sigterm-iterations 10 \
  --json target/reliability.json --markdown target/reliability.md
```

`save_bench --phases` measures `Host::save` as serialize+compress+CRC, pure
zstd compression, write+rename, fsync probes and a residual term; it is a
diagnostic, not a new gate. `load-gate.py --wait-interest` starts the measured
window only after every bot has its full interest set, and `--save-interval 0`
is accepted so an operator can reproduce the no-autosave control. Neither flag
changes the default gate: the published 1.0 numbers still use the original
join-aligned window.

Each tool prints a human-readable report and the Rust tools can additionally
write CSV and Markdown tables; the load gate writes JSON and Markdown. The Rust
`server_bench` and `soak` exit non-zero when the save/reload hash differs or an
invariant fails; `worldgen_bench` exits non-zero on a determinism mismatch or
an explicitly requested timing gate. The load gate exits non-zero on functional
failures (join, interest set, chunks, edits, pongs, disconnects) and, in the
full and soak profiles, on a 35 ms tick p99 miss; `--quick` reports the target
as advisory unless `--enforce-quick` is passed. Wall-clock gates for the
single-player benchmarks remain opt-in (`--max-mean-ms`, `--max-p95-ms`) and
are not enabled on shared CI.

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
  final save/reload hash;
- load gate: 8 protocol-v2 bots at a 6-chunk interest radius for 60 s on a
  fresh server, recording join and interest-set load time, chunk frames,
  bytes in/out per client, Move/edit/chat/ping counters, nearest-rank tick
  p50/p95/p99/max over the server's rolling window, TPS, overruns, RSS and
  shutdown drain; the `--soak` profile repeats it for 10 minutes with 10 s
  autosaves, an RSS/fd/thread plateau check and a save/reload state-hash check.

## 4. CI

The `bench` job in `.github/workflows/ci.yml` builds the release tools and runs
the four `--quick` profiles on Ubuntu, now with CSV/Markdown output and an
artifact upload; `save_bench --quick --phases` also exercises the phase probe.
A separate `load-gate` job builds the headless server and runs
`scripts/load-gate.py --quick --wait-interest` (4 bots, 12 s, temp data
directory, ephemeral loopback port, no GPU) and uploads the JSON/Markdown
report. An opt-in `reliability` job (`workflow_dispatch` with
`run_reliability=true`) runs the 20-cycle/churn/300 s-soak audit and uploads its
report; it is manual because the full lane costs ~15 minutes.

The quick profiles assert correctness (determinism, invariants, save/reload
hash, joins, interest sets, chunks and edits), not wall-clock thresholds; a
shared hosted runner is not a bench box, so the 35 ms target is advisory in the
quick gate. The local quick gate took about 14 s of run time (0.05 s startup +
2.2 s streaming + 12 s load + shutdown) before `--wait-interest`; the wait adds
~2.1 s of pre-measurement streaming. Measured local numbers are recorded in
[Measurements](08-measurements.md).

The pre-existing offscreen render gate, save-size guard and Fedora/Windows/macOS
matrix are unchanged.

## 5. Known gaps

- **The 8-player gate is smaller than the 50-player reference case.** The load
  gate now measures 8 co-located protocol-v2 bots at a 6-chunk (169-column)
  interest radius and passes the 35 ms tick p99 target on the local
  workstation; the 16- and 50-player 256-block rows (PERF-SRV-001/002/003) and
  the 2,000-column resident set remain unmeasured by this harness.
- **The "reference machine" caveat.** The 1.0 gate names the 4 vCPU / 8 GiB
  reference-server class. The recorded 8-player run is a single run on a
  Ryzen 7 9800X3D workstation (8 cores/16 threads, 30 GiB) with the same
  process running on loopback, so it demonstrates the software path, not the
  published reference-hardware row. A 4-vCPU pinned run and the 3-run median
  rule of spec/25 §3.7 are still required before quoting the gate as an
  official capacity result.
- **The bots are synthetic.** They speak the v2 framing and consume every
  server message, but they do not decode chunk payloads, simulate physics or
  verify world state; the scenario is creative so edits are deterministic and
  survival item/hunger paths are only exercised by the scripted `soak` tool.
- **Zero overruns is not demonstrated.** The 10-minute 8-bot soak recorded one
  85.8 ms tick immediately after an autosave (synchronous `Host::save`), while
  its p99 was 0.146 ms. The no-overrun part of `PERF-TICK-010` needs a longer
  run after the save-pause work, not a relaxed threshold in this harness.
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
  save takes 154–156 ms, against the 30 ms PERF-IO-004 tick-impact budget; the
  current snapshot writer is synchronous. At 1,089 columns an end-to-end 1 Hz
  autosave moves tick p99 to 85 ms and every logged overrun lands on an
  autosave tick. The 289-column 21–24 ms result is tmpfs-only; on a btrfs data
  directory the same save is slower and the 169-column point is the currently
  demonstrated safe size. The phase breakdown and design proposal are in
  [the 1.0.1 performance audit](1.0.1-performance.md).
- **`--ticks 0` is not a read-only verification.** `Host::open` streams the
  two-column ring and the headless path saves it, so reopening the same world
  directory can grow the resident set and change the state hash. Reliability
  checks compare two independent copies instead; a read-only hash mode is an
  open item.
- **No last-good snapshot fallback.** A truncated `world.obs` is rejected with
  a non-zero exit (good: no silent partial load), but there is no previous-
  generation recovery. Sidecar truncation degrades to warnings and missing
  records.

## Open questions

1. **Protocol bot harness** (owner MS-15) — resolved for the 8-player row:
   `scripts/load-gate.py` owns the server spawn, bot loop and report, so it
   does not need an in-process `DedicatedServer` harness in `ob_tools`. The
   remaining question is whether the 16/50-player matrix gets its own budget
   row and runner or extends this script.
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
- [CI workflow](../../.github/workflows/ci.yml) — quick benchmark, load-gate
  and manual reliability jobs
- [1.0.1 performance audit](1.0.1-performance.md) — save-pause decomposition,
  reliability results and the design proposal

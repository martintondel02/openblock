# Benchmark reports

This directory holds benchmark run artifacts produced by `cargo xtask bench` and
the CI bench jobs. Reports are JSON documents following
`openblock.bench/1` ([spec/25 §4.2](../../docs/spec/25-performance-budgets.md)).

- One file per run: `perf/reports/<commit>-<scene>-<preset>-<machine>.json`.
- Reports contain no personal data: the machine is a bench-box id, not a
  hostname.
- The comparison algorithm compares against the last green run of the same
  scene/preset/tier/scene-hash; a new scene records a baseline instead of
  gating.
- Only summarized reports and baselines are committed (`baselines/`); raw
  per-frame dumps are CI artifacts and are not committed.

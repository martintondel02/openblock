# ADR-0004 — 16³ sections with paletted storage

> Status: accepted; amended by DEC-048 (32³ mesh clusters are the default from MS-04) · Date: 2026-11-24 · Decision: DEC-004, DEC-005

## Context

The chunk section is the most consequential data-structure decision in a voxel
engine. It determines memory, edit latency, meshing cost, network delta size,
lighting propagation, save format and the mod-visible world API. The main
candidates were 16³ (Minecraft, Sodium, Weft) and 32³ (fewer sections, fewer
draw records), or a hybrid where storage is 16³ and meshing groups 2×2×2
sections into 32³ clusters.

Key numbers from the research:

- A 32³ storage section is 8× the volume of 16³; a single corner edit dirties 8×
  as many blocks and makes full-section resends 8× larger.
- Paletted 4-bit storage costs ~2 KiB per 16³ section; a 32³ section costs
  ~16 KiB and its palettes have higher entropy (more bits per entry).
- Light storage dominates: 4 B/block (sky8 + RGB8) costs ~415 MB at a 512-block
  radius; packing to 2 B/block halves it to ~208 MB.
- Every production voxel renderer inspected stores 16³ sections; draw-call
  pressure is solved by batching at a higher level (regions/clusters,
  multi-draw-indirect, GPU culling), not by larger storage units.

## Decision

1. **Storage, editing, lighting and network deltas are 16³ sections.** Chunk
   columns are 16×16. Heights are multiples of 16.
2. **Paletted storage** with uniform (0-bit), 4-bit, 8-bit and 16-bit modes, bit
   packing in u64 words without straddling, and an overflow palette above 256
   distinct states.
3. **Light is stored separately**, packed at 2 B/block (sky4 + RGB4×3), so
   relighting does not rewrite block data and light memory is halved versus the
   naive 4 B/block layout.
4. **Meshing may cluster 2×2×2 sections (32³).** This is a render-side
   optimisation and, per DEC-048, the default meshing unit from MS-04 (the MS-04 benchmark sets its tuning, not whether it is used);
   content, saves and protocol never see clusters.

## Consequences

- Edit latency, network deltas and light propagation stay proportional to 16³,
  which keeps building responsive and multiplayer bandwidth sane.
- Draw-call pressure is handled where it belongs: region/cluster culling,
  multi-draw-indirect and GPU compaction (DEC-013).
- A 32³ cluster mesher adds complexity (cross-section face merging, cluster-edge
  handling) that must be benchmarked against the simpler per-section mesher
  before becoming the default.
- The light format is a compatibility commitment: changing bits-per-channel
  later is a save-format migration.
- Memory at 512 blocks fits the budget (~208 MB light + ~150 MB blocks for
  typical terrain) with headroom for meshes and textures.

## Alternatives considered

- **32³ storage** — rejected: 8× edit/remesh/network cost, worse palette
  entropy, and no production precedent; keeps the door open only as a mesh
  cluster.
- **4 B/block light** — rejected: doubles the dominant memory term for no
  gameplay benefit.
- **Separate light world / GPU-only light** — rejected: lighting is
  gameplay-visible and must be authoritative on a headless server.
- **Run-length or sparse-only storage** — rejected as the primary format: good
  for homogeneous regions, poor for mixed terrain and edits; the uniform-section
  fast path captures most of the win.

## References

- [Voxel rendering SOTA](../research/03-voxel-rendering-sota.md)
- [Weft teardown](../research/01-weft-teardown.md)
- [World model chapter](../spec/05-world-model.md)

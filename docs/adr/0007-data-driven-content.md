# ADR-0007 — Data-driven content with namespaced registries

> Status: accepted · Date: 2026-11-24 · Decisions: DEC-006, DEC-019

## Context

The game must eventually express ~1,200 blocks, ~1,500 items, ~160 entities,
~65 biomes, recipes, loot, worldgen, sounds, UI and translations — and then
extend all of it with community content. Hard-coding that in Rust would make
content changes require a recompile, block modding, and make compatibility
between game versions a code-merge problem. Minecraft's datapack/resource-pack
split proves the data-driven approach works at scale, but its registry model is
incomplete (no new blocks, no behaviour without mod loaders).

## Decision

1. **Everything declarative is data**: block/item/entity/biome/recipe/loot/
   worldgen/feature/sound/model/UI definitions live in content packs with
   schemas, validation and versioned migrations.
2. **Namespaced resource IDs** (`namespace:path`) are the canonical identity.
   Numeric `u16` registry indices are a session-local optimisation and never
   cross process or restart boundaries (DEC-006).
3. **Registries freeze** after pack loading into dense arrays and lookup tables;
   hot paths use indices, disk/network/mod APIs use IDs.
4. **Layered packs with deterministic, phase-ordered resolution** and explicit
   EDIT/REPLACE/REMOVE operations plus `requires`/`load_after`/`load_before`
   dependencies. Conflicts are errors with actionable messages; there is no
   "last writer wins".
5. **Content hash** (Merkle over resolved registries + schemas + protocol
   version) is exchanged in the multiplayer handshake so clients and servers
   cannot silently diverge.
6. **The base game is pack #0** and uses the same schemas, hooks and validation
   as third-party packs.

## Consequences

- Content changes do not require engine changes; mods and official content share
  one pipeline, and the engine code shrinks over time.
- Schemas become a public compatibility surface: they need versioning,
  migrations, documentation and backwards compatibility testing.
- Validation must be excellent: error messages with file, line, field and
  expected type are a product feature, not a nicety.
- Load time and memory must be budgeted; a large modpack can otherwise dominate
  startup.
- Numeric-index discipline is essential: any leak of indices into saves or
  network data is a corruption bug.

## Alternatives considered

- **Code-first registration (Rust builder APIs)** — fine for engine plugins, but
  requires recompilation and produces version-coupled mods; kept for the native
  tier only.
- **Unstructured JSON blobs** — rejected: no validation, no migrations, poor
  tooling.
- **Single monolithic pack format** — rejected: no layering, no overrides, no
  ecosystem.

## References

- [Modding research](../research/06-modding-and-content-system.md)
- [Content system chapter](../spec/15-content-system.md)
- [Weft teardown](../research/01-weft-teardown.md)

# 00 — Conventions, Terminology and Document Map

> Status: normative · Last updated: 2026-11-24 · Supersedes: nothing
>
> This document is binding on every other document in `docs/`. If a chapter
> needs to deviate, it must say so explicitly and link to the decision that
> permits it.

## 1. Purpose

The OpenBlock specification is a set of documents that together are
*decision-complete*: a competent engineer can implement a milestone without
asking "what did we mean by this?". This chapter defines how those documents are
written, what words mean, and which identifiers to use when cross-referencing.

Read order for a newcomer:

1. [`../vision.md`](../vision.md) — what we are building and why.
2. [`../decisions.md`](../decisions.md) — the binding decision register (DEC-xxx).
3. [`01-technical-architecture.md`](01-technical-architecture.md) — how the
   pieces fit together.
4. The chapter for the subsystem you are touching.
5. [`../plan/01-milestones.md`](../plan/01-milestones.md) — when it gets built.

## 2. Normative language

| Word | Meaning |
| --- | --- |
| **MUST** / **MUST NOT** | Absolute requirement. A change that violates it is a bug, not a preference. |
| **SHOULD** / **SHOULD NOT** | Expected default. Deviation needs a written justification in the PR or a new DEC. |
| **MAY** | Genuinely optional. |
| **TBD** | Not yet decided. Every TBD MUST name an owner milestone or an open question ID. |
| **WONTFIX** | Considered and deliberately rejected; the reason is recorded. |

Requirements get stable identifiers so tests, PRs and issues can cite them:
`REQ-<AREA>-<NNN>` (for example `REQ-NET-014`). Performance budgets use
`PERF-<AREA>-<NNN>`. Risks use `RISK-<NNN>`. Decisions use `DEC-<NNN>`.
Milestones use `MS-<NN>`.

## 3. Terminology

These terms are used with exactly these meanings everywhere. Do not invent
synonyms; when a new term is needed, add it here first.

### 3.1 World and space

| Term | Definition |
| --- | --- |
| **World** | A saved universe: a set of dimensions, the players' persistent data, the world seed and global game rules. Identified by a stable world UUID. |
| **Dimension** | An independently generated space with its own seed derivation, height limits, sky/ambience and generator. The vanilla-inspired set is `overworld`, `underworld` (nether-like) and `terminus` (end-like), plus arbitrary content-defined dimensions (DEC-053); shipped identifiers never use Minecraft proper nouns. |
| **Chunk** (or **chunk column**) | The horizontally aligned column of blocks at integer `(cx, cz)`, 16×16 blocks in XZ unless DEC-004 changes it. The unit of streaming, loading, saving and interest management. |
| **Section** | The vertical subdivision of a chunk and the unit of storage, editing, lighting, meshing input and network deltas. Size is 16³ (DEC-004). A chunk therefore contains `HEIGHT / 16` sections. |
| **Mesh cluster** | A 2×2×2 group of sections (32³ blocks) meshed as one render unit, and the default meshing unit from MS-04 (DEC-048). Render-only: invisible to content, save format and protocol. |
| **Block type** | A registry entry describing block behaviour and appearance (for example `openblock:stone`). Immutable during a session. |
| **Block state** | A specific configuration of a block type: type ID plus a compact set of property values (facing, waterlogged, growth stage, …). The unit stored in the world. |
| **Palette** | A per-section mapping from dense local indices to block state IDs, plus the bit-packed array of indices. |
| **Block entity** | Extra per-position data that is too large to live in the block state (chest contents, sign text, furnace progress, mod-defined data). |
| **Region** | A 32×32-chunk group, the unit of on-disk storage and (with `TICK_REGION` granularity) of scheduled ticking. |
| **Cull batch** | A group of sections culled and submitted together (8×4×8 sections in the MVP renderer). Chapter 03 historically calls these "regions"; in every other context *region* means the 32×32-chunk storage unit above. |
| **Tick** | One step of the authoritative simulation at a **fixed 20 Hz** (DEC-007, amended by DEC-038). A server may raise the *snapshot rate* to 30 Hz; the simulation tick never changes. Never tied to frame rate. |
| **Frame** | One presentation step on a client, at whatever rate the display allows. Frames interpolate between ticks. |
| **Snapshot** | An immutable, versioned view of simulation state handed from the simulation to rendering, networking or persistence. |

### 3.2 Architecture

| Term | Definition |
| --- | --- |
| **Core** | The `ob_core` crate: app lifecycle, scheduler, config, diagnostics. Knows nothing about blocks. |
| **Platform layer** | OS integration owned by `ob_core`: windows, input devices, threads, time, filesystem, GPU surface creation. |
| **System** | A scheduled unit of work with declared read/write access to state (`Stage` + ordering edges). |
| **Stage** | A deterministic ordering band inside a tick or frame (`Input`, `PreTick`, `Tick`, `PostTick`, `Prepare`, `Render`, `Present`). |
| **Service** | A long-lived object with a lifecycle that is not a world entity (renderer, network transport, audio engine, content registry). |
| **Simulation** | The authoritative, deterministic game state machine: `ob_sim`. Runs identically on a client's local prediction branch and on the dedicated server. |
| **Presentation** | Everything that turns simulation snapshots into pixels and sound. Client-only; never authoritative. |
| **Prediction** | Running the same simulation locally on the client ahead of server acknowledgement, then reconciling. |
| **Reconciliation** | Re-applying local inputs after a server correction so the client converges to authoritative state. |
| **Interest set** | The set of chunks, entities and events a given client needs at the current moment. |

### 3.3 Content and modding

| Term | Definition |
| --- | --- |
| **Content** | Anything data-defined: block types, items, recipes, loot, biomes, world features, sounds, textures, models, translations, UI screens, game rules. |
| **Content pack** | A directory or archive of declarative content plus assets that layers on top of the built-in base pack. The first-party content is itself a content pack (`base`). |
| **Mod** | Code, in addition to content, that changes behaviour: a **WASM mod** (sandboxed, portable, server-safe) or a **native plugin** (trusted, loaded from a shared library, opt-in). |
| **Resource ID** (or **ID**) | A namespaced identifier `namespace:path`, ASCII `[a-z0-9_.-]`, unique per registry. The canonical name for any content. |
| **Registry** | A frozen, ordered, indexed map of all entries of a content kind for a session. Registry order is a pure function of the loaded packs. |
| **Tag** | A named set of resource IDs, used to express group membership (`openblock:mineable/pickaxe`, `openblock:logs`) without hardcoding. |
| **Host ABI** | The stable interface the engine exposes to WASM mods: host functions, events, queries and capability grants. |
| **Capability** | A permission a mod must declare (and the server must allow) to touch a subsystem: networking, filesystem, world writes, UI. |
| **Pack hash** | `BLAKE3(protocol version, content hash, view hash)`: identifies the exact resolved content (registries, balance profile) plus asset view that a client and server must agree on. See spec/15 §3.6. |

### 3.4 Networking

| Term | Definition |
| --- | --- |
| **Host** | The authoritative simulation process: either a dedicated server or a player's integrated server. |
| **Client** | A process that renders and sends inputs; never trusted. |
| **Dedicated server** | A headless host with no GPU or window (`openblock-server`). |
| **Integrated server** | A host embedded in the client process, used for single-player and LAN. |
| **Peer** | Any connected process in a session. |
| **Session** | One server instance and its connected peers, identified by a session ID. |
| **Snapshot / delta** | Serialized authoritative state: full snapshots at join, per-tick deltas afterwards. |
| **Tick packet** | The server→client message carrying one tick's deltas for one client's interest set. |
| **Input frame** | The client→server message carrying one tick of player inputs, with sequence number. |
| **Authority** | The single source of truth for a piece of state. Authoritative state is never inferred from client messages. |

### 3.5 Rendering

| Term | Definition |
| --- | --- |
| **Renderer** | `ob_render`: owns the GPU device, resources and render graph. |
| **Render graph** | A declared pass/resource dependency graph the renderer compiles and executes each frame, handling barriers and transient resource aliasing. |
| **Pass** | One render or compute dispatch stage with declared inputs/outputs (for example `opaque-voxels`, `shadow-cascades`, `composite`). |
| **Section mesh** | GPU geometry generated from one section's visible faces, in the opaque or translucent bucket. |
| **Render set** | The per-frame collection of sections, entities and effects selected by culling and LOD. |
| **LOD** | Level of detail: reduced-detail representation for distant sections. `LOD0` is full resolution. |
| **Chunk buffer** | A GPU buffer region holding one or more section meshes, allocated from a shared arena. |
| **Material** | A shader + pipeline + parameter set combination (opaque voxel, translucent, cutout, sky, particles, UI, …). |

## 4. Units and coordinate conventions

- **Length**: one block is one metre. Internal world units are metres; there is
  no unit conversion layer.
- **Axes**: right-handed, `+X` east, `+Y` up, `+Z` south (this matches the
  intuition of Minecraft players and keeps imported community maths easy).
  Yaw 0° faces `-Z` (north), increasing yaw rotates clockwise viewed from above.
- **Angles**: radians internally, degrees only in UI and content files.
- **Time**: seconds as `f64` for presentation; integer ticks for simulation.
  Never compare floats for tick boundaries.
- **World positions**: `f64` at world scale (a 30,000,000-block world needs it),
  `i32` for block, chunk and section coordinates, `f32` locally around the
  camera. A `WorldPos` newtype MUST be used; raw `f64` triples are forbidden in
  APIs that take multiple coordinate spaces.
- **Chunk/local**: chunk coordinates are `i32` (`cx, cz`); in-chunk coordinates
  are `u8`; section coordinates are `i32` (`sx, sy, sz`).
- **Dimensions**: a chunk column is 16×16 blocks; a section is 16×16×16 blocks;
  a render-time *mesh cluster* (default from MS-04, DEC-048) is 2×2×2 sections. Height is
  `[min_y, max_y)` in blocks, multiples of 16, at most 1024; the base dimensions
  mirror the genre's overworld `-64..320` and the underworld/terminus `0..256` (DEC-004, DEC-053).
- **Render distance** is always expressed in **blocks** in engineering
  documents, with the chunk equivalent in parentheses (default 512 blocks =
  32-chunk radius). A dedicated server defaults to 256 blocks, equal to its simulation
  distance; 512 blocks is opt-in with the cost published in spec/25 (DEC-052). Never
  write "32 chunks" without saying radius or diameter.
- **Mass/velocity**: SI (kg, m/s, N), with block-collision AABBs defined in
  content data.

## 5. Identifier conventions

| Kind | Format | Example |
| --- | --- | --- |
| Resource ID | `namespace:path` | `openblock:oak_log`, `mymod:machine_casing` |
| Namespace | `[a-z0-9_.-]{1,64}`, reserved: `openblock`, `minecraft` (prohibited) | `openblock`, `ironworks` |
| Path | `[a-z0-9_.-/]{1,128}` | `stone`, `building/stone_bricks` |
| Registry entry index | `u16` dense index assigned at registry freeze | `BlockId(0)` |
| Persistent reference | Resource ID string, never numeric indices | save/network data |
| Decision | `DEC-<NNN>` | `DEC-004` |
| Requirement | `REQ-<AREA>-<NNN>` | `REQ-NET-014` |
| Performance budget | `PERF-<AREA>-<NNN>` | `PERF-RENDER-003` |
| Risk | `RISK-<NNN>` | `RISK-007` |
| Milestone | `MS-<NN>` | `MS-03` |
| Epic | `EPIC-<NNN>` | `EPIC-042` |
| ADR | `ADR-<NNNN>` | `ADR-0003` |

**Numeric indices are session-local.** Anything that survives a restart (saves,
network joins, mod handshakes) MUST use resource IDs. A mismatch between
numeric IDs on client and server is a content-hash failure, not a silent bug.

## 6. Determinism rules

The simulation is deterministic given the same inputs, in order to make client
prediction, replays, and tests possible:

1. Fixed timestep. Simulation advances only in integer ticks.
2. Seeded PRNG per subsystem (worldgen, weather, random ticks, entity AI), with
   the seed derived from `(world seed, dimension, subsystem, coordinates, tick)`
   — never from wall-clock time, thread identity or iteration order.
3. No parallel iteration over unordered maps inside a tick. Hash maps are
   `IndexMap`/`BTreeMap`/sorted arrays for anything simulation-visible.
4. Floating-point math in the simulation MUST use the same operation order on
   all platforms. Transcendental functions (`sin`, `cos`, `powf`, …) come from
   a pinned implementation (`libm`-style crate or our own) rather than the
   platform libm.
5. Randomness for presentation (particles, ambient sounds) MAY be
   non-deterministic and lives in `ob_client`/`ob_render` only.
6. Any system that cannot be made deterministic MUST NOT affect
   simulation-visible state.

## 7. Document conventions

Every specification chapter has this shape:

```markdown
# <NN> — <Title>

> Status: normative | draft | exploratory · Last updated: YYYY-MM-DD
> Depends on: DEC-xxx, docs/... · Implements: MS-xx

## 1. Purpose and scope
## 2. Requirements            <!-- table with REQ-* IDs -->
## 3. Design                  <!-- the actual design, normative -->
## 4. Data structures and formats
## 5. Performance budgets     <!-- PERF-* IDs -->
## 6. Failure modes and edge cases
## 7. Testing strategy
## 8. Open questions          <!-- each with an owner milestone -->
## 9. References
```

Rules:

- **No duplicated decisions.** State a decision once in
  [`../decisions.md`](../decisions.md), reference it by ID elsewhere.
- **Numbers, not adjectives.** "Fast" is not a requirement; "≤ 2 ms CPU frame
  time on the reference machine" is.
- **Every TBD has an owner.** A TBD without a milestone is a planning bug.
- **Diagrams are mermaid.** Source lives in the document (no binary images) so
  it reviews in diffs. Complex diagrams may also exist as `.mmd` in
  [`../diagrams/`](../diagrams/).
- **Tables for enumerations, prose for reasoning.** A wall of bullets is not a
  design.
- **Cite outside facts.** If a number comes from an external source, link it in
  the References section. If it is our own estimate, say "estimate".
- **English, present tense, RFC-2119 keywords.** The repo is international;
  write plainly and avoid idioms.

## 8. Status legend

| Status | Meaning |
| --- | --- |
| **normative** | Approved; implementations must comply. Changes need a DEC. |
| **draft for review** | Complete enough to critique; expect edits. |
| **exploratory** | Options and research, not a commitment. |
| **deprecated** | Kept for history; do not implement. |

## 9. Ownership and change control

- Each chapter has an owner area listed in [`../README.md`](../README.md).
- A change to a **normative** chapter MUST either cite an existing DEC or add
  one to the register in the same PR.
- The decision register is append-only: decisions are superseded, never
  deleted.
- Research documents (`docs/research/`) are snapshots of what we knew on their
  date; they are not normative and are not updated retroactively.

## 10. Open questions

None. This chapter is procedural.

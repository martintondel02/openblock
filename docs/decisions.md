# OpenBlock Decision Register

> Status: normative · Last updated: 2026-11-24
>
> This is the authoritative list of technical and product decisions. A
> specification chapter MUST NOT contradict a decision without superseding it
> here in the same pull request. Decisions are append-only: to change one, add
> a new `DEC-*` marked **supersedes DEC-xxx** and update the old row's status.
>
> Each decision names its owner milestone (`MS-xx`, see
> [`plan/01-milestones.md`](plan/01-milestones.md)), its reversal cost
> (how painful it is to change later) and a short rationale. Long-form
> reasoning lives in the linked ADR or research document.

## How to read this register

| Column | Meaning |
| --- | --- |
| **ID** | Stable identifier, cited by specs, code comments, tests and PRs. |
| **Status** | `settled` (implement it), `provisional` (decided, but a named benchmark may overturn it), `open` (needs a decision), `superseded`. |
| **Reversal cost** | `low` (local), `medium` (touches a few crates), `high` (save/protocol/API compatibility), `extreme` (rewrite of a subsystem). |

## 1. Foundation

### DEC-001 — Rust, edition 2024, pinned stable toolchain
**Status:** settled · **Reversal cost:** extreme · **Owner:** MS-00

All first-party code is Rust, edition 2024, built with the stable toolchain
pinned in `rust-toolchain.toml` (currently 1.100.0, `stable`). No C/C++ except
battle-tested dependencies (GPU drivers, codec libraries). No Go, C#, JS or
Python in the product; scripting is WASM or data (DEC-023).

*Why:* the brief's requirements — maximum performance with memory safety, a
solo-to-small team, a long-lived open-source project, and a server that must not
crash on malicious input — are exactly Rust's strengths. C++ would add memory
safety and build-infrastructure risk for no performance gain in this problem
domain; Go has too much GC and no mature GPU story; C# and Java require a
runtime and a separate native story for the renderer.

### DEC-002 — Purpose-built engine on wgpu; not the Bevy framework
**Status:** settled · **Reversal cost:** extreme · **Owner:** MS-00

OpenBlock owns its application loop, scheduler, renderer, world model, physics
and networking. Graphics go through **wgpu 30** (WebGPU API on Vulkan, DX12,
Metal; GLES/WebGPU later). Windowing via **winit 0.30**, math via **glam 0.34**,
jobs via **rayon**, entities via **hecs** plus purpose-built SoA storage.

*Why:* Bevy is excellent but its generic retained-scene renderer, per-material
state costs and frequent breaking releases do not solve the hard parts of a
voxel game (GPU-driven section submission, LOD, streaming, a headless server),
and adopting it imposes framework churn. The measured cost centres in wgpu are
validation and state changes, not draw counts; owning the renderer lets us batch
and cache them deliberately. See
[research/02](research/02-engine-and-render-stack.md).
*Escape hatch:* if wgpu proves too slow on a specific pass, an `ash` Vulkan
backend can be added behind the renderer's device abstraction; that is a
performance decision, not an architectural one. Revisit only with a measured
gap of >20% on the reference machine.

### DEC-003 — Crate topology with a GPU-free server
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-00

The workspace is split by responsibility and, critically, by **GPU dependency**:

- `ob_core` (app loop, scheduler, config, diagnostics, platform) — no GPU
- `ob_math` — no GPU
- `ob_world`, `ob_worldgen`, `ob_content`, `ob_sim`, `ob_protocol`, `ob_net`,
  `ob_modding` — **must not link `wgpu`, `winit` or `cpal`**
- `ob_render`, `ob_ui`, `ob_audio` — client presentation, GPU/audio allowed
- `ob_client`, `ob_server`, `ob_tools` — binaries
- `xtask` — developer automation

CI enforces this: `cargo tree -p ob_server` must not contain `wgpu`, `winit`,
`ash` or `cpal`. The dedicated server builds and runs on a machine with no GPU
and no audio device.

*Why:* the "single-player code path rots" failure mode is avoided by making the
server the same simulation library the client embeds. The dependency rule makes
that structural rather than a convention.

### DEC-004 — World units: 16×16×16 sections, 16×16 chunk columns, optional 32³ mesh clusters
**Status:** settled (section size), provisional (cluster size) · **Reversal cost:** high · **Owner:** MS-01

- **Chunk column:** 16×16 blocks in XZ. It is the unit of streaming, loading,
  saving and interest management.
- **Section:** 16×16×16 blocks. It is the unit of block storage, lighting,
  editing, meshing input and network deltas.
- **Mesh cluster:** 2×2×2 adjacent sections (32×32×32 blocks) MAY be meshed as one
  unit. Clusters are a render-side optimisation only; they are invisible to
  content, save format and protocol.
- **Height:** dimensions define `min_y` and `max_y` in blocks, multiples of 16.
  Base dimensions mirror Minecraft: overworld `-64..320` (384), nether and end
  `0..256` (256). The engine supports arbitrary heights up to 1024.

*Why:* research and prior art agree that 16³ is the right storage/edit/light
granularity: 32³ storage makes a single corner edit dirty 8× the volume, grows
light memory 8×, and makes recovery resends 8× larger, while every production
voxel renderer inspected (Minecraft, Sodium, Weft) uses 16³. A 32³ **mesh**
cluster captures the flat-build merge and draw-record savings without changing
the simulation-visible unit. See [research/01](research/01-weft-teardown.md) §2
and [research/03](research/03-voxel-rendering-sota.md) §2. The cluster decision
stays provisional until the MS-04 mesh benchmark (corner-edit latency vs draw
count).

### DEC-005 — Paletted section storage with packed 4-bit light
**Status:** settled · **Reversal cost:** high · **Owner:** MS-01

Sections store block state indices with a palette:

| Mode | Index width | Typical cost | Used when |
| --- | --- | --- | --- |
| uniform | 0 bits | 8 bytes | one block state in the section |
| packed | 4, 8 or 16 bits | 2 KiB / 4 KiB / 8 KiB | mixed content |
| palette overflow | 16 bits | up to 8 KiB + palette | >256 states (rare, modded) |

Light is stored **separately** from block data, packed at **2 bytes per
block**: 4 bits sky + 4 bits each of red, green, blue block light. Sky light is
4 bits for parity with smooth lighting; the format reserves an optional
monochrome/monochrome-sky mode for quality presets and memory pressure.

*Why:* 4-bit palettes are 500 B per 1000 blocks (2 KiB per section), and
uniform sections collapse to 8 B. Light dominates memory at 16³: storing it as
4 B/block (sky8+RGB8) costs ~415 MB at a 512-block radius, while 2 B/block costs
~208 MB. Power-of-two widths (4/8/16) give branch-free unpacking; 5/6-bit packing
was rejected. See [research/03](research/03-voxel-rendering-sota.md) §2–3.

### DEC-006 — Namespaced resource IDs; session-local numeric indices
**Status:** settled · **Reversal cost:** high · **Owner:** MS-02

Every content entry has a `namespace:path` resource ID (lowercase ASCII, `-`,
`_`, `.`, `/`). Registries freeze into dense `u16` indices for hot paths, but
**numeric indices never cross a process boundary or a restart**. Saves,
protocol messages, content hashes and mod manifests use resource IDs.

*Why:* numeric-index drift is the classic silent corruption bug in modded
voxel games. IDs are also the ecosystem contract: a mod's content must be
nameable by players and tools. See [research/06](research/06-modding-and-content-system.md) §3.

### DEC-007 — Deterministic simulation with a fixed tick
**Status:** settled · **Reversal cost:** extreme · **Owner:** MS-02

The authoritative simulation advances only in fixed ticks at
**20 Hz default** (server-configurable to 30 Hz; 64 Hz is out of scope for the
world simulation). Rendering and UI interpolate between ticks. Determinism rules
are normative in [spec/00 §6](spec/00-conventions.md#6-determinism-rules):
seeded per-subsystem RNG, ordered iteration, pinned transcendental math, no
wall-clock or thread-identity inputs to simulation state.

*Why:* client prediction and reconciliation, replays, deterministic tests and
server-side anti-cheat all require it. A 20 Hz tick matches the genre's feel and
is affordable; 30 Hz is offered for PvP servers at +50% simulation cost.
See [research/04](research/04-networking-sota.md) §2.

### DEC-008 — Server authority; clients predict, never decide
**Status:** settled · **Reversal cost:** high · **Owner:** MS-06

The host (dedicated or integrated server) is the only authority for world
state, entity state, inventories, block changes and combat. Clients send
*inputs*, never outcomes. The client runs the player movement simulation ahead
of the server for responsiveness and reconciles on correction. Gameplay-relevant
world data is never generated on the client; only visual LOD may be derived
locally from deterministic seed data.

*Why:* every public-server feature (anti-cheat, fair PvP, mod verification,
replay evidence) depends on it. Weft's client-authoritative movement is the
explicit counter-example ([research/01](research/01-weft-teardown.md) §8).

### DEC-009 — License: MIT OR Apache-2.0 code, CC BY 4.0 assets, clean-room provenance
**Status:** settled · **Reversal cost:** extreme · **Owner:** MS-00

Code is dual-licensed MIT OR Apache-2.0. Original art/audio is CC BY 4.0;
placeholders CC0; fonts OFL 1.1. Contributions are inbound=outbound under DCO
1.1; no CLA. **No Minecraft or Weft code, assets, names or data tables are
copied.** Weft (MPL-2.0) is read for concepts and measured numbers only; file-
level copyleft means any copied file would stay MPL and could not be relicensed
under our dual license.

*Why:* the Rust convention, maximum adoption (including proprietary forks and
hosting businesses), an express patent grant, and a clean story for mod authors.
See [research/07](research/07-legal-licensing-and-ip.md) and
[ADR-0008](adr/0008-license-and-provenance.md).

### DEC-010 — Specification-first, decision-registered, milestone-driven process
**Status:** settled · **Reversal cost:** low · **Owner:** MS-00

Code follows `docs/spec/`. Normative changes require a `DEC-*` entry. Every
milestone has runnable exit criteria and a demo. The base game is built on the
public content/mod APIs — no private engine hooks. Documentation and code are
reviewed in the same PR that changes behaviour.

*Why:* the project is large, partly AI-assisted and community-staffed; without a
written contract, contributors diverge and regressions are undetectable.

## 2. Rendering and presentation

### DEC-011 — Render graph with explicit pass and resource contracts
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-04

`ob_render` executes a declared render graph: passes declare reads/writes of
named resources; the graph derives barriers, transient resource aliasing and
pass ordering. Shader packs and mods hook *declared* extension points, never the
frame loop. Every pass has a documented cost budget.

*Why:* composition instead of patching is what makes "good shaders" plus
moddability plus multiplatform viable; it also gives us profiling attribution per
pass and prevents the Minecraft shader-pack fragility described in the vision.

### DEC-012 — Greedy/BGM meshing into a 16-byte vertex-pulled quad format
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-04

Section (or cluster) geometry is produced by greedy meshing, with binary greedy
meshing (BGM) as the target implementation. The vertex format is a
**16-byte-per-quad** record with position, face direction, texture layer, UV/tile
index, and 4-bit RGB + sky light per corner (vertex pulling; no per-vertex
normals). Quads are emitted into per-layer opaque and translucent buckets.

*Why:* binary greedy meshing measures 50–200 µs per 64³ chunk (74 µs average on
a Ryzen 3800X) and the 16 B/quad record is 4–5× denser than Sodium's 80 B/quad;
vertex bandwidth is irrelevant at our scale. Texture tiling and AO correctness
are handled by the UV/tile encoding rather than by forbidding greedy merging.
See [research/03](research/03-voxel-rendering-sota.md) §3.

### DEC-013 — GPU-driven visibility: CPU region culling at MVP, Hi-Z + MDI by 1.0
**Status:** provisional · **Reversal cost:** medium · **Owner:** MS-04

- **MVP (`MS-04`):** CPU culls region batches (e.g. 8×4×8-section regions) and
  clusters against the frustum; one multi-draw-indirect call per layer.
- **1.0:** GPU compute culling against the previous frame's Hi-Z depth,
  atomic compaction into per-layer indirect argument buffers, zero-instance
  arguments as the portable path (Metal loops MDI and lacks `indirect_count`).

Draw calls must stay in the low tens per frame. No per-section main-thread loop
at a 512-block render distance: the CPU budget there is ~19 ns per section.

*Why:* at 144 FPS the frame is 6.94 ms and the GPU budget is ~4 ms; visibility
and submission must not scale with section count. This mirrors the working
design evidenced in Weft and Sodium while remaining portable.

### DEC-014 — Shadows: cascaded shadow maps first, voxel/ray-traced later
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-08

Engine-owned **cascaded shadow maps** (2–3 cascades at 2048², 16-bit depth) ship
at MVP; virtual shadow maps / voxel DDA ray-traced shadows are a 1.0+ quality
tier. Shader packs receive a stable shadow-API contract (shadow matrices, atlas,
cascade selection) rather than sampling game state directly.

*Why:* CSM is the only shadow technique inside the MVP risk budget; Weft delegates
shadows to packs and detects them by substring search, which cannot support a
product promise, let alone anti-cheat.

### DEC-015 — CPU-authoritative flood-fill lighting in packed 4-bit channels
**Status:** settled · **Reversal cost:** high · **Owner:** MS-05

Sky light and coloured block light are propagated by the host on the CPU using
flood fill with incremental updates on block edits. Workers parallelise
propagation across independent regions; the network and save formats carry the
packed result. GPU lighting is presentation-only (it may not change gameplay
state) and is limited to effects such as dynamic light flares.

*Why:* lighting is gameplay-visible (spawning, crops, mob behaviour), must be
identical on the headless server, and must survive saves. CPU flood fill is the
authority everywhere; Starlight-class engineering (batching, propagation
ordering) is the optimisation path, not GPU recomputation.

### DEC-016 — Shading: PBR-lite defaults, TAA + render scale, no vendored upscalers
**Status:** settled · **Reversal cost:** low · **Owner:** MS-08

First-party pipeline: forward+ clustered shading, PBR-lite materials for
entities and items, stylised voxel shading with baked AO, GTAO, SSR water,
volumetric fog/clouds, HDR with tone mapping, and **TAA with render scale** as
the default anti-aliasing/reconstruction path. DLSS/FSR/XeSS are optional
*external* components users install; no binary-only SDK is vendored or
redistributed in this repository (their licenses forbid source redistribution and
would contaminate the MIT/Apache grant).

### DEC-017 — Texture arrays + offline KTX2/BC compression
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-04

Block and item textures are **texture arrays** (one layer per texture) with
mipmaps, anisotropic filtering and array-layer addressing, not a single atlas.
Assets are compiled offline to KTX2 with BC7/BC5 (desktop) or ASTC/ETC2 (mobile),
with an uncompressed fallback. Texture budget at 1.0: at least 4,096 layers on
desktop, 1,024 on mobile, with a documented atlas fallback.

*Why:* arrays eliminate atlas bleeding and UV gymnastics; offline compression
avoids runtime transcoding and stale `basis-universal`-class dependencies. Weft
hard-requires 65,536 binding-array elements, which would exclude integrated GPUs
and phones — we deliberately do not.

### DEC-018 — LOD is visual-only and ships after 1.0
**Status:** settled · **Reversal cost:** low · **Owner:** MS-12

At a 512-block default render distance, no LOD is required for parity; the MVP
and 1.0 ship full-detail sections out to the render distance. Beyond-view
terrain (impostor/heightfield LOD, optional GPU generation) is **visual only**:
it never feeds collision, spawning, drops, ore knowledge or anti-cheat, so a
generation mismatch can never become a gameplay bug or exploit.

*Why:* LOD becomes mandatory at 128+ chunks, not 32; and client-side generated
terrain leaks ungenerated ore to clients, as Weft's design demonstrates.

## 3. Content, modding and simulation

### DEC-019 — Registries + tags + phase-ordered packs; vanilla is pack #0
**Status:** settled · **Reversal cost:** high · **Owner:** MS-03

Content is data: registry entries with typed components
(`BlockDef`, `ItemDef`, `EntityDef`, `RecipeDef`, `LootDef`, `BiomeDef`,
`WorldgenDef`, `SoundDef`, `UiDef`, …), tags for group membership, and pack
layers with **EDIT / REPLACE / REMOVE** operations applied in a deterministic,
phase-ordered load. Namespaced dependencies (`requires`, `load_after`,
`load_before`) resolve deterministically; conflicts are errors, never "last
writer wins". The first-party game is the `openblock:base` pack, loaded through
the same pipeline as third-party packs.

*Why:* this is the one proven mechanism (evidenced in Weft) that lets mods change
vanilla-like content — ore generation, recipes, fuel values, loot — without
forking the game, and it gives us server-side content hashing for free.
See [research/06](research/06-modding-and-content-system.md) §3.

### DEC-020 — Three-tier modding: content packs, WASM mods, native plugins
**Status:** settled · **Reversal cost:** high · **Owner:** MS-09

| Tier | What it is | Trust | Distribution |
| --- | --- | --- | --- |
| **Content pack** | Declarative data + assets, optional UI screens | untrusted data, schema-validated | client and server, hot-reloadable |
| **WASM mod** | Sandboxed behaviour on a versioned host ABI (wasmtime), server-authoritative | untrusted code, capability-gated, quota-limited | server installs; clients may receive content only |
| **Native plugin** | Trusted shared library behind a tiny versioned C ABI | operator-trusted, separate process recommended | post-1.0, allowlisted, never auto-downloaded |

WASM mods receive **capabilities** (read world, write world, spawn entity, UI,
network, storage) declared in the manifest and approved by the server operator,
with per-mod CPU, memory, host-call and action quotas and per-mod cost
attribution. The ABI is versioned by schema, not by source hash. No mixin-style
binary patching, ever.

*Why:* Weft's 226-function exact-hash native ABI is fast but unsandboxed and
rebuilds the ecosystem on every ABI edit; Luanti's Lua is friendly but its own
security notes admit no process isolation. A tiered model gives modders reach
while bounding the blast radius for server owners. See
[research/06](research/06-modding-and-content-system.md) §2–4.

### DEC-021 — Player physics: f64 positions, AABB sweep, pinned transcendentals
**Status:** provisional · **Reversal cost:** high · **Owner:** MS-02

World positions are `f64` (needed for very large coordinate ranges); velocity
and collision use AABB sweeps against the voxel grid with content-defined hitbox
shapes. Simulation math avoids platform `libm` for transcendental functions
(pinned implementation in `ob_math`) and fixes operation order. Fixed-point sim
math is the fallback if the cross-platform determinism test suite in MS-02 shows
drift; the test exists specifically to make that call with data.

*Why:* fixed-point everywhere is invasive and unnecessary if IEEE-754 semantics
plus pinned functions reproduce state; but we refuse to guess — we measure.
See [research/02](research/02-engine-and-render-stack.md) §3.

### DEC-022 — Parity baseline: Minecraft Java Edition 1.21.11, Tier 2 by 1.0
**Status:** settled · **Reversal cost:** low · **Owner:** MS-14

The parity reference is **Minecraft Java Edition 1.21.11 (2025-12-09)**: 1,166
block IDs, 1,505 item IDs, 157 entity types, 65 biomes. The checklist is
generated from registry dumps and lives in the repo; "latest version" is a moving
target and is handled by an explicit delta process at each OpenBlock release.
Tier definitions (0–4) are in [vision §6](vision.md#6-scope-what-all-the-features-of-minecraft-means-here);
1.0 requires Tier 2 with ≥90% automated parity-corpus coverage. Minecraft-specific
quirks we deliberately do not copy are listed with replacements in
[the feature inventory](research/05-minecraft-feature-inventory.md) §5.

*Why:* unbounded parity is how clones die. A pinned baseline with a generated
checklist is finishable and testable; "match whatever Mojang shipped last week"
is not.

### DEC-023 — Server-authoritative inventory transactions on a reliable ordered stream
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-06

Inventory and container interactions are state-machine transactions validated by
the host: click intents reference container slots and cursor state, the host
replies with authoritative slot deltas, and the client renders only confirmed or
optimistically-shadowed state that it can roll back. Container traffic uses a
reliable ordered QUIC stream (not datagrams).

*Why:* inventory duplication is the most common multiplayer exploit in the
genre; mixing item moves with unreliable datagrams guarantees ghost items under
packet loss.

## 4. Networking and operations

### DEC-024 — QUIC (quinn) transport behind a `Transport` trait
**Status:** settled · **Reversal cost:** high · **Owner:** MS-06

Gameplay and world streaming travel over **QUIC via quinn 0.11 + rustls 0.23**:
unreliable **datagrams** for input frames and tick snapshots, independent
unidirectional **streams** for chunks, world events and container traffic, and a
bidirectional control stream. A `Transport` trait isolates the protocol from the
socket layer so LAN discovery, in-process loopback tests, iroh-based
friend-to-friend play and a later WebTransport front-end reuse the same messages.

*Why:* QUIC gives TLS 1.3, connection migration, congestion control and
multiplexing without writing a reliability layer; datagrams avoid retransmitting
obsolete state (the classic QUIC-for-games mistake), and per-chunk streams stop
one lost chunk from stalling the tick. See
[research/04](research/04-networking-sota.md) §1.

### DEC-025 — Interest-managed replication with progressive, rate-capped join
**Status:** settled · **Reversal cost:** high · **Owner:** MS-07

The host replicates per player only what that player's **interest set** contains
(chunks within simulation distance, visible/relevant entities, events), using
snapshot + delta encoding, 16³-granular block-change sets, priority scheduling
and per-player bandwidth caps. A 512-block-radius join is tens of MB of
compressed world data, so join is progressive: spawn ring first, then expanding
rings, with precompressed chunk caches, a loading UI and an optional HTTP range
download for large worlds.

*Why:* naive "send the world" joins stall both the tick and the player's uplink;
budgets and prioritisation are the difference between a playable server and an
unplayable one.

### DEC-026 — Identity: self-signed server keys by default, no central account required
**Status:** settled · **Reversal cost:** high · **Owner:** MS-07

A server generates an Ed25519 keypair on first run; clients pin the SPKI on first
connection (TOFU) and can join by a shareable ticket (`host:port#fingerprint` or
a compact ticket string). ACME/real certificates are supported for public
servers; a central OpenBlock account service (OIDC) is optional and never
required. Player identity is a keypair; bans and permissions are keyed on public
keys, not IPs. QUIC 0-RTT never authorises world-changing actions.

*Why:* self-hosters will not run a PKI, and a mandatory central account is both a
single point of failure and a privacy liability. Key-based identity also enables
peer-to-peer play and cryptographic mod/content verification later.

### DEC-027 — Protocol: bit-packed hot path, versioned messages, content-hash handshake
**Status:** settled · **Reversal cost:** high · **Owner:** MS-06

Per-tick messages are hand-written bit-packed encodings (block edits ~5 bytes,
entity deltas ~12 bytes), while control/join/metadata messages use schema-based
serialisation (postcard + versioned structs). The protocol is versioned
semantically; every connection performs a handshake exchanging protocol version,
content-pack hash set and mod manifest, and mismatches fail with an actionable
message instead of desyncing. The wire format is documented publicly so
third-party tools can be built.

*Why:* Serde struct layout is a poor wire contract for 20 Hz hot paths, and
content mismatch is the top cause of "it works on my server" modded multiplayer
bugs.

### DEC-028 — Dedicated server is a first-class product
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-07

`openblock-server` is a single static-ish binary plus a world directory, with a
TOML config, console commands, whitelist/ops/permission levels, graceful
shutdown, automatic backups, structured logs, Prometheus metrics, world
pregeneration, and an official container image. It runs with no GPU, no window
and no audio stack.

*Why:* "self-hostable servers" is a headline requirement; treating the server as
a by-product of the client is how the genre's incumbents ended up with fragile
server ecosystems.

### DEC-029 — Anti-cheat is server-side validation plus replay evidence
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-10

Every client claim is validated against host state: movement speed/acceleration/
collision/noclip, reach and line of sight, mining speed vs tool and block,
placement adjacency, inventory transactions, combat range and cooldowns, chat
rate. Violations are logged with a compact replay of the preceding seconds;
automatic action (rubber-band, kick) is configurable. Client-side anti-cheat
modules are explicitly out of scope.

*Why:* client-side anti-cheat is unworkable in an open-source game with an open
protocol; the only defensible model is "never trust the client", which the
architecture already enforces.

## 5. Client, persistence and platform

### DEC-030 — Retained, data-driven UI; no shipping UI framework dependency
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-05

Screens are data (RON) describing a retained widget tree with stable IDs, laid
out by a flex/grid engine (taffy), rendered by our own wgpu pass, with text
shaped by cosmic-text and rasterised through glyphon. egui is permitted only in
developer tools. Inventory interaction semantics are implemented once in a
shared UI layer with server-authoritative transactions (DEC-023).

*Why:* pixel-exact game UI, serialisable screens for mods, stable IDs for
accessibility, and batching all require a retained model; immediate-mode UI and
framework-specific UI crates fail at least one of those.

### DEC-031 — Audio: event-bus engine on kira/cpal/symphonia
**Status:** settled · **Reversal cost:** low · **Owner:** MS-11

Game code emits **sound events** (position, kind, parameters); a mixer service
resolves them against content-defined sound definitions, applies spatialisation,
attenuation, priority and voice limits, and streams music with crossfades.
Positional voice chat is a post-1.0, feature-gated module using Opus over a QUIC
relay, with moderation tooling.

*Why:* game code must not know about audio backends; an event bus keeps sounds
data-driven/modable and preserves the headless server's independence from audio.

### DEC-032 — Localization from day one: Fluent + BCP-47, bitmap + vector fonts
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-05

All user-facing strings are Fluent messages keyed stably
(`openblock.ui.inventory.title`), never hard-coded, including those in content
packs (which may ship translations). Locale IDs are BCP-47; plural/gender use
Fluent selectors. Fonts: a bitmap pixel atlas for the default UI font and a
dynamically rasterised vector cache for CJK/RTL/emoji; no MSDF requirement.
Accessibility: remappable input, UI scale, colour-blind palettes, subtitles/
captions, reduced motion, narrator via AccessKit/OS speech.

*Why:* retrofitting localization and accessibility after content exists is one
of the most expensive mistakes a game can make; both are cheap if designed in.

### DEC-033 — Save format: checksummed, compressed region files with migrations
**Status:** settled · **Reversal cost:** extreme · **Owner:** MS-03

Worlds are saved as **32×32-chunk region files** containing a versioned header,
checksummed chunk index, zstd-compressed section payloads and sidecar metadata
(`level.json` equivalent, player data, `ids.json` resource-ID mapping, scheduled
ticks). All writes are atomic (write-temp + fsync + rename) with a journal/
double-buffer guard; the format has an explicit version with forward migrations
from the first public build. Block/section data is stored palette-compressed;
entities and block entities use versioned structs keyed by resource IDs.

*Why:* saves are the players' most valuable asset and the hardest thing to
change later. Weft's format has no migrations and refuses newer formats; we
commit to migration tooling from the first public release.

### DEC-034 — Platforms and packaging: desktop first, headless servers everywhere
**Status:** settled · **Reversal cost:** medium · **Owner:** MS-13

1.0 platforms: Windows 10/11 x86_64, Linux x86_64 (X11+Wayland), macOS 12+
(Apple Silicon and Intel), plus headless servers on Linux/Windows/macOS
x86_64 and aarch64. Android is post-1.0 on the roadmap (Vulkan; touch UI,
safe areas and 48dp targets designed for from the start). Web/WebGPU is
aspirational. Packaging: signed GitHub Releases with delta updates, an
auto-updater with signed manifests and rollback, a Flatpak/AppImage/AUR path,
and an official container image for servers.

### DEC-035 — Performance gates in CI, budgets are normative
**Status:** settled · **Reversal cost:** low · **Owner:** MS-02

[Performance budgets](spec/25-performance-budgets.md) are normative. CI runs
benchmarks for worldgen, meshing, lighting, physics, serialisation and the frame
loop on dedicated runners (or a self-hosted bench box) and fails PRs that
regress a budget beyond its tolerance. Frame-time targets are measured at p95
and p99, not just average, with scripted fly-through/explosion/water scenes.

*Why:* "performance is a feature" is only true if regressions are caught
mechanically; Weft ships 188 unit tests that CI never runs — we deliberately do
the opposite.

### DEC-036 — Governance: DCO, maintainers, public roadmap, provenance rules
**Status:** settled · **Reversal cost:** low · **Owner:** MS-00

Contributions use DCO 1.1 (no CLA). Maintainers own areas; the roadmap and
decision register are public; ADRs record history. AI-assisted contributions are
allowed with explicit provenance rules and are held to the same review, test and
license standards. Security reports follow `SECURITY.md`; moderation follows
`CODE_OF_CONDUCT.md`.

### DEC-037 — Name and branding: OpenBlock is a working name pending clearance
**Status:** provisional · **Reversal cost:** medium · **Owner:** MS-16 (1.0 launch)

"OpenBlock" is the project's working name. It is already used by unrelated
software projects and a domain, so **a trademark clearance search is a launch
gate**; if clearance fails, the name can change before 1.0 with a documented
migration (repo org rename, domain, mod namespace reserved). In-game names avoid
Mojang trademarks: the base pack uses original names, and the README carries the
required disclaimer.

*Why:* code licensing never grants trademark rights, and renaming after 1.0 is
far more expensive than before it.

## 6. Open decisions (must close before their milestone)

| ID | Question | Owner | Deadline |
| --- | --- | --- | --- |
| OPEN-001 | Mesh clusters: 32³ clusters on or off by default after the MS-04 benchmark? | MS-04 | before save/network format freeze |
| OPEN-002 | Fixed-point vs f64 simulation math after the MS-02 cross-platform determinism test | MS-02 | before MS-06 |
| OPEN-003 | Default render distance and FPS gate: 512 blocks at 144 FPS performance mode / 120 FPS default — confirm with MS-04 benchmark | MS-04 | before 0.1 |
| OPEN-004 | Colored (RGB) block light vs monochrome: ~100 MB more at 512 blocks; decide after visual review | MS-05 | before 0.1 |
| OPEN-005 | World height: 384 default (parity) with 1024 engine ceiling — confirm content needs | MS-01 | before world format freeze |
| OPEN-006 | Whether WASM mods may ship client presentation code (vs server-only) | MS-09 | before 0.1 |
| OPEN-007 | Native plugin policy details (signing, separate process, crash containment) | MS-09 | post-1.0 |
| OPEN-008 | Whether to support WebTransport/web client at all | MS-13 | post-1.0 |

## 7. Rejected alternatives (recorded so we don't relitigate)

| Alternative | Why rejected |
| --- | --- |
| **Bevy framework** | Generic renderer doesn't solve voxel LOD/streaming/indirect draws; frequent breaking releases; custom render nodes still required. Its ECS/other crates remain usable à la carte. |
| **C++ / custom Vulkan** | No performance advantage for this workload, much higher memory-safety and build-infrastructure risk, slower iteration for a small team. |
| **Godot / Unity / Unreal** | Cannot express the required engine-level control (GPU-driven voxel pipeline, deterministic headless sim, custom replication), and licensing/runtime costs conflict with the project's open-source model. |
| **Vulkan-only (ash)** | Excludes macOS and limits contributor reach; kept as a later measured escape hatch behind the renderer abstraction. |
| **Minecraft protocol/client compatibility at launch** | Pins us to vanilla block states and chunk semantics, undermines custom content, and the only Rust implementation (Valence) has not released since 2023. Possible post-1.0 gateway. |
| **Client-side world generation** | Leaks ungenerated terrain/ore, requires bit-exact CPU/GPU/mod cross-platform generation, and makes anti-cheat impossible. |
| **32³ storage sections** | 8× edit/remesh/network cost per edit; every production renderer inspected uses 16³. Kept only as a render mesh cluster. |
| **Native-only modding (Weft-style C ABI)** | Unsandboxed, no hot reload, ecosystem-wide rebuild on ABI changes, crashes the host. Kept as a post-1.0 trusted tier. |
| **Lua/Rhai as the untrusted boundary** | No memory isolation, no fuel/fuel-equivalent metering, harder to make portable and deterministic; WASM gives sandboxing, metering and language freedom. |
| **P2P-first multiplayer** | Friend-to-friend is a feature, not the architecture: authoritative servers are required for anti-cheat, persistence, mods and 24/7 worlds. |
| **Bincode/Serde for per-tick packets** | Larger, layout-coupled wire format; hand bit-packing is ~3× smaller on the hot path. |
| **Vendored FSR/XeSS/DLSS** | Binary-only redistribution terms are incompatible with MIT/Apache-2.0; optional external installs only. |
| **32×32 chunk columns** | 4× the block count per column and no gameplay benefit; streaming/save granularity stays 16×16. |

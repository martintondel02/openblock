<p align="center">
  <h1 align="center">OpenBlock</h1>
  <p align="center"><strong>A fast, deeply moddable, server-first voxel sandbox — written in Rust.</strong></p>
</p>

<p align="center">
  <a href=".github/workflows/ci.yml"><img alt="CI" src="https://github.com/martintondel02/openblock/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE-MIT"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <img alt="Status" src="https://img.shields.io/badge/status-specification%20stage-orange.svg">
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.100%2B-B7410E.svg?logo=rust">
</p>

OpenBlock is an open-source voxel sandbox: mine, build, explore and survive
alone or with friends on servers you host yourself. It is designed around four
promises:

- **Performance by architecture** — GPU-driven rendering, paletted 16³
  sections, parallel simulation, budgets enforced in CI. Target: 512-block
  view distance at 120–144 FPS on mid-range hardware.
- **Visuals that scale** — a modern forward+ pipeline with cascaded shadows,
  AO, TAA, screen-space water, volumetric atmosphere, and a *contract-based*
  shader-pack API instead of fragile bytecode patching.
- **Moddability as a core feature** — data-driven content packs, sandboxed WASM
  behaviour mods with a versioned host ABI, and opt-in native plugins. The base
  game itself is built on the public API.
- **Server-first multiplayer** — an authoritative, headless dedicated server
  that runs on a GPU-less VPS, with self-signed identity, content-hash
  verification and progressive world streaming.

Minecraft Java Edition's feature set is the **floor**, not the ceiling. Beyond
it, OpenBlock is designed to express content and systems Minecraft cannot:
programmable logic blocks, first-class energy/fluid networks, scriptable mob
AI, procedural dungeons, server-side minigames and more.

> **Status: pre-alpha, specification stage.** There is no playable build yet.
> The repository currently contains the full specification, research and plan
> that the implementation will follow — see [`docs/`](docs/README.md). If you
> want to help build it, start with [`CONTRIBUTING.md`](CONTRIBUTING.md) and
> [`docs/plan/01-milestones.md`](docs/plan/01-milestones.md).

---

## Table of contents

- [Why another block game?](#why-another-block-game)
- [What is planned](#what-is-planned)
- [Documentation](#documentation)
- [Architecture at a glance](#architecture-at-a-glance)
- [Roadmap](#roadmap)
- [Performance targets](#performance-targets)
- [Frequently asked questions](#frequently-asked-questions)
- [Contributing](#contributing)
- [License and legal](#license-and-legal)

## Why another block game?

The genre's incumbent is fifteen years old, and the things players and server
owners complain about are structural, not cosmetic:

| Problem | OpenBlock's answer |
| --- | --- |
| FPS collapses with render distance and entity count | GPU-driven culling and submission, greedy meshing, paletted sections, budgeted streaming |
| Shader packs are fragile and mutually exclusive | Render graph with declared pass contracts; packs are composed, validated, and cannot affect gameplay |
| Mods break every version and can wreck servers | Three-tier modding with a versioned ABI, capabilities, quotas and content-hash verification |
| Servers are expensive and hard to run | Deterministic parallel simulation, headless GPU-free server, one-command container deploy, metrics and backups |
| Multiplayer UX is hostile | Self-hosted reference server, invite tickets, progressive join, no mandatory central account |

Minecraft is a masterpiece and its design is why this genre exists. We are
arguing that its implementation and extension model left room for a modern,
open, performance-first engine — not that the game is bad.

Read the full reasoning in [`docs/vision.md`](docs/vision.md).

## What is planned

Target feature set for **1.0** (Tier 2 parity, see
[`docs/vision.md`](docs/vision.md#6-scope-what-all-the-features-of-minecraft-means-here)):

- **World**: infinite deterministic terrain, biomes with climate blending,
  caves, aquifers, ores, villages and structures, nether and end dimensions,
  day/night, weather, flowing water and lava.
- **Survival**: health, hunger and XP; tools, armor and durability; crafting,
  smelting, brewing, enchanting, anvils and trading; mobs with pathfinding;
  bosses and progression; redstone and automation; farming and breeding.
- **Creative**: unlimited palette, fast block picker, structure/clipboard tools,
  undo, spectator.
- **Multiplayer**: authoritative dedicated server, LAN play, invite tickets,
  content-hash verification, interest-managed replication, prediction and
  reconciliation, anti-cheat validation, admin console and metrics.
- **Modding**: content packs (blocks, items, recipes, loot, worldgen, biomes,
  sounds, models, UI, translations), sandboxed WASM mods with capabilities,
  hot reload, per-mod profiling, and a stable API with a compatibility promise
  from 1.0.
- **Beyond Minecraft**: a platform for programmable blocks, energy/fluid
  networks, scriptable AI, procedural dungeons, per-biome shaders and ambience,
  and server-side minigame scripting.

The specification is explicit about what is **not** planned: no Minecraft
client/protocol compatibility at launch, no Minecraft assets, no paywall or
marketplace, no blockchain. See the
[non-goals](docs/vision.md#7-non-goals).

## Documentation

The repository is documentation-first: code follows the spec, not the other way
around.

| Start here | |
| --- | --- |
| [Vision](docs/vision.md) | The product, pillars, scope tiers, success metrics, non-goals |
| [Decision register](docs/decisions.md) | Every binding technical decision (`DEC-xxx`) with rationale |
| [Specification](docs/spec/00-conventions.md) | Terminology, then 25 chapters from architecture to performance budgets |
| [Roadmap](docs/plan/00-roadmap.md) | Phases, releases and the effort model |
| [Milestones](docs/plan/01-milestones.md) | `MS-00 … MS-16` with testable exit criteria |
| [Research](docs/research/01-weft-teardown.md) | Prior-art teardowns and technology surveys used to make the decisions |

Full document map: [`docs/README.md`](docs/README.md).

## Architecture at a glance

```
Processes:  openblock (client + integrated server)   openblock-server (headless)
            openblock-tools (asset/pack/world tools) xtask (build automation)

Crates:     ob_core   app loop, scheduler, config, diagnostics, platform
            ob_math   coordinates, AABBs, deterministic RNG, pinned math
            ob_world  paletted 16³ sections, chunks, light, world queries
            ob_worldgen  deterministic parallel terrain, biomes, structures
            ob_content   registries, tags, content packs, validation
            ob_sim    authoritative simulation: entities, physics, gameplay
            ob_protocol  versioned bit-packed wire protocol
            ob_net    QUIC transport, replication, interest management
            ob_modding  content-pack runtime, WASM host, native ABI
            ob_render  wgpu 30 renderer: render graph, voxel pipeline, post
            ob_ui / ob_audio  retained UI, text, localization; sound engine
            ob_client / ob_server / ob_tools  binaries

Key decisions: Rust + wgpu 30 (not Bevy), 16×16×16 paletted sections with
packed 4-bit light, GPU-driven multi-draw-indirect submission, 20 Hz fixed tick,
server-authoritative QUIC multiplayer, MIT OR Apache-2.0.
```

- [`spec/01-technical-architecture.md`](docs/spec/01-technical-architecture.md)
- [`ADR-0002 — Rust and a purpose-built engine`](docs/adr/0002-rust-and-purpose-built-engine.md)
- [`ADR-0003 — wgpu-first renderer`](docs/adr/0003-wgpu-first-renderer.md)
- [`ADR-0005 — Server-authoritative QUIC multiplayer`](docs/adr/0005-server-authoritative-quic.md)

## Roadmap

| Phase | Releases | What you can do |
| --- | --- | --- |
| A | `0.0.1` | Walk around a generated world, break and place blocks, save and reload |
| B | `0.1.0-alpha` | Play at 512 blocks of view with a real UI, inventory and content packs |
| C | `0.2.0-alpha` | Play with friends on your own headless server |
| D | `0.3.0-beta` | Survive: biomes, caves, structures, dimensions, crafting, hunger, combat |
| E | `0.4`–`0.5` | Mobs, redstone, enchanting, bosses; WASM mods; shadows and audio |
| F | `0.6`, `1.0-rc` | Tier-2 parity, 50-player servers, anti-cheat, accessibility |
| G | `1.0` | Signed installers, updater, docs, mod API 1.0 freeze, launch |

Nominal effort to 1.0 is ~160 engineering dev-weeks plus content and QA — about
15–18 months for a three-person team, or 2.5–3 years solo. The plan and its
assumptions are public: [`docs/plan/00-roadmap.md`](docs/plan/00-roadmap.md).

## Performance targets

Measured on the reference machine (8-core CPU, 32 GiB RAM, RTX 3060-class GPU,
NVMe, 1080p) and enforced by benchmarks in CI:

| Metric | Target |
| --- | --- |
| 512-block render distance (32 chunks), default quality | ≥ 120 FPS average, 1% low ≥ 90 FPS |
| 512 blocks, performance mode | ≥ 144 FPS verified |
| 1024-block render distance | ≥ 60 FPS |
| CPU main-thread frame time | ≤ 2.0 ms |
| Client memory at 512 blocks | ≤ 4 GiB |
| Join time, pregenerated world | ≤ 5 s to playable |
| Worldgen throughput (8 threads) | ≥ 250 columns/s |
| Dedicated server | 50 players at 20 TPS on 4 vCPU / 8 GiB |

Budgets and their enforcement: [`spec/25-performance-budgets.md`](docs/spec/25-performance-budgets.md).

## Frequently asked questions

**Is this a Minecraft clone?**
It is a voxel sandbox in the Minecraft genre, aiming for feature parity as a
baseline and going beyond it. It contains no Minecraft code, assets, names or
data, and is not affiliated with Mojang or Microsoft.

**Can I use my Minecraft worlds or play with vanilla clients?**
Not planned for launch. World import and an optional compatibility gateway are
post-1.0 community-style projects, never core features.

**Why Rust?**
Memory safety under adversarial input (network, saves, mods), no GC pauses in
the 20 Hz simulation, and native-speed GPU/CPU work with a strong crate
ecosystem. The alternatives are analysed in
[`ADR-0002`](docs/adr/0002-rust-and-purpose-built-engine.md).

**Why not use Bevy?**
Bevy is excellent, but its renderer does not solve voxel streaming, LOD,
visibility or GPU-driven submission, and its release cadence would add migration
cost. We use a purpose-built engine on wgpu; see
[`research/02`](docs/research/02-engine-and-render-stack.md).

**When can I play?**
The first playable tech preview (`0.0.1`) is the `MS-03` milestone. Until then
the repository is the specification and plan.

**Is it free? Can I run a server commercially?**
Yes. Code is MIT OR Apache-2.0; self-hosting and commercial hosting are allowed.
There will be no paywall, marketplace cut or required central account.

**Can I contribute if I use AI tools?**
Yes, under the same review, test and provenance standards as any contribution —
see [`CONTRIBUTING.md`](CONTRIBUTING.md) and
[`plan/04-team-and-process.md`](docs/plan/04-team-and-process.md).

## Contributing

OpenBlock is at the stage where **specification and design contributions are as
valuable as code**. Good entry points:

- Review a spec chapter and open an issue with questions or contradictions.
- Take a `good first issue` from the milestone board.
- Improve the research documents with better evidence.
- Write a content pack template or an example mod once the API exists.

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
[spec conventions](docs/spec/00-conventions.md) first. Participation is covered
by our [Code of Conduct](CODE_OF_CONDUCT.md).

## License and legal

- **Code**: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
- **Original assets**: CC BY 4.0 (see [`docs/research/07-legal-licensing-and-ip.md`](docs/research/07-legal-licensing-and-ip.md)).
- **Contributions**: inbound = outbound under DCO 1.1; no CLA.

OpenBlock is not an official Minecraft product. It is not approved by or
associated with Mojang Studios or Microsoft. OpenBlock contains no Minecraft
code or assets, and does not read Minecraft worlds.

The project is inspired by prior art, in particular the Rust voxel engine
[Weft](https://github.com/Weft-Rust/Weft) (MPL-2.0), which is studied for ideas
and measured numbers only — no Weft code is copied. See
[`ADR-0008`](docs/adr/0008-license-and-provenance.md) for our provenance rules.

"OpenBlock" is a working project name pending trademark clearance
(DEC-037).

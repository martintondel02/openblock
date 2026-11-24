# OpenBlock — Vision

> Status: draft for review · Last updated: 2026-11-24
> This document defines *what* we are building and *for whom*. The technical
> "how" lives in [`spec/`](spec/00-conventions.md); the "when" lives in
> [`plan/`](plan/00-roadmap.md).

## 1. In one paragraph

OpenBlock is an open-source voxel sandbox game: mine, build, explore, survive,
and play together on servers you can host yourself. It is written from scratch
in Rust with a purpose-built engine, designed around four promises: **frames
that stay fast as the world grows**, **graphics that scale from a laptop to a
ray-traced showcase**, **modding that is a first-class part of the engine
instead of a pile of hooks**, and **multiplayer that is server-first, cheap to
host and pleasant to administrate**. It targets the full Minecraft Java Edition
feature set in two stages: **Tier-1 parity at 1.0 (Early Access)**, then **full
Tier-2 parity at 2.0**, and then adds content and systems Minecraft cannot
express. It contains no Minecraft or Weft code or assets.

The short version for the README:

> **OpenBlock** — a fast, deeply moddable, server-first voxel sandbox in Rust.

## 2. Why build another block game?

Because the incumbent is 15 years old and its constraints are structural, not
cosmetic. The gap is not "Minecraft but in Rust" — plenty of clones exist. The
gap is a game that keeps Minecraft's readability and survival loop while
removing the three things that players and server owners actually complain
about.

| Problem today | Why it exists | What OpenBlock does instead |
| --- | --- | --- |
| FPS collapses with render distance and entity count | Single-thread-heavy simulation, per-chunk draw calls, no LOD, meshing and lighting on limited worker threads | GPU-driven rendering with LOD, paletted sections, bindless/multi-draw, a job system that scales with cores, budgets enforced in CI |
| Shader packs are fragile and mutually exclusive | Rendering pipeline is patched, not composed | Render graph with explicit pass contracts; shader packs are content, composed through declared hooks; first-party lighting is modern (CSM/VSM-class shadows, GTAO, TAA, SSR water, volumetric atmosphere) |
| Mods break every version and can wreck servers | Bytecode patching, no stable API, no sandboxing | Three-tier modding: declarative content packs, sandboxed WASM behaviour mods with a versioned host ABI, and opt-in native plugins; server-side mod verification and per-mod performance budgets |
| Servers are expensive and hard to run | Simulation can't use multiple cores, tick loops touch everything | Parallel region ticking, deterministic systems, headless server with no GPU dependency, one-command Docker/systemd deployment, metrics and admin tooling |
| Multiplayer UX is hostile | Port forwarding, opaque server lists, no integrated lobby | Self-hostable dedicated server plus host-a-world-invite-friends flows; signed, versioned protocol; content-hash handshake so modded clients and servers can't silently desync |

We are not claiming Minecraft is bad. It is the most successful sandbox ever
made, and its design is the reason this genre exists. We are claiming that its
*implementation* and *extension model* left a gap that a modern, open,
performance-first engine can fill.

## 3. Product pillars

These are ordered. When two pillars conflict, the earlier one wins.

### P1 — Performance is a feature

Frame time and tick time are budgeted, measured and regression-gated. "It runs"
is not acceptance; "it runs at the target on the reference machine with the
budget headroom documented" is. Performance work is designed in, not optimised
in later: data layout first, parallelism by default, GPU-driven where it pays.

### P2 — Visuals that scale with hardware

The default look is clean and readable, but the renderer is built for
modern techniques: cascaded/virtual shadows, ambient occlusion, temporal
anti-aliasing, screen-space water and reflections, volumetric fog and clouds,
HDR and tone mapping, and an optional ray-traced path. Shader packs are
composable contracts, not file replacements, and must not be able to break
gameplay or cheat in multiplayer (visual-only authority rules).

### P3 — Server-first multiplayer

The authoritative simulation is a library that runs identically in
single-player, in a LAN host, and in a headless dedicated server. There is no
"single-player code path" that rots. Predictable bandwidth, interest-managed
replication, cheap hostings, first-class admin tooling, and a protocol that is
documented and versioned so third-party tools can exist.

### P4 — Moddable to the core

The first-party game is itself content built on the public content and mod
APIs. There is no "engine-private" path that only the base game may use. Content
packs and mods are loadable at runtime, hot-reloadable in development,
distributable, signable, and safe to run on a server. The API is versioned with
an explicit deprecation policy, not "whatever the internals are this month".

### P5 — Beyond Minecraft

Minecraft parity is the floor. The ceiling is content the vanilla game cannot
express: programmable blocks and computers, first-class energy/fluid/heat
networks, fully data-driven mob AI with scripting, procedural dungeon and
structure APIs, climate and ecology simulation, per-biome shader and ambience
packs, server-side minigame/plugin scripting, and cross-dimension logistics.
These are designed as platform features, not one-off mechanics.

### P6 — Determinism and observability

The simulation is deterministic given inputs. That makes client prediction,
replays, automated tests, golden-world comparisons and bug reports tractable.
Every tick can be recorded, inspected and replayed; every system can be
attributed a cost in a profiler; every network message can be captured and
decoded by a third-party tool.

### P7 — Open, owned by its community

MIT OR Apache-2.0, DCO, public roadmap, public design process. No open-core
paywall, no marketplace cut, no telemetry by default. Self-hosting is a
first-class distribution path, not a tolerated loophole.

## 4. What playing it feels like

**Survive.** Wake up in a generated world, punch trees, build a shelter before
night, descend for ores, smelt, craft, enchant, farm, fight, and reach the
underworld and, at 2.0, the terminus — with the same verbs and pacing as the
game you already know, but at 144 FPS and with fewer "wait, why did that
happen?" moments.

**Build.** Creative mode with an infinite palette, a fast block picker,
blueprint and structure tools, undo, and a selection/clipboard system that a
modern editor would be embarrassed not to have. Build large things without
worrying about the engine falling over.

**Play together.** Invite a friend into your single-player world with one
click. Or rent a $5/month VPS, run `docker compose up`, and host 50
**co-located** players at 20 TPS — the published distributed-player row is
lower and states its own configuration (DEC-052). Or join a public server
whose mods are verified automatically before you connect.

**Make it yours.** Change textures, sounds, lighting and UI with a content
pack. Write a WASM mod in Rust, Lua or TypeScript that adds a machine, a mob,
a dimension, or a whole minigame, and run it on the server without anyone
installing a client mod — because the client can load the server-provided
content packs when it trusts the server.

## 5. Who it is for

| Audience | What they need from us |
| --- | --- |
| **Players** | A game that feels like the genre they love, runs fast, and doesn't crash |
| **Server owners** | Cheap, stable hosting; clear config; moderation and anti-cheat; backups and metrics |
| **Modders** | A documented, stable, sandboxed API; fast iteration; distribution and tooling |
| **Content creators** | Data-driven packs, shader and resource packs, map/structure sharing |
| **Contributors** | A spec they can implement against, tests that tell them the truth, and a codebase that is fun to work in |
| **Educators and researchers** | A deterministic, inspectable simulation and a scriptable environment |

## 6. Scope: what "all the features of Minecraft" means here

Full parity with modern Minecraft is a multi-year effort even for a funded
team. We refuse to lie about that, so parity is defined in tiers, each with an
explicit exit criterion and an honest release.

The reference is **Minecraft Java Edition 1.21.11** (the last version with a
high-confidence registry baseline; newer drops are handled as a documented delta
process — see [DEC-022](decisions.md) and
[the feature inventory](research/05-minecraft-feature-inventory.md)).
Bedrock-only mechanics are out of scope unless they are strictly better.

| Tier | Meaning | Release |
| --- | --- | --- |
| **Tier 0 — Core** | World, blocks, player movement, mining/placing, inventory basics, creative mode, save/load, single-player and LAN | `0.0.1`–`0.1` (MS-03, MS-04, MS-05) |
| **Tier 1 — Parity essentials** | Crafting/furnaces, tools and tiers, health/hunger/XP, common mobs, biomes, caves, structures, day/night, boats/minecarts, basic redstone, dimensions and portals, enchanting basics | `0.3`–`0.6`, then **1.0 Early Access** (MS-08, MS-10, MS-11, MS-13, MS-14) |
| **Tier 2 — Full parity** | All vanilla blocks/items/mobs/biomes/structures, full redstone components, villagers and trading, bosses, raids, potions, advancements/statistics, commands and game rules, full server properties | **2.0** (MS-17) |
| **Tier 3 — Deep parity and polish** | Spectator, full datapack semantics, edge-case mechanics parity, accessibility, controller support, full localization | post-2.0 by default; the items already scheduled earlier ship earlier — controller at `0.5` (MS-12), spectator, full localization and adopted datapack semantics at `0.6`/1.0 (MS-14) |
| **Tier 4 — Deliberately different** | Quirks that exist for historical reasons or hurt players. Documented individually with our replacement. | ongoing |

"All the features of Minecraft" therefore means **Tier 1 at 1.0 Early Access**
and **Tier 2 at 2.0**, verified by the parity gate from
[DEC-051](decisions.md): (a) **100% of in-scope checks pass** and (b) **≥ 85%
vanilla coverage** measured by a hand-authored behavioural checklist. At 1.0,
(a) is measured over the Tier-0/1 checklist while (b) is measured over the full
pinned baseline; at 2.0 both are measured over the full Tier-2 baseline.
Coverage and pass rate are separate numbers, never averaged; the checklist counts
behaviour families, not registry IDs, so 85% coverage at 1.0 is not a claim that
every Tier-2 block, mob and command exists. Registry dumps are a cross-check,
never the corpus. Anything we choose not to copy is listed in
[the feature inventory](research/05-minecraft-feature-inventory.md) with a
replacement or a reason.

## 7. Non-goals

Explicitly **not** in scope; adding any of these requires a new decision:

1. **Minecraft client or protocol compatibility** as a launch feature. We do
   not read Minecraft worlds or serve vanilla clients. (A compatibility
   gateway is a possible post-1.0 community project, not a core feature.)
2. **Minecraft assets.** No Mojang textures, sounds, fonts, or names. Our
   palette and names are original, with community packs able to fill the
   familiarity gap.
3. **Bedrock parity / mobile-first design** at launch. Android is a later
   milestone; phones are supported once the renderer and UI are ready.
4. **Web/WebGPU client** at launch. The protocol and engine are designed to
   keep the door open; it is not a 1.0 deliverable.
5. **A marketplace with revenue share**, microtransactions, loot boxes,
   crypto/NFTs, or any blockchain integration.
6. **Hosted-service lock-in.** There will be no "official servers are better"
   paywall; the self-hosted server is the reference implementation.
7. **Photorealism at the cost of readability.** Voxel identity and gameplay
   clarity beat realism when they conflict.
8. **Mods that can run unsandboxed on servers by default.** Native plugins are
   opt-in for operators who accept the risk.

## 8. Success metrics

We will judge ourselves on measurable outcomes, not feature count. Numbers are
targets for the **1.0 Early Access** milestone unless labelled post-1.0, and are
re-validated at each release (see
[Performance budgets](spec/25-performance-budgets.md) for the authoritative
budget table). Full Tier-2 parity is a 2.0 target, not a 1.0 one
([DEC-051](decisions.md)).

| Area | Metric | Target at 1.0 |
| --- | --- | --- |
| **Performance** | Sustained FPS, 1080p, 512-block render radius (32 chunks), default quality, reference machine | ≥ 120 FPS average, 1% low ≥ 90 FPS; 144 FPS as a verified performance mode |
| | CPU main-thread frame time (excludes GPU) | ≤ 2.0 ms |
| | Client memory at 512 blocks | ≤ 4 GiB RSS |
| | Join time, pregenerated world, warm cache | ≤ 5 s to playable |
| | Chunk generation (terrain + features), 8 worker threads | ≥ 250 columns/s |
| **Server** | Players at 20 TPS, 256-block render and simulation distance, 4 vCPU / 8 GiB VPS — **co-located** (shared column cache, same region) | 50 players |
| | Players at 20 TPS, **distributed** (no shared columns; the published configuration in [spec/25 §3.6](spec/25-performance-budgets.md#36-server-capacity-model) is 50 players at 64-block render/simulation on 8 vCPU) | Separate, lower published row measured at MS-15 |
| | Cost per player-hour on commodity VPS, co-located reference configuration | ≤ $0.02 |
| | Tick time p99 with 2,000 loaded chunks and 50 co-located players | ≤ 35 ms |
| | Server memory | ≤ 2 GiB + 20 MiB/player |
| **Modding** | Time from `git clone` of the mod template to seeing a new block in-game | ≤ 10 min |
| | Mods that crash the host process (WASM tier) | 0 |
| **Quality** | Crash-free sessions, measured only from **opt-in** crash signatures (no default telemetry) | ≥ 99.5% of reporting sessions; tracked, not a 1.0 gate |
| | In-scope parity checks passing (Tier 0/1 checklist at 1.0, Tier 2 at 2.0) | 100% |
| | Vanilla coverage, hand-authored behavioural checklist | ≥ 85% of pinned-baseline vanilla behaviours at 1.0; the same floor applies at 2.0 |
| **Community** | Independent public servers running for 30+ days | ≥ 100 — **aspiration**, counted from the opt-in public server directory; baseline published after 1.0 |
| | Published content packs / mods | ≥ 200 — **aspiration**, counted from the opt-in content index; baseline after 1.0 |

Sustained 1,024-block rendering is an `MS-20` post-1.0 target, not a 1.0 metric
([DEC-018](decisions.md), [DEC-052](decisions.md)). Community numbers are
aspirations until a census mechanism exists; the project ships no telemetry by
default (P7), so they cannot be claimed as measured at launch.

## 9. Platforms and hardware

| Tier | Target | Notes |
| --- | --- | --- |
| **Primary — launch** | **Fedora Linux 44/45 x86_64** (Wayland session, Vulkan, PipeWire); Flatpak first, RPM/COPR native, AppImage/tarball fallbacks | The reference platform for performance, CI, packaging and support (DEC-054) |
| **Supported secondary — launch** | Windows 10/11 x86_64, macOS 12+ universal2 (Apple Silicon and Intel) | Same 1.0 feature set; DX12/Metal; not the reference for performance gates |
| **Best effort** | Other Linux distributions via Flatpak/AppImage; Linux aarch64 build | No distro-specific packaging promise at launch |
| **Later** | Android (Vulkan), Steam Deck (verified), iOS (feasibility) | Touch UI and mobile budgets |
| **Aspirational** | WebGPU in browser | Single-player and small servers |
| **Dedicated server — primary** | **Fedora Server 44/45 x86_64**, RPM + hardened systemd unit + firewalld service; container image | Headless, **no GPU required**, from day one |
| **Dedicated server — secondary** | Linux aarch64, Windows, macOS | Same binary, best-effort packaging |

**Reference machine** for all budgets: **Fedora Workstation 45 (Wayland, Vulkan)**, 8-core
CPU (Ryzen 7 5800X3D / i7-12700 class), 32 GiB RAM, RTX 3060 12 GiB / RX 6600 XT
class GPU, NVMe SSD, 1080p display. **Minimum spec** at 1.0: Fedora 44+
(Wayland or X11), 4-core CPU, 8 GiB RAM, GTX 1060 / RX 580 class GPU with a
Vulkan-capable driver, 720p, 8-chunk render distance, ≥ 60 FPS.

## 10. How we compare

| | Minecraft Java | Luanti (Minetest) | Weft | Veloren | **OpenBlock** |
| --- | --- | --- | --- | --- | --- |
| Language / engine | Java, custom | C++ / Irrlicht | Rust, custom | Rust, custom | **Rust, custom** |
| Rendering | Legacy pipeline, fragile shader packs | Older pipeline, limited shaders | Modern Vulkan/wgpu, GPU worldgen | Modern wgpu | **GPU-driven, shader packs, LOD** |
| Server model | Authoritative, mods required client-side | Authoritative, Lua mods both sides | P2P-first + dedicated | Authoritative MMO | **Server-first, WASM/pack mods** |
| Modding | Bytecode patching, breaks per version | Lua, stable, unsafe-ish | Native C ABI | Rust plugins | **Data packs + WASM + native, versioned, sandboxed** |
| Parity target | Itself | Own design | Minecraft-like subset | Own RPG design | **MC Java 1.21 as floor, more above** |
| License | Proprietary | LGPL/CC | MPL-2.0 | GPL | **MIT OR Apache-2.0** |

Weft is the closest prior art and a strong technical reference; we say so
openly and link to it. OpenBlock differs in scope (parity + custom content),
process (open specification, community governance), and target (servers as a
product, not a side mode).

## 11. Engineering principles

1. **Measure, then decide.** A performance claim without a benchmark is a
   wish. Every hot path has a benchmark before and after.
2. **Data-oriented, not object-oriented.** Flat arrays, indices, palettes,
   SoA where it matters; pointer-chasing is a last resort.
3. **Own the hot path; buy the rest.** Hand-written world, meshing, lighting,
   replication and render graph; battle-tested crates for TLS, compression,
   windowing, serialization.
4. **Deterministic simulation, non-deterministic presentation.** The world
   tick is testable and replayable; particles and audio are free to vary.
5. **Server authority is non-negotiable.** The client is an input device with a
   very good renderer. Every client claim is validated.
6. **No hidden global state.** Systems receive what they need; services have
   explicit lifecycles; tests can build a world with two players and no window.
7. **Specify what you build, test what you specify.** A requirement without a
   test is documentation debt.
8. **Small vertical slices.** Every milestone ends in something you can run,
   measure and demo — never six months of framework with nothing to show.
9. **Boring where possible.** Exotic choices must pay rent in latency, safety
   or performance, and must be reversible if they don't.
10. **The player is the reviewer.** Design documents are judged by whether the
    resulting game is fun, not by their elegance.

## 12. Naming and identity

- **Name:** OpenBlock. Chosen because it is descriptive, unencumbered in the
  game space as far as our trademark screening shows, and reads the same in
  most languages.
- **Brand boundary:** "OpenBlock" is not "Minecraft". The project does not use
  Mojang names for its content in the base pack: the dimensions are
  `openblock:underworld` (the nether-like dimension) and `openblock:terminus`
  (the end-like dimension), and entities are named originally — our iconic
  exploding mob is the *Blaster* ([DEC-053](decisions.md)). Players can rename
  content in content packs, and familiarity is preserved through mechanics, not
  trademarks.
- **Legal:** see [`research/07-legal-licensing-and-ip.md`](research/07-legal-licensing-and-ip.md)
  and [`adr/0008-license-and-provenance.md`](adr/0008-license-and-provenance.md).
  The README carries the disclaimer; there is no affiliation with Mojang or
  Microsoft, and no Minecraft code or assets are used.

## 13. What must be true for this to succeed

Honest pre-mortem. If any of these fail, the project stalls:

1. **A playable vertical slice by MS-03.** If the world isn't fun to walk
   around in within the first milestone months, nothing else matters.
2. **Performance targets met *while* building features.** Optimising later is
   the standard way this genre of project dies.
3. **A modding API that modders actually adopt.** Ship the template, docs and
   examples with the first release; dogfood by building the base game as
   content.
4. **Server hosting that "just works".** One command, a compose file, a
   backup story, and a crash-free tick loop.
5. **A community that can see progress.** Public roadmap, playable builds early,
   honest communication about parity tiers.
6. **A contribution pipeline that survives AI-generated volume.** Clear
   provenance rules, review standards and CI gates (see
   [Team and process](plan/04-team-and-process.md)).

## 14. Elevator pitch (for store pages and the README)

> **OpenBlock** is an open-source voxel sandbox built in Rust. Mine, build,
> explore and survive alone or with friends on servers you host yourself.
> It brings Minecraft Java's Tier-1 feature set at 1.0 Early Access, targets
> full Tier-2 parity at 2.0, and goes further with a scriptable content
> platform, modern GPU-driven rendering, and a multiplayer stack designed for
> people who run servers for fun.
>
> Fast by architecture, open by license, moddable by design.

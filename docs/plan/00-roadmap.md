# Roadmap — from empty repository to 1.0

> Status: normative planning · Last updated: 2026-11-24
> Depends on: [vision](../vision.md), [decision register](../decisions.md)
> Companion: [Milestones](01-milestones.md) (exit criteria), [MVP definition](02-mvp.md)

## 1. How to read this roadmap

- The unit of progress is a **milestone** (`MS-xx`), not a date. Each milestone
  ends in something runnable, demonstrable and measurable.
- Effort is expressed in **dev-weeks** (one experienced, full-time engineer).
  AI-assisted development improves some of this (see §11) but is not assumed for
  hard debugging, driver work or game feel.
- **Dependencies are real.** The milestone graph below is the critical path; the
  numbering is the recommended build order, not a strict sequence.
- **Performance gates are part of every milestone**, not a final phase (DEC-035,
  [spec/25](../spec/25-performance-budgets.md)).
- Public releases are cut from milestones that pass their exit criteria. There
  are no "release engineering weeks" outside `MS-16`.

```mermaid
graph LR
  M00[MS-00<br/>Foundation] --> M01[MS-01<br/>World core]
  M00 --> M02[MS-02<br/>Engine core]
  M01 --> M03[MS-03<br/>First playable]
  M02 --> M03
  M03 --> M04[MS-04<br/>GPU rendering]
  M03 --> M05[MS-05<br/>Content + UI]
  M03 --> M13[MS-13<br/>Persistence]
  M04 --> M06[MS-06<br/>Networking]
  M05 --> M06
  M06 --> M07[MS-07<br/>Dedicated server]
  M02 --> M08[MS-08<br/>Worldgen]
  M05 --> M08
  M07 --> M09[MS-09<br/>Modding]
  M05 --> M09
  M08 --> M10[MS-10<br/>Survival loop]
  M10 --> M11[MS-11<br/>Depth: mobs/redstone/...]
  M04 --> M12[MS-12<br/>Visuals + audio]
  M11 --> M14[MS-14<br/>Parity + polish]
  M12 --> M14
  M09 --> M14
  M07 --> M15[MS-15<br/>Scale + anti-cheat]
  M14 --> M16[MS-16<br/>1.0 launch]
  M15 --> M16
```

## 2. Release philosophy

1. **Nothing stays unplayable for long.** From `MS-03` onward, `main` always
   builds into a game you can launch and walk around in. If a refactor breaks
   that, it lands behind a flag or not at all.
2. **Release early, release often, be explicit about state.** Every public
   release says which parity tier it covers and what is missing.
3. **Saves are sacred from the first public build.** Once `0.1-alpha` ships, a
   save opened by a newer version upgrades automatically and never silently
   corrupts (DEC-033). Pre-0.1 saves may break; the format is documented anyway.
3. **The protocol is documented from the first multiplayer release.** Third-party
   tools, bots and server implementations are a feature (DEC-027).
4. **Semver for crates and the protocol; marketing versions for releases.**
   `0.x` may break APIs; `1.0` freezes the save format (with forward migrations),
   the mod host ABI (DEC-020) and the documented protocol.
5. **Every release has a demo video and a changelog.** Progress must be visible
   to players and contributors.

## 3. Phases at a glance

| Phase | Milestones | Release | Player-visible outcome | Engineering effort |
| --- | --- | --- | --- | --- |
| **A — Foundation and first playable** | MS-00, MS-01, MS-02, MS-03 | `0.0.1` tech preview | A window, a generated world, a blocky person, mine and place | 19 dev-weeks |
| **B — Engine maturity and content platform** | MS-04, MS-05 | `0.1.0-alpha` | 512-block view at high frame rates, inventory, HUD, content packs | 18 |
| **C — Multiplayer and servers** | MS-06, MS-07 | `0.2.0-alpha` | Invite a friend or join a self-hosted server; 20 TPS server binary | 17 |
| **D — World and survival** | MS-08, MS-10, MS-13 | `0.3.0-beta` | Biomes, caves, structures, dimensions; health/hunger/crafting/furnaces | 31 |
| **E — Depth and ecosystem** | MS-09, MS-11, MS-12 | `0.4.0-beta`, `0.5.0-beta` | Mobs, redstone, enchanting/trading/bosses; WASM mods; shadows and audio | 44 |
| **F — Parity, scale and polish** | MS-14, MS-15 | `0.6.0-beta`, `1.0.0-rc` | Tier-2 parity corpus green; 50-player servers; anti-cheat; accessibility | 22 |
| **G — Launch** | MS-16 | `1.0.0` | Storefront builds, updater, docs, launch | 8 |
| | | | **Total to 1.0** | **≈ 160 dev-weeks (nominal)** + content/QA |

## 4. Phase A — Foundation and first playable (`0.0.1`)

**Goal:** prove the architecture end-to-end with the smallest possible vertical
slice: a real world, real chunks, real meshing, real physics, real save file —
not a tech demo with fake data.

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-00 Foundation** | Workspace, CI gates, conventions, decision register, benchmark harness skeleton, asset/license policy, `hello window` through wgpu, `hello headless server` process | 1 |
| **MS-01 World core** | Paletted 16³ sections, chunk columns, dimension height model, world storage, block changes, light storage, deterministic RNG, serialization round-trip tests | 4 |
| **MS-02 Engine core** | App loop, stages, fixed tick, scheduler, job system, platform abstraction, config, diagnostics/profiler, determinism test harness, `hecs` entity store | 4 |
| **MS-03 First playable** | `openblock` window, wgpu device, naive mesher → greedy mesher, chunk streaming around the player, first-person controller with AABB voxel physics, break/place with raycast, minimal content registry, integrated server, save/load, HUD crosshair/FPS | 10 |

**Phase A exit criteria**

- Launch `openblock`, spawn in a deterministic test world, walk, jump, fall,
  collide, break and place blocks for ten minutes without a crash.
- A 256-block view distance holds 60 FPS on a mid-range GPU with the naive
  pipeline (this is the baseline the next phase must beat by 4×).
- Save, quit, relaunch, and every block and inventory slot is exactly as left.
- The headless test harness can run 10,000 ticks of the same world and produce a
  bit-identical state hash on Linux and Windows CI runners.
- CI enforces fmt, clippy `-D warnings`, tests, cargo-deny, shader validation and
  the `ob_server` GPU-free dependency rule.

**Demo:** a 60-second clip of walking through generated terrain, breaking and
placing blocks, FPS counter visible.

## 5. Phase B — Engine maturity and content platform (`0.1.0-alpha`)

**Goal:** turn "a prototype that renders" into "an engine that holds 512 blocks
of view at speed, with a content pipeline and a real UI".

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-04 GPU rendering** | Render graph, section/cluster meshing on worker threads, GPU upload ring, region/cluster culling, multi-draw-indirect submission, texture arrays, CSM shadows, basic post (tone map, TAA), streaming budgets, benchmark scenes and CI perf gates at p95/p99 | 8 |
| **MS-05 Content + UI** | Registry/tags/pack loader and validator, `base` pack as data, retained UI (taffy + cosmic-text), main menu, HUD, inventory/containers with server-authoritative transactions, settings, localization scaffolding, audio event bus with placeholder sounds | 10 |

**Phase B exit criteria**

- 512-block render distance sustains **≥ 120 FPS** on the reference machine,
  p99 frame time within the budget; a scripted fly-through benchmark is enforced
  in CI (DEC-035).
- Content packs load from a directory/zip, are schema-validated, hot-reload in
  development, and the base game is itself a pack with no private hooks.
- Inventory transactions are server-authoritative and survive 30% packet loss in
  the in-process harness without item duplication.
- All UI strings are localizable; one non-English locale is shipped as proof.

**Demo:** a fly-through at 512 blocks with the FPS overlay; creating a new block
via a content pack and placing it.

## 6. Phase C — Multiplayer and servers (`0.2.0-alpha`)

**Goal:** two players on the same world, one of them on a VPS, with prediction
that feels good on a 60 ms link.

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-06 Networking** | QUIC transport (quinn) with datagrams/streams, protocol v1 with content-hash handshake, join and progressive world streaming, interest management, entity replication, client prediction/reconciliation, LAN discovery, in-process network simulator, netem-style loss/latency tests | 12 |
| **MS-07 Dedicated server** | `openblock-server` binary, TOML config, console commands, ops/whitelist/bans, backups, graceful shutdown, structured logs, Prometheus metrics, pregeneration CLI, Docker image, systemd unit, server administration docs | 5 |

**Phase C exit criteria**

- Two clients join a dedicated server over the internet; movement feels local
  under 80 ms RTT and 2% packet loss; block edits from one player appear on the
  other within one tick.
- A 4 vCPU / 8 GiB VPS runs a 16-player world at 20 TPS with 2,000 loaded chunks.
- The server survives a malicious-client fuzz corpus without crashing, and
  movement/reach violations are detected and logged.
- `docker compose up` in an empty directory produces a joinable server in under
  a minute, with the ticket printed to the log.

**Demo:** a split-screen recording of two players building together, and a
`docker compose up` terminal capture.

## 7. Phase D — World and survival (`0.3.0-beta`)

**Goal:** the game becomes a *survival game*: a generated world worth exploring
and a progression loop worth playing.

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-08 Worldgen** | Noise stack, terrain shapes, biomes (climate-based), caves/aquifers/ravines, ore distribution, vegetation and features, structures (villages, temples, strongholds, …), nether and end dimensions, deterministic parallel generation at ≥ 250 columns/s on 8 threads, pregeneration | 14 |
| **MS-10 Survival loop** | Health/damage types/armor/combat, hunger and food, XP, tools and tiers, drops and block-breaking rules, crafting/stonecutting/furnaces/blast furnace/smoker, containers and hoppers, item entities, death/respawn, beds/anchors, difficulty, weather effects | 12 |
| **MS-13 Persistence maturity** | Region file format v1 frozen, migrations, world management UI, backup/restore, corruption recovery, `ids.json` content mapping, import/export tooling, save-size benchmarks | 5 |

**Phase D exit criteria**

- A player can start a world, survive a night, mine, smelt, craft tools and armor,
  build a house, explore 2,000 blocks in every direction, and enter the nether —
  all without admin commands.
- Worldgen is deterministic across platforms and produces no floating structures,
  broken biomes or unplayable spawns over a 10,000-seed corpus (nightly run).
- The save format round-trips a modded world with unknown content IDs without
  data loss (unknown content is preserved, not dropped).

**Demo:** a 10-minute "first day" survival playthrough.

## 8. Phase E — Depth and ecosystem (`0.4.0-beta`, `0.5.0-beta`)

**Goal:** the content depth that makes it Minecraft, and the modding platform
that makes it more than Minecraft.

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-09 Modding** | Content pack SDK and validator, WASM mod host (wasmtime) with capabilities and quotas, event/query host ABI v1, mod template and examples, server mod verification, per-mod profiler, `base` game fully migrated onto the public API, hot reload | 10 |
| **MS-11 Depth** | Mobs and AI (pathfinding, spawning, behaviours), redstone (full component set, deterministic update ordering), farming/breeding, brewing/enchanting/anvil/trading/villagers, bosses and end progression, minecarts/boats, portals, statistics and advancements, commands and game rules, spectator/creative completeness | 24 |
| **MS-12 Visuals and audio** | Full post stack (GTAO, SSR water, volumetric atmosphere, bloom), weather, particles, ambient sound and music system, dynamic range and spatialisation, quality presets, shader-pack API v1, controller support | 10 |

**Phase E exit criteria**

- A Tier-2 feature checklist run passes for mobs, redstone, enchanting and
  trading (the parity corpus is executable, see [MS-14](#10-phase-f--parity-scale-and-polish)).
- A mod written against the template adds a block, an item, a recipe, a mob and a
  UI screen and runs on a server without any client installation; a misbehaving
  mod is stopped by quotas instead of crashing the host.
- Redstone circuits behave deterministically and survive save/load; a suite of
  reference circuits (door, clock, piston extender, comparator logic) passes.
- The audio mix has a documented voice/streaming budget and never drops the
  frame budget.

**Demo:** a modded server minigame written entirely as a WASM mod, plus a redstone
contraption showcase.

## 9. Phase F — Parity, scale and polish (`0.6.0-beta`, `1.0.0-rc`)

**Goal:** close the parity gap, make servers scale, and make the game feel
finished rather than feature-complete.

| Milestone | What it delivers | Effort |
| --- | --- | --- |
| **MS-14 Parity + polish** | Tier-2 parity closure driven by the generated checklist, parity acceptance corpus ≥ 90%, spectator, full datapack semantics (where we adopt them), accessibility (narrator, remap, colour-blind, subtitles, controller), localization completeness, UX polish, tutorial/onboarding, world creation screen, performance presets | 14 |
| **MS-15 Scale + anti-cheat** | Interest-management tuning for 50 players, parallel region ticking, anti-cheat validation suite and replay evidence, moderation tools, admin RPC, load/stress harness (bots), server capacity model and benchmark, DDoS/rate-limit hardening, crash containment | 8 |

**Phase F exit criteria**

- The parity corpus reports ≥ 90% Tier-2 coverage with failures triaged and
  documented in the inventory.
- 50 bot players on the 4 vCPU VPS reference hold 20 TPS for one hour with p99
  tick ≤ 35 ms; a 100-player stress test is documented with the config that
  achieves it on 8 vCPU.
- Accessibility checklist passes: full keyboard/gamepad navigation, remappable
  bindings, UI scale, subtitles, narrator on menus, colour-blind palettes.
- No known crash, duplication, or remote-exploit issue from the fuzz corpus.

## 10. Phase G — 1.0 launch (`1.0.0`)

**Milestone MS-16** delivers the boring-but-critical launch work: signed builds
for all 1.0 platforms, auto-updater with delta patches and rollback, Flatpak/
AppImage/AUR packaging, container images, website and download page, migration
from 0.x saves, mod API 1.0 freeze and compatibility promise, launch trailer,
trademark clearance sign-off (DEC-037), and the support/security playbook.

**1.0.0 ship criteria** are the union of all phase criteria plus: 30 consecutive
days of release-candidate play without a P0 bug; a save from `0.1-alpha` migrates
to `1.0` losslessly; the dedicated server runs 7 days unattended on the reference
VPS without leaking memory or corrupting a save.

## 11. Effort model and calendar scenarios

Nominal totals (engineering only; see §12 for content production):

| Phase | Dev-weeks |
| --- | --- |
| A Foundation + first playable | 19 |
| B Rendering + content/UI | 18 |
| C Multiplayer + servers | 17 |
| D World + survival | 31 |
| E Modding + depth + audio/visuals | 44 |
| F Parity + scale | 22 |
| G Launch | 8 |
| **Total** | **≈ 159** |

Calendar scenarios (full-time equivalents, with 20% coordination overhead for
teams):

| Scenario | Throughput assumption | First playable (`0.0.1`) | Public alpha (`0.1`) | Multiplayer (`0.2`) | `1.0` |
| --- | --- | --- | --- | --- | --- |
| **Solo, full-time, AI-assisted** | ~0.85 dev-week per calendar week | ~4–5 months | ~8–10 months | ~12–14 months | **~2.5–3 years** |
| **Solo, part-time** | ~0.4 dev-week per calendar week | ~10–12 months | ~1.8 years | ~2.5 years | **5+ years** |
| **Team of 3** | ~2.4 dev-weeks per calendar week | ~2 months | ~4 months | ~6 months | **~15–18 months** |
| **Team of 6** | ~4.4 dev-weeks per calendar week | ~5 weeks | ~2.5 months | ~4 months | **~10–12 months** |

The single most common failure mode for projects like this is scope. The roadmap
is deliberately structured so that **each phase is a shippable product**: a
2.5-year solo timeline is survivable because `0.1-alpha` arrives in months, not
years.

## 12. Content production (not in the numbers above)

Engineering effort does not include the content a parity game needs. Budget
separately:

| Work | Estimate | Notes |
| --- | --- | --- |
| Art: block/item textures, mob models, UI | 12–20 dev-weeks equivalent | Commission or license; must be original (DEC-009) |
| Audio: SFX, ambience, music | 4–8 dev-weeks equivalent | CC0/CC-BY sourcing plus commissioned music |
| Level design / structures / worldgen tuning | 6–10 weeks | Overlaps with MS-08 and MS-14 testing |
| QA / playtesting / parity verification | 20–30% of engineering time | The parity corpus makes this tractable |
| Localization (community) | Ongoing | Tooling from MS-05 makes it cheap |

Recommendation: build a **placeholder pack** early (MS-03) with programmer art
so engineering never blocks on art, and commission the real pack in Phase D so
it is ready for the alpha releases.

## 13. AI-assisted development (how this plan actually uses agents)

The plan is sized for a human team; AI agents are a multiplier on specific kinds
of work, and the plan should be honest about which.

| Work type | AI leverage | Why |
| --- | --- | --- |
| Research and specification drafting | High | Parallel synthesis of prior art, checklists, first drafts |
| Boilerplate, data schemas, CLI tools, tests | High | Mechanical, verifiable by CI |
| Documentation, changelogs, API references | High | Verifiable against code and specs |
| Content data (recipes, loot tables, block definitions) | High | Schema-validated, mass-produced |
| Refactors with strong tests | Medium | Tests are the safety net |
| GPU driver bugs, frame-time mysteries, netcode feel | Low | Requires measurement and judgment |
| Game design and balance | Low–medium | Needs playtesting taste |
| Security review and threat modelling | Medium | Useful as a second pair of eyes, not a substitute |

Process assumptions used for the calendar scenarios: AI-generated work is
reviewed by a human, CI gates it (DEC-035), and provenance rules apply
(DEC-036). Teams that skip review will spend the saved time on debugging
regressions.

## 14. Parallel tracks and the critical path

The critical path runs through `MS-03 → MS-04 → MS-06 → MS-11 → MS-14 → MS-16`.
Safe parallelisation for a 3-person team:

| Track | Owner | Milestones |
| --- | --- | --- |
| Rendering and engine | Engineer A | MS-02, MS-04, MS-12, perf |
| Simulation and gameplay | Engineer B | MS-01, MS-08, MS-10, MS-11 |
| Networking, content, tools | Engineer C | MS-05, MS-06, MS-07, MS-09, MS-13 |

Content/art/audio can be commissioned from Phase C onward. Documentation and
content packs are ideal for AI-assisted parallel work.

## 15. Post-1.0 roadmap

| Release | Theme | Highlights | Effort |
| --- | --- | --- | --- |
| `1.1` | Flagship content I | First-party differentiators built on the mod API: programmable logic blocks (an in-world computer), first-class energy/fluid networks, one showcase tech pack | 10–14 |
| `1.2` | Worlds and dungeons | Procedural dungeon/structure API, showcase adventure pack, map sharing | 8–12 |
| `1.3` | Mobile | Android port, touch UI, mobile perf tiers, Play Store release | 8–12 |
| `1.4` | Far terrain | Visual-only LOD/far-terrain renderer out to 128+ chunks | 4–6 |
| `1.5` | Native plugins + server mesh | Signed native plugin tier in a supervised process, multi-server network (proxy/lobby) | 6–10 |
| `1.6` | Web and reach | WebTransport/web client feasibility, better Steam Deck and accessibility passes | 8–12 |
| TBD | Compatibility gateway | Optional Minecraft-client gateway as an isolated, best-effort bridge (not core) | 10–16 |

Community mods and content packs are expected to fill most of the post-1.0
content surface; the engineering roadmap should shrink, not grow, after 1.0.

## 16. Risks to this schedule (summary)

Full register: [Risks](05-risks.md). The schedule-sensitive ones:

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Custom renderer takes longer than estimated | +30–50% on Phase B | Vertical slice first, buy UI/audio/text libraries, cut shadows/post if needed |
| Netcode "feel" iterations | +50% on MS-06 | In-process simulator from day one; reference netcode patterns; accept correction |
| Parity content velocity (Minecraft ships 70–145 blocks/year) | Unbounded | Pinned baseline 1.21.11; generated checklist; delta process per release |
| Solo founder burnout | Project death | Public milestones, playable builds, community early, scope tiers |
| Performance target missed at 144 FPS | Reputation | 120 FPS release gate, 144 FPS performance mode, honest benchmarking |
| Art/audio production | Blocks releases | Placeholder pack early; commission in Phase D; CC0 fallback |

## 17. What to do in the first two weeks

If the team is one person and today is day zero:

1. Freeze the specification skeleton and the decision register (done in this
   repo).
2. Build the `MS-00` foundation: CI, profiler harness, `wgpu` window, headless
   server process, benchmark scene.
3. Start `MS-01` (paletted sections + tests) and `MS-02` (tick loop + determinism
   harness) in parallel.
4. Publish a weekly devlog. Recruiting contributors is easier when progress is
   visible.
5. Do **not** start with the worldgen or the mobs. The vertical slice is the
   single most important de-risking step in the entire plan.

## 18. Open questions

1. **Funding and staffing shape (owner: project lead, before Phase B).** The
   calendar scenarios assume full-time effort; the roadmap does not assume a
   funding source. Decide whether to pursue grants/sponsorship before or after
   the `0.1` alpha. → [Team and process](04-team-and-process.md) §7.
2. **Whether `0.0.1` is distributed publicly (owner: MS-03).** A public tech
   preview recruits contributors earlier but commits to save compatibility
   messaging and support load.
3. **Benchmark hardware (owner: MS-00/MS-04).** The performance gates require a
   dedicated bench box or a stable self-hosted runner; hosting cost is not yet
   budgeted.
4. **Content production timing (owner: MS-08/MS-14).** When to commission the
   real art/audio pack depends on the first public release date; see §12.
5. **Whether to ship 1.0 on Steam (owner: MS-16).** Steam adds Deck verification,
   store review and revenue-share considerations but reaches far more players
   than GitHub Releases alone. → [Platforms](01-milestones.md).

## 19. References

- [Vision](../vision.md), [decision register](../decisions.md)
- [Milestones](01-milestones.md), [MVP definition](02-mvp.md),
  [Work breakdown](03-work-breakdown.md), [Team and process](04-team-and-process.md),
  [Risks](05-risks.md), [Release and operations](06-release-and-operations.md)
- [Performance budgets](../spec/25-performance-budgets.md)
- Research: [Weft teardown](../research/01-weft-teardown.md),
  [engine stack](../research/02-engine-and-render-stack.md),
  [voxel rendering](../research/03-voxel-rendering-sota.md),
  [networking](../research/04-networking-sota.md),
  [Minecraft inventory](../research/05-minecraft-feature-inventory.md),
  [modding](../research/06-modding-and-content-system.md),
  [legal](../research/07-legal-licensing-and-ip.md),
  [UI/audio/i18n](../research/08-ui-audio-localization.md),
  [persistence/packaging](../research/09-persistence-assets-packaging.md),
  [custom content](../research/10-custom-content-and-differentiation.md)

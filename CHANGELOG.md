# Changelog

All notable changes to OpenBlock are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/) from `1.0.0` onward. Pre-1.0
releases may change APIs, save formats and the protocol between minors; see
[docs/plan/00-roadmap.md](docs/plan/00-roadmap.md) §2.

## [1.2.0] — 2026-11-25 (Light & Home)

The survival-comfort release: caves go dark and torches matter, beds end the
night, wheat makes food renewable, and the inventory grows from 9 to 36 slots.
Protocol **v3** (36-slot inventory, light/sleep sync); save format 4 remains and
1.0/1.1 worlds migrate their fake sky light automatically.

### Added — real lighting and torches
- Full sky + RGB block-light engine: top-down sky seeding, 6-neighbour BFS with
  per-block attenuation, 31³ edit relighting, chunk-border propagation, and a
  cached deterministic light hash. Block light is stored in save v4 as before.
- New content (append-only ids): torch (emission 14), dry/wet farmland, four
  wheat stages and beds, plus seeds/wheat/bread and wood/stone/iron hoes,
  recipes and 11 texture layers; cross-shaped cut-out models.
- Renderer: packed sky/block light per vertex (smooth 3×3 light + AO), warm torch
  tint, cutout cross models, cutout shadows; 512-block scenes still render.
- Migration: additive `world_meta.txt` with `light_version=2`; 1.0/1.1 worlds
  get a one-time relight (dark caves) with the marker written only after the
  save commits. No save-format change.
- Server-authoritative lighting sync: `LightUpdate` batches changed section
  light to nearby players (raw/zstd, rate-limited, interest-filtered); clients
  apply verbatim and remesh affected sections/neighbours. Single-player consumes
  dirty light sections through the existing mesh scheduler.
- Measured: seeding +0.14–0.16 ms/chunk; ordinary edit relight p50 <0.15 ms
  (deep-shaft worst 1.9 ms); 512 streaming 470 ms and 512 mesh+upload ~399 ms;
  world heap fell (fake light removed) to 132 MiB, RSS ~270 MiB.

### Added — beds, sleep, farming and spawning
- Beds (two blocks, fixed orientation), personal respawn, night/thunder sleep:
  single-player fast-forwards to sunrise over 100 ticks; multiplayer requires
  all alive players asleep, one awake blocks and timeout/damage wakes everyone;
  obstructed beds refuse, damage wakes.
- Wheat farming: hoe grass/dirt to farmland, plant seeds (light ≥9), hydration
  from water within 4 blocks, 2,400/1,200 ticks per stage dry/wet, stage block
  updates, harvest wheat + seeds, grass seed drops (12.5%); `crops.txt` persists
  crop state and old saves without it still load.
- Hostile spawning now requires gameplay light ≤7, so torches suppress spawns;
  night/day factor derived from the server clock.
- Server-authoritative `UseBlock` routing with rejection reasons for missing
  hoe/seeds, darkness, wrong time, obstruction, reach, support, sleep state.

### Added — 36-slot inventory and protocol v3
- 9 hotbar + 27 main + 4 armor slots (equipment now 36..40, windows 100+);
  inventory UI is four rows with Armor/Main/Hotbar labels in both single-player
  and network clients; click pick/place, Shift-click quick transfer, armor
  equip/unequip; compact 960×540@125 layout.
- Server inventories grow to 36, pickup fills hotbar→main→drop, death keeps the
  full inventory; `inventory.txt` migrates old 9-slot saves and writes only
  non-empty rows; furnace/chest windows unchanged.
- Protocol v3: InventorySync carries hotbar/main/equipment; LightUpdate,
  TimeSync, SleepState, SleepRequest and Wake added; v1/v2 peers are rejected
  with an actionable mismatch message.

### Verification
- Workspace tests pass (client 289 lib + loopback, server 118 lib + 66
  integration, content 42, world 62, render 174, protocol 35+, core 44, tools);
  clippy `-D warnings`, fmt, rustdoc, architecture/docs checks, smoke twice,
  load gate and backup/restore pass; CI covers Fedora 44/45, Ubuntu, Windows and
  macOS.
- New v1.2.0 screenshots: torch-lit room, wheat farm, sleep overlay, 36-slot
  inventory and compact 960×540@125; design docs `docs/plan/1.2-*.md`.
- Seed-42 smoke hash (covers blocks/entities/crops/personal respawn; derived
  light is verified by world save/load and relight tests instead, since border
  propagation can legitimately settle differently across streaming reloads)
  `ed80bbaf244bc31d`.

### Known limitations
- No weather yet; sleep has no thunder window; crops freeze in unloaded
  columns; no bone meal, tool tiers beyond iron, or offhand slot; bed pillow is
  always +Z; no wall torches or coloured light; predicted multiplayer block
  placement still relights locally before the authoritative LightUpdate;
  dedicated-server chunk memory still grows with exploration; far shadows/LOD
  limits from 1.1 remain.

## [1.1.0] — 2026-11-25 (Reach: 512-block view distance and shaders)

The "reach" release: the world is now visible 512 blocks away and the renderer
has a real shader/lifecycle pipeline, plus the first exploration content and a
save system that no longer hitches. Save format 4 and protocol 2 are unchanged.

### Added — 512-block render distance
- Render distance now goes up to **32 chunks / 512 blocks** (core, settings,
  network interest radius, CLI/server config), with a Settings row showing both
  units and a one-time memory/GPU warning when crossing 16 chunks.
- Distance LOD: full 16³ section meshes within 10 chunks, merged 2×2-chunk
  surface-shell batches beyond (top 48 blocks per column), frustum + distance
  culling, deterministic batched draw order. Far villages/trees/water/cliffs
  stay visible; far caves/deep geometry are intentionally omitted.
- Adaptive streaming: full 4,225-column world in ~0.5 s single-player; loading
  overlay with column/section progress; movement released once the near ring is
  meshed (45 s fallback); far chunks stream in the background.
- Multiplayer streaming: server interest radius 32 with adaptive chunks/tick;
  client request pacing (128/s burst 64), eviction outside radius+2, release of
  section meshes and LOD batches. Loopback 512 join: 12.6 s, 13.2 MiB,
  server tick p99 1.46 ms.
- Camera far plane 560 m, fog/horizon retuned (400–532 m at 512), F3 counters
  for chunks/sections/LOD batches/draws/vertices/streaming/memory.
- New `render_distance_bench` tool with preset/full-detail/time/camera options.

### Added — shaders, lighting and graphics
- New HDR render graph shared by window and offscreen paths: 2 sun-shadow
  cascades → opaque world → sky/clouds → water/outline → bloom → ACES post → UI.
- **Shadows**: 32/160 m cascades, stable sphere/texel-snapped fitting, PCF,
  cascade blending, AABB proxies for distant casters; receives on terrain,
  entities and water.
- **Water**: split opaque/water meshes, translucent sorted pass, animated wave
  normals, Fresnel sky tint, sun glints, shadows and fog.
- **Sky/clouds**: procedural world-space cloud layer with coverage/wind/day-night
  tint, horizon haze; sun/moon/stars retained.
- **Post**: HDR bloom, ACES-fit tonemapping, saturation, vignette, FXAA-lite.
- **Graphics settings tab**: preset (Low/Medium/High/Ultra/Custom), shadows,
  clouds, water, cloud coverage, bloom, FXAA; new F3 Graphics/Shadows lines.
- Measured at 512 blocks (seed 42, readback benchmark): Low 147 FPS @720p /
  100 @1080p; High 91/62; Ultra 86/62. RSS ~455 MiB at 4,225 columns.

### Added — exploration, saves and performance
- **Worldgen structures** (deterministic, chunk-local, existing blocks only):
  ruined tower, abandoned camp, stone circle, mineshaft adit; one candidate per
  8×8-chunk region with dry/flat/ravine-free site rules, spawn exclusion and
  order-independent placement; +<=0.02 ms/chunk generation cost.
- **Async saves**: copy-on-write world snapshots + a single save worker that
  serializes/compresses/stages/fsyncs off the tick thread; tick pause is
  **0.41 ms at 4,225 columns** (was 318 ms), peak RSS +36 MiB, previous save
  kept on failure, autosave coalescing, bounded shutdown wait with sync fallback.
- **Append-only save compatibility**: v4 loader accepts saves whose registry
  fingerprint is a strict prefix of the current registry (blocks added at the
  end), so future content keeps 1.0.x worlds open; removals/reorders still
  rejected with a clear error.
- Spawn search is structure-aware; seed-42 spawn unchanged; new smoke hash
  `eba0c42219691190`.
- Save caps raised for 512-radius worlds (512 MiB payload / 1 GiB file).

### Verification
- Workspace tests, clippy `-D warnings`, fmt, rustdoc, architecture/docs checks,
  smoke, quick load gate, backup/restore and the 512-block benchmark all green;
  CI covers Fedora 44/45, Ubuntu, Windows and macOS.
- New v1.1.0 classic-texture screenshots: 512 vistas day/dusk/night, water,
  clouds, Graphics tab, F3 and title/survival UI.

### Known limitations
- Far-cascade PCF is 2×2 hardware taps; AABB shadow proxies lose tree silhouettes
  beyond 64 m; shadows reach 160 m, not 512; no water SSR/refraction/foam or
  underwater volume; fixed exposure; FXAA-lite (no TAA/MSAA); far LOD hides deep
  caves and can show a tier seam at extreme grazing angles.
- Server chunk memory still grows with exploration (save-safe unload is next);
  client single-player autosave uses the async path, some tools still call the
  synchronous API.

## [1.0.2] — 2026-11-25 (Minecraft-style UI font)

### Added
- **Dual-font UI**: all body text, HUD, menus, chat, tooltips and F3 now use a
  Minecraft-style TTF font — bundled **Monocraft 4.2** under SIL OFL 1.1 — while
  the logo and screen/panel titles keep the original 8x8 bitmap font. Rendering
  uses a second rasterized 128x64 glyph atlas (16x8 cells of 8x8, printable
  ASCII, cap-height-normalised, crisp binary alpha); layout metrics are
  unchanged (8 px advance/line at scale 1), so no UI re-layout was needed.
- **Runtime font override**: `--ui-font PATH` (persisted) and `--default-ui-font`;
  `Settings.ui_font_path`; auto-discovery of any `.ttf`/`.otf` in
  `<data-dir>/fonts/` (preferring `minecraftia`, then `minecraft`, then first
  sorted) and a shallow cross-platform system Minecraftia search; built-in
  Monocraft fallback. Invalid/unreadable fonts warn and fall back without
  failing startup.
- **Settings → Display → UI font** row showing the active font/source, plus
  Reset (built-in) and Reload (rescan) actions; the row works from the title
  Settings too. F3 reports the active UI font. Title/heading call sites migrated
  to `title_text`; body text uses `text()`.
- `docs/client-ui-font.md`, v1.0.2 screenshots (classic textures), asset
  licensing metadata: `assets/ui-font/Monocraft.ttf` + OFL text, REUSE
  annotation as `OFL-1.1`.

### Notes
- Minecraftia itself is **not bundled** (its licence is personal-use only);
  place `Minecraftia.ttf` in `<data-dir>/fonts/` or pass `--ui-font` to use the
  exact font locally. Monocraft is the bundled, permissively licensed
  Minecraft-style font.
- No save-format or protocol change; existing worlds/settings/servers work.

### Verification
- ob_render: 123+ tests including dual-font GPU tests; ob_client: 267 lib +
  13 loopback; ob_core: 39; workspace check, clippy `-D warnings`, fmt, rustdoc,
  architecture/docs checks; offscreen font sample and v1.0.2 screenshot set
  visually verified (crisp UI text, 8x8 titles intact).

## [1.0.1] — 2026-11-25

Strict post-release polish update; no save-format or protocol change.

### Added
- **GUI scale**: Auto/75/100/125/150 in the Display tab, persisted as
  `ui_scale_percent` (Auto keeps 1280x720 and 960x540 at the 100% baseline;
  `--ui-scale` overrides one run for audits). Applies live to menus, HUD,
  containers, F3, chat, name tags and the connect overlay; F3 now reports the
  effective scale and logical size.
- Compact responsive layouts for 960x540 at 125/150% (logical 640x360): settings,
  death, front-end forms, creative inventory and the F3 overlay reflow instead
  of clipping.
- Connect/loading/failure panels are content-sized, with endpoint/player lines,
  progress percentage, keyboard focus on Reconnect/Restart and Quit, and no
  stale world/roster fields in error mode.
- 1.0.1 screenshots in `docs/img/v1.0.1-*` and implementation notes in
  `docs/client-gui-scale.md`.

### Fixed — server, persistence and networking audit
- **Save-size guard**: `Host::save` now refuses (and keeps the previous save)
  when the v4 payload/file would exceed the loader's limits, instead of writing
  an unopenable world.
- **Autosave actually runs**: tick-advanced state (idle connected players,
  furnaces, mobs, item physics, ticks/day) now marks the world dirty, so
  autosave no longer loses hours of progress.
- **Handshake absolute deadline**: trickled handshake frames can no longer hold
  handshake slots indefinitely.
- **Container windows revalidated per transaction**: teleport-plus-move in one
  tick can no longer use a stale/distant chest; distance, expected block and
  live record are checked.
- **Crash-safe multi-file save**: all files staged and fsynced, sidecars renamed
  before `world.obs`, directory fsync; non-regular/device/FIFO paths are
  rejected before opening.
- **Entity-id and sidecar hardening**: item/mob ids stay inside the host space
  (bit-63/u64::MAX rejected), durability only on durable items, stricter
  `network_players.txt` tokens, heightmap no-op/air fixes.
- **State hash completeness**: persisted yaw/pitch/flying (canonical angles) are
  now hashed and round-trip exactly; seed-42 smoke hash is
  `eba0c42219691190` (deterministic across fresh runs and save/reload).
- Landed attacks push an immediate inventory/vitals/selection sync (tool-wear
  latency removed).

### Fixed — docs, security and packaging audit
- Corrected release reality (sources/binaries/checksums are staged, not
  published), stale SIGTERM claims, privacy/logging claims, test/screenshot
  counts, broken anchors and stale 1.0 scope notes.
- Rewrote `SECURITY.md` for the real 1.0 surface; REUSE now covers every tracked
  file with exactly one license; CI workflows are read-only by default.
- Fixed AppStream launchable/screenshots/category, RPM metadata, Flatpak commit
  pin and packaging blocker docs; added release-asset/checksum/SBOM checklist.

### Performance/reliability audit (1.0.1)
- Re-measured v1.0.0 vs 1.0.1 with 3+ runs: worldgen 0.918 ms/chunk, host tick
  p99 0.0002–0.0257 ms, save/reload and 8-client load gate unchanged; no
  regression.
- Autosave impact now measured end to end (1 Hz autosave: p99 11.1 ms at 169
  columns, 85 ms at 1,089 columns) and the synchronous-save gap is decomposed
  (~55–58% zstd, ~39–40% serialize/CRC, <3% IO/fsync) with an async/compressed
  cache proposal for the 1.0.x line.
- 20/20 start/stop/reload cycles, 8-client churn, 300 s soak, SIGTERM-under-save
  and corrupted-sidecar/world tests pass; `--ticks 0` is not read-only
  (documented); CI benchmark/load-gate lanes stable with artifacts/timeouts.
- Remaining known gaps unchanged: network chunk unloading, transactional
  multi-file crash window, client-authoritative movement, save pause, reference
  hardware capacity.

### Fixed — UX
- Survival inventory no longer draws the gameplay hotbar/vitals over its footer
  at small logical heights; creative inventory reserves the hotbar band.
- Long chat/player-list/world-name/toast/reason text is width-clamped instead of
  crossing the panel edge.
- Front-end labels, pager and footer hints no longer overlap fields or panel
  borders at 640x360 logical.

## [1.0.0] — 2026-11-25 (Early Access)

The first Early Access release: a complete single-player survival loop, a
self-hosted multiplayer alpha with server-authoritative survival, procedural
audio, a real front-end and measured performance/operations tooling.

### Highlights since 0.2.0 (1.0 hardening)
- **Full acceptance scenario** added to CI (`crates/server/tests/acceptance.rs`):
  new world -> spawn safety -> trees/stone/ore reachability -> logs -> planks/
  sticks/table/tools -> stone -> furnace smelting -> iron tools/armor -> mobs ->
  cooked food -> fall damage -> death/respawn -> chests -> save/reload, plus a raw
  protocol-v2 multiplayer loop and edge cases (full inventory, tool break, full
  chest, mid-death save, dead actions).
- **Spawn safety fix**: spawn now resolves only safe dry surface columns (no
  starting inside a tree/feature), persists in an additive `spawn.txt` sidecar,
  is covered by the state hash and is stable across save/reload; old saves
  resolve once and persist. Smoke hash (seed 42) is now
  `1dba6105028ba6e5`.
- **Dead-player actions fixed**: dead survival players can no longer break/place
  via the single-player action path (the network path already rejected them).
- **UI/UX audit fixes**: title "EARLY ACCESS" + version/footer/contrast fixes;
  960×540 panel/footer collisions; settings checkboxes as On/Off and keybind
  conflict layout; inventory/crafting/container footer and header consistency;
  compact furnace/chest panels; full-durability bars hidden; HUD held-item label
  collision at narrow widths; F3 cached-section units; chat/toast/name-tag
  overlaps; new offscreen audit modes and the final v1.0.0 screenshot set.
- **Cross-platform CI fixes**: macOS accepted sockets forced blocking (Darwin
  inherits non-blocking), Windows platform-gated test imports, and the
  load-gate/benchmark/packaging jobs validated.
- **Performance/ops**: 8-client load gate (tick p99 0.163 ms, 20 TPS, RSS
  22.3 MiB, 0 overruns >35 ms) and a 10-minute soak (plateau, stable hash, no
  fd/thread leaks) documented; async-save/reference-hardware rows remain known
  gaps.

### Known limitations (accepted for 1.0.0)
- Recipe-book crafting (no spatial 3×3 grid); no torches/light propagation,
  farming, weather, beds, dimensions or redstone.
- Multiplayer has no TLS/auth/anti-cheat and client-authoritative movement;
  server-side crafting is not implemented.
- Windows/macOS audio is a silent stub; Linux uses runtime PulseAudio.
- Whole-world synchronous saves (autosave pause on 2,000+ column worlds).
- 4-vCPU/8-GiB reference capacity, 16/50-player and async-save work are planned
  for the 1.0.x line; 1.0.1 is the strict post-release audit/polish update.

### Verification
- Workspace tests, acceptance suite, adversarial protocol/save/pack/audio tests,
  network loopback survival tests, clippy `-D warnings`, fmt, rustdoc,
  architecture/docs checks and the 4-bot CI load gate.
- Headless smoke (seed 42, save format 4): `1dba6105028ba6e5`, stable across
  fresh runs; save v1-v4 migrations verified.
- Release screenshots (classic textures) in `docs/img/v1.0.0-*`.

## [0.2.0] — 2026-11-25 (front-end, controls, load gate)

### Added
- Front-end: single-loop `RootApp` router; title screen (Continue with last
  world, New World with validated filesystem-safe name + numeric seed, Load
  World browser with size/mtime/recent-first sorting and safe confirmed delete,
  Multiplayer direct-connect with address/name validation and five persisted
  recent servers, Settings, Quit); host-optional settings from the title;
  offscreen screenshot modes for every menu (`--screenshot-title`,
  `--screenshot-new-world`, `--screenshot-worlds`, `--screenshot-connect`,
  `--screenshot-settings-title`); `--title|--menu` override.
- Interactive key rebinding: `ob_core::config::{Action, KeyName, MouseButton,
  BindKey, Keybinds}` for 17 actions, duplicate rejection, clear/reset,
  unknown-key-tolerant serde, atomically persisted, applied live in both
  single-player and network runtimes; text fields/chat own the keyboard while
  editing.
- Accessibility/UX: Reduced-motion setting (no damage shake / sprint FOV pump),
  persisted Show HUD, multiplayer name tags (camera projection, distance fade,
  overlap culling), persisted last world/recent servers, HUD/death/chat/roster
  polish, menu version/save/protocol/OS line.
- Multiplayer load gate: lock-free `ServerMetrics` (rolling tick p50/p95/p99/max,
  >35/>50 ms overrun counters, chunk queue, bandwidth, edits/actions, RSS/fd/
  thread proxies), `--metrics-json`/`--metrics-interval`, stdlib
  `scripts/load-gate.py` (quick 4-bot CI mode, full 8-bot gate, `--soak`
  10-minute scenario with plateau/hash checks) and a CI job; docs with measured
  results and known gaps.

### Fixed
- macOS networking: accepted sockets are forced back to blocking mode (Darwin
  inherits the listener's non-blocking flag; every loopback/network test was
  rejected as `timeout`).
- Windows clippy: platform-gated imports in the save-robustness tests.
- Cross-platform network loopback tests now pass on macOS/Windows CI.

### Measured
- 8-client/60 s load gate: 8/8 joins, all interest sets in 2.15 s, 20.0 TPS,
  tick p50/p95/p99 0.061/0.124/0.163 ms, 0 overruns >35 ms, RSS 22.3 MiB,
  21 chunks/s, ~28 KiB/s per client, 656/656 edits accepted.
- 10-minute 8-client soak: 20.00 TPS, p99 0.146 ms, RSS plateau +1.9 MiB after
  warm-up, fds/threads stable, 60 autosaves, stable save/reload hash.
- Test suite 683+ tests plus the new front-end/keybind/load-gate tests; smoke
  seed 42 unchanged (`33875a6597c2ce7b`).

## [0.1.0] — 2026-11-25 (multiplayer alpha)

### Added
- **Multiplayer / dedicated server**: `ob_protocol` v2 (TCP + JSON framing,
  versioned catalog, palette chunk codec with zstd option, strict limits),
  `openblock-server` with world streaming, edit/movement/chat relay, survival
  inventories/vitals/equipment, server-authoritative actions, furnace/chest
  container windows with revisions, death/respawn, mob/item replication,
  per-player persistence, whitelist/max players/motd/config/console commands,
  autosave and graceful SIGINT/SIGTERM saves. Client `--connect` mode with
  connect/loading/failure UI, chunk replication, predicted edits with revert,
  remote player avatars, chat/roster, replicated entities, survival HUD and
  remote container screens.
- **Mobs & combat**: cow/pig/sheep/chicken/zombie content+mob specs, deterministic
  AI (wander/flee/chase/attack), day/night/depth spawning with caps, zombie sun
  burn, melee combat with weapon attack damage and tool durability, knockback,
  armor mitigation (up to 80%) and armor wear/break, mob drops as item entities,
  mob persistence (`mobs.txt`), blocky client avatars/interpolation, hit
  feedback/audio.
- **Ores, smelting, gear, containers**: coal/iron/copper ore veins (deterministic,
  depth-banded, density-tuned) with tool-tier drops; furnace block entity with
  fuel, 200-tick smelting, progress/lit state and persistence; 25 recipes incl.
  iron tools, leather and iron armor; chest block entity with 27 slots; armor
  equipping/defense/durability and equipment persistence; container and furnace
  UIs with click-to-cursor transfer, revisions and output-only slots.
- **Audio**: dependency-free `ob_audio` with 77 procedurally synthesized SFX
  clips plus two loops (79 total; material footsteps, break/place, pickup,
  hurt/death, eat, craft, tool break, splash, UI, wind and music), 48 kHz stereo
  mixer with 16 voices and a soft limiter; Linux PulseAudio backend loaded at
  runtime (no ALSA/cpal build deps),
  silent stub on other platforms; Audio settings tab with master/SFX/music
  volumes and mute, persisted; event hooks across gameplay.
- **Worldgen**: deterministic cave/ravine systems and ore veins; spawn-safe
  crust, cave/ore density tests and throughput reports.
- **Server operations**: hardened systemd unit/sysusers/tmpfiles, multi-stage
  container image + compose file, backup/restore scripts with retention and
  verification, Python load-test bots, server dependency check (no
  GPU/audio/X11 deps), admin handbook (quickstart/config/admin/security/backup/
  upgrade/troubleshooting).
- **Benchmarks/QA**: `worldgen_bench`, `server_bench`, `save_bench`, `soak`
  tools, quick CI benchmark profile, published performance envelope and known
  gaps; security/adversarial test coverage.

### Changed
- Save-world sidecars now include mobs, block entities (furnaces/chests) and
  network players; item pickup and inventory transfers preserve tool/armor
  durability exactly.
- Server banner/version reports protocol 2 consistently (core constant).
- Protocol v1 clients are refused with an upgrade reason.
- Settings: Audio tab; F3 shows armor/mobs/net status; network mode uses
  server-authoritative survival HUD.

### Measured
- Test suite: 683 passed, 0 failed, 10 ignored across the workspace (client 186
  lib + 13 loopback, server lib 89 + protocol 30 + world 35 + content 39 +
  audio 36 + render/sim/tools suites; adversarial/robustness tests included).
  Ignored entries are intentional GPU screenshot probes, real-pack/measurement
  reports and the network-throughput probe.
- Benchmarks: worldgen 0.921 ms/chunk mean (p99 1.147), server tick p99
  <=0.03 ms single-player baseline, save 21 ms @289 columns / 156 ms @2,025
  columns, 60,000-tick soak pass; v1-v4 save migrations pass.
- Smoke seed 42: `33875a6597c2ce7b` (save format 4; network runtime state is not
  hashed); two fresh runs deterministic.

## [0.0.9] — 2026-11-24 (tech preview, UI readability, pickup radius)

### Fixed
- F3 debug overlay readability: text scale 1.8/1.6 with +8 px line gaps, four
  grouped sections separated by gaps/separators, padded and non-overlapping
  panels, gold header accent, dimmer secondary column, word-wrapped long GPU
  names, adaptive side-by-side/stacked/left-only layout for 1280x720 and
  960x540, and a footer hint that no longer overlaps the hotbar. New
  `--screenshot-debug` offscreen mode.
- Settings could previously only be raised when clicking rows because row clicks
  always applied `+1`. Range/choice rows now draw explicit `<` / `>` hitboxes
  (hover-highlighted, dimmed at bounds) that adjust down/up respectively; row
  body clicks only focus, toggles still toggle, and keyboard Left/Right and
  mouse wheel both directions are covered by regression tests.
- More spacing in every menu: settings 620x520 (row pitch 44, larger gaps),
  crafting 620x560 (30 px rows, section headings, scrolls at small heights),
  inventory slot gaps/footer spacing, and death-screen button spacing; layout
  and minimum-spacing tests updated.

### Changed
- Dropped items are collected from a **3 m radius** around the player's chest:
  ready items are magnet-attracted at 7 m/s and picked up at 0.9 m (the 0.5 s
  pickup delay and partial pickup/overflow rules are unchanged). New tests for
  in-radius attraction, out-of-radius behaviour and the radius boundary.
- Controls/settings footer hints mention clickable arrows; the settings
  screenshot mode opens the Game tab as before.

### Measured
- Test count: 367 -> 382 (client 113, server 37 + 2 integration, render 109,
  world 32, sim 20, content 27, core 16, bins/docs). Save/smoke state hash is
  unchanged (`62b21c82b5d848e0`, seed 42) because pickup does not alter the
  smoke scenario. UI layout is CPU-only; no frame-time or GPU-cost change.
- Docs screenshots added for the spaced F3 overlay and settings arrows
  (classic textures).

## [0.0.8] — 2026-11-24 (tech preview, crafting, tools, drops, settings)

### Added
- Blocks: planks, cobblestone and crafting table (appended after snow, ids 12-14)
  with procedural textures in all four styles and resource-pack mappings
  (`oak_planks`, `cobblestone`, `crafting_table_top/side`).
- Items/tools: stick and wooden/stone pickaxes, axes, shovels, swords with stack
  sizes, tool tiers/speeds/durabilities (wood 59, stone 131). Tool-dependent
  mining speed and drops (stone/cobblestone need a pickaxe to drop), durability
  wear per broken block, tool-break feedback, procedural tool icons + durability
  bars in the hotbar/inventory/crafting screens.
- Recipes: planks, sticks, crafting table, and eight tools; ingredient totals
  with 3x3 table gating via a crafting table within reach. `C` opens the crafting
  screen (Basics/Tools sections, availability, ingredient colours, keyboard/mouse
  navigation) and right-clicking a crafting table opens it too; crafting consumes
  the selected hotbar ingredients and reports success/failure toasts.
- Dropped-item entities: broken blocks spawn visible mini-cube items with
  deterministic impulses, gravity/collision, automatic pickup after 0.5 s,
  partial pickup when the inventory is almost full, 5-minute despawn, persisted
  `items.txt`, and a state hash covering them. A render agent added an instanced
  entity pipeline (textured block cubes + flat-tinted item cubes).
- Full tabbed Settings screen on Esc: Game (mode, HUD, F3), Display (render
  distance, FOV, sensitivity, fullscreen), Controls reference (including C and
  Shift/Ctrl), Textures (style/pack + active source); keyboard + mouse, live
  application, atomic persistence, Resume/Save & quit buttons.
- Save format v4: new blocks appended without moving existing ids; v1/v2/v3
  saves load (v3 fingerprint mismatch tolerated because of the append-only
  invariant) and are upgraded on the next save. `ob_core::SAVE_FORMAT_VERSION=4`.

### Changed
- Stone now drops cobblestone instead of stone.
- Survival mining uses effective tool-aware times, not raw hardness; breaking a
  block no longer adds items straight to the inventory — drops spawn in the world
  and are picked up automatically (pickup toasts replace the old direct ones).
- Settings screen replaces the old pause list; F1/F3 shortcuts are suppressed
  while it is open.

### Measured
- Test count: 293 -> 367 (content 27, server 35+2 integration, render 109,
  sim 20, world 32, client 100, core 16, plus bins/docs). Smoke hash for seed 42
  is `62b21c82b5d848e0` (save format 4, dropped-item entities included).
- New textures keep the same 18-layer array with full mip chains; entity
  rendering adds one instanced draw pass with a per-instance 40-byte buffer and
  no new textures. Benchmark gate unchanged.

## [0.0.7] — 2026-11-24 (tech preview, survival foundations)

### Added
- Survival mode (default for new settings; `--creative` or the settings toggle
  switches): health and hunger (20 each), fall damage above a safe impact speed,
  starvation at zero hunger, slow regeneration above 18 hunger, a red damage
  flash, hearts and hunger HUD icons, pickup toasts and a death/respawn screen.
- Items: a minimal item catalogue (one item per block plus `openblock:apple`),
  nine-slot item hotbar with stack counts, deterministic block drops (grass drops
  dirt), a 1-in-6 deterministic apple drop from leaves, placement that consumes
  the selected stack, and right-click eating that restores hunger.
- Vitals and inventory persist with the world (`player.txt` extended, new
  `inventory.txt`), and the smoke state hash covers them; old saves load with
  defaults. Death teleports nowhere — respawn returns to the spawn point and
  keeps the inventory.
- Movement: crouch/sneak (`Shift`) with reduced speed, lower eye height and
  ledge protection; sprint (`Ctrl`) with a smooth FOV boost; walk-mode gravity
  and jumping confirmed; flying remains on `F`.
- Settings/CLI: `--survival`/`--creative`, game-mode row, and new F3 lines for
  game mode, vitals and movement state.

### Changed
- Hotbar scrolling direction now matches the usual convention (scroll down →
  next slot).
- Client inventory screen has a survival mode (hotbar stacks with counts and
  item hints) and keeps the creative palette in creative.
- Controls: Shift is crouch (was sprint), Ctrl is sprint.

### Measured
- Test count: 244 -> 293 (items/drops, vitals/fall/starvation/regen/eating,
  inventory/actions/persistence, crouch/sprint/edge protection, HUD/death UI).
- No rendering or streaming performance changes; the offscreen benchmark gate is
  unchanged.

## [0.0.6] — 2026-11-24 (tech preview, resource packs)

### Added
- Runtime loading of Minecraft-format resource packs for block textures:
  `--texture-pack PATH` / `--resource-pack PATH`, `--no-texture-pack`, and a
  Texture pack row in the pause/settings menu. The selection persists in
  `config.json`; missing packs fall back to the procedural style with a clear
  warning.
- The loader accepts a pack root, `assets/minecraft`, `textures/block`, or a flat
  block-PNG directory; resolves standard Minecraft filenames with fallbacks;
  crops animated textures to their first frame; scales mixed resolutions to one
  mipmapped array (16–256 px); falls back per layer to the procedural pack
  (e.g. water); applies 0/255 cut-out alpha to leaves and opaque alpha elsewhere.
- Vanilla-style biome tinting: grayscale grass tops and leaves get the plains
  grass/foliage tint, and `grass_block_side_overlay.png` is tinted and composited
  over the grass side, so high-resolution Minecraft packs look correct.
- F3 shows the active texture source and texel resolution; startup logs it.

### Notes
- No pack assets are bundled or redistributed. The reference test used a local
  pack with 128×128 block textures: 128 px array, 8 mip levels, only `debug` and
  `water` fall back to procedural layers.
- Test count: 216 -> 244.

## [0.0.5] — 2026-11-24 (tech preview, gameplay slice)

### Added
- Inventory screen (`E`): every registered block in a centred grid with icons,
  hover names and hardness hints; click or Enter assigns the block to the
  selected hotbar slot; arrow-key selection, wheel scrolling.
- Hotbar expanded to nine slots (`1`–`9`, mouse wheel); slots skip blocks that
  the active registry does not define.
- Hold-to-mine: raycast to 5 m, per-block hand-breaking hardness (stone 1.5 s,
  log 2.0 s, leaves 0.2 s, bedrock/water unbreakable), a progress bar under the
  crosshair, and a targeted-block wireframe outline; right-click placement now
  has a 200 ms cooldown.
- Day/night cycle: `Environment::from_ticks` over a 24,000-tick day (20 minutes
  at 20 Hz) drives sky palettes (day/twilight/night), a sun and moon disc, a
  deterministic procedural star field, moonlight and ambient tinting; fog
  follows the horizon colour; the F3 overlay shows the in-game clock. New worlds
  start at sunrise.
- Targeted-block outline rendered in both the windowed and offscreen renderers
  (`BlockOutline`, `SceneParams`, `Renderer::render_scene`,
  `OffscreenRenderer::render_world_scene`).
- Five biomes with smoothly blended heights: plains, forest, desert, snowy
  taiga and mountains; biome surfaces (sand dunes, snow, stone slopes),
  broadleaf and spruce trees, taiga snow drifts and boulders; public
  `Generator::biome_at` / `Biome` API.

### Changed
- World generation changed for every seed (new climate fields and biome
  blending), so new worlds look different and spawn points may move; existing
  saves still load and only newly generated chunks use the new generator.
- Screenshots accept `--time-of-day F` and the inventory has
  `--screenshot-inventory PATH`.

### Measured
- Biome shares at 512x512: seed 1 plains 14.6%, forest 22.7%, desert 21.3%,
  taiga 15.8%, mountains 25.6%; seed 42 plains 9.0%, forest 26.5%, desert 9.8%,
  taiga 12.6%, mountains 42.2%.
- Generation throughput: 1.45 ms/chunk in the test profile (300 chunks in
  435 ms), worst chunk 1.85 ms.
- Test count: 166 -> 216 (biome/worldgen, environment/outline GPU tests,
  inventory/mining/UI tests).

## [0.0.4] — 2026-11-24 (tech preview, visual-quality slice)

### Added
- Four selectable procedural texture styles with distinct art direction and
  palettes: `classic` (16², Minecraft-inspired), `painterly` (32²,
  Hytale-inspired), `lush` (32², detailed nature) and `minimal` (16², clean
  modern). Each style is deterministic, generated at startup and exposed via
  `--texture-style NAME` and a new Settings row (applied live and persisted).
- Full mip chains for block textures with nearest magnification and linear
  minification/mip filtering, so distant blocks stop shimmering.
- Alpha cut-out leaves: leaf layers punch clustered transparent holes and the
  world shader discards those texels (clustered coverage ~4.5–15% by style).
- Sky gradient (horizon to zenith) and distance fog for both renderers; distant
  geometry fades into the horizon instead of ending at a hard chunk edge.
- Subtle deterministic per-quad brightness variation so large merged surfaces
  (grass fields, stone floors, canopies) no longer read as one flat colour.

### Changed
- `BlockTextureArray` stores its style and resolution, supports
  `new_with_style`, and both renderers can switch styles at runtime
  (`Renderer::set_texture_style`, `OffscreenRenderer::set_texture_style`).
- Settings gain `texture_style` (validated enum, default `classic`) and the
  pause/settings menu gains texture-style rows.
- Offscreen screenshots use the persisted style, so `--screenshot` renders the
  same look as the window.

### Notes
- Perf impact of mips/alpha cut-out/per-quad tint is negligible on the
  reference machine; the offscreen benchmark gate is unchanged.
- Styles are procedural, not hand-painted packs. Hand-authored PNG texture
  packs remain an MS-05 content-system task; see docs/spec/15-content-system.md.

## [0.0.3] — 2026-11-24 (tech preview, MS-05 UI slice)

### Added
- HUD: crosshair, hotbar with isometric block icons, slot numbers, selected
  slot highlight and the selected block name.
- F3 debug overlay: FPS and frame p50/p95, position/chunk/facing, targeted block,
  seed, render distance, loaded chunks, sections drawn/cached, draw calls,
  triangles, mesh worker/in-flight/backlog counters, heap, ticks, GPU and window
  info; F1 toggles the HUD.
- Pause/settings menu (Esc): render distance (applied live), field of view,
  mouse sensitivity, fullscreen, debug overlay and HUD toggles, Save & quit.
  Settings persist to `<data-dir>/config.json` with clamping and atomic writes.
- F2 writes a PNG screenshot of the current view including the HUD.
- `ob_render::ui`: immediate-mode UI renderer with a generated 8x8 bitmap font,
  solid/textured quads, isometric block icons, alpha blending and batching, plus
  a GPU integration test that verifies a UI overlay on top of a rendered world.

### Changed
- Chunk meshing runs on a rayon worker pool with `SectionSnapshot` inputs,
  per-section epochs and stale-result dropping; the main thread only uploads
  (32 submissions and 4 ms upload budget per frame). `--mesh-workers N`.
- Chunk generation is parallel inside `Host::ensure_streaming` (deterministic
  insertion order): 2,191 -> ~19,200 chunks/s (8.8x) on the reference machine.
- Client loads settings at start-up; an explicit `--render-distance` overrides
  the persisted value.
- Content packs: `packs/base/blocks.json` mirrors the built-in registry, with a
  strict loader/validator (format version, ids, textures, bounds) and a parity
  test; `openblock-server --pack PATH` loads a pack.
- Save format v3 adds the content-registry fingerprint (REQ-SAVE-035); v1/v2
  saves still load and upgrade, and a mismatch is refused with an actionable
  error.

### Measured
- Parallel meshing: RD12 steady-state ~174.7 FPS (p50 5.7 ms, p95 11.5 ms) vs a
  serial build still draining its backlog; RD16 uncapped 174.9 FPS. Cold-start
  first-2 s mean frame time 9.80 -> 5.68 ms (-42%).
- Parallel generation: 256 chunks 116.8 -> 13.2 ms; identical determinism hash
  at 1, 4 and 16 threads.
- World save and memory numbers from v0.0.2 are unchanged (48.9 KiB save,
  576 KiB heap for 504 sections).

## [0.0.2] — 2026-11-24 (tech preview, MS-04 slice)

### Added
- Textured rendering: 14-layer procedural block texture array (grass, dirt,
  stone, sand, water, logs, leaves, bedrock, gravel, clay, snow), UV+layer
  vertex format, texture-array sampling in the shared pipeline.
- Greedy meshing with ambient occlusion: per-face/slice masks merged by
  texture, brightness and AO key; a 16x16 water surface becomes 6 quads, a 4x4
  platform top becomes 1 quad.
- Frustum culling for both the windowed and offscreen renderers; culled
  sections are no longer meshed or drawn.
- Benchmark harness: `openblock-tools bench` with cached-geometry frames,
  p50/p95/max timing, JSON reports and a `--max-frame-ms` CI gate (lavapipe on
  Ubuntu).
- World-generation quality: mountains and plateaus, snow peaks, stone slopes,
  sand/gravel/clay shores, connected tree canopies and sparse boulders.
- Client diagnostics: `--seconds N` runs the window for N seconds and prints
  mean FPS with p50/p95/p99 frame times; `--x11` forces the X11 backend.
- Graceful server shutdown: Ctrl-C/SIGTERM saves before exit.

### Changed
- Save format v2: zstd-compressed payload using the pure-Rust zstd crate, with
  compact uniform light storage (no 8 KiB array for uniform sections) and v1
  read compatibility. The 169-chunk reference world shrank from 35.6 MiB to
  48.9 KiB; host memory for 504 sections dropped from 4,416 KiB to 576 KiB.
- Content registry has 12 block types with per-face textures; hotbar unchanged.
- `RenderStats.sections` now counts sections drawn after culling.

### Measured
- See [docs/plan/08-measurements.md](docs/plan/08-measurements.md): 115 FPS at a
  128-block radius and 135 FPS at a 192-block radius on RADV integrated graphics
  at 1280x720 (steady state); 0.62 ms p50 for cached offscreen frames.

## [0.0.1] — 2026-11-24 (tech preview, specification stage)

The first runnable build: a windowed voxel client on wgpu 30 and a headless
host, wired to the first implementation of the engine core.

### Added
- `openblock` client: winit 0.30 window, wgpu 30 renderer, procedural terrain
  streaming, walk/fly AABB physics, block break/place, hotbar selection, world
  autosave, FPS/position in the window title.
- `openblock-server`: headless 20 Hz host with the same simulation code as the
  client, autosave, world reload, `--smoke` determinism check and `--ticks N`.
- Engine foundation: coordinate/AABB/RNG crate, in-code block registry,
  paletted 16³ sections with 4/8/16-bit palettes, packed sky/RGB light storage,
  chunk columns, raycasting and dirty-section tracking; deterministic terrain
  generator; player physics and block interaction; v0 snapshot save format
  with CRC32C; wgpu mesher with face shading and ambient occlusion.
- Offscreen render-to-PNG path (`--screenshot`) that works without a window or
  display, plus a GPU test that verifies non-black geometry output and skips
  cleanly when no Vulkan adapter exists.
- Linux/Fedora-first CI: Fedora 44/45 container jobs, a headless host smoke test
  in every Rust job, and a lavapipe offscreen render check on Ubuntu.

### Changed
- Crate versions move to 0.0.1; packaging metadata and Flatpak manifest are
  pinned to the same version.
- Networking, modding, UI text, audio and the GPU-driven render path remain
  later milestones (MS-06 and onward); this build is a single-player tech
  preview.

## [0.0.0] — 2026-11-24 (specification stage, pre-first-build)

This section is the pre-build snapshot from before the first playable release;
it is kept for history and is superseded by [0.0.1] above.

### Added

- Project specification (`docs/spec/`), research (`docs/research/`), roadmap and
  milestones (`docs/plan/`), decision register (`docs/decisions.md`) and ADRs
  (`docs/adr/`).
- Cargo workspace skeleton for the crate topology in DEC-003, CI, license and
  provenance policy.
- **Fedora-first platform decision (DEC-054)**: Fedora 44/45 x86_64 (Wayland,
  Vulkan, PipeWire) is the primary platform and reference distribution; Flathub
  Flatpak, COPR RPM, a hardened Fedora Server RPM/systemd/firewalld package, and
  required `fedora:44`/`fedora:45` CI jobs. Packaging scaffolding in `packaging/`.
- Linux-first platform section in the README, Fedora build prerequisites, and
  Fedora-specific requirements in spec/02, 03, 14, 19, 21, 22, 24 and 25.

### Changed

- **Documentation re-baseline after the 2026-11-24 review board.** The plan is now
  two tracks: 1.0 Early Access at Tier-1 parity (~200–240 engineering
  dev-weeks) and full Tier-2 parity at 2.0 (`MS-17`, ~500–800 dev-weeks). The
  previous 159-dev-week total was 5–8× below the project's own research
  inventory. Roadmap effort/calendar/content/critical-path sections, milestones,
  MVP, work breakdown and risks are reconciled; the README separates 1.0 from
  2.0 and post-1.0.
- **DEC-051:** the parity gate is defined once as two numbers — 100% of in-scope
  checks pass, and ≥ 85% vanilla coverage from a hand-authored behavioural
  checklist. The "≥ 90% coverage" pass claim is retired; coverage and pass rate
  are reported separately, and full Tier-2 parity is 2.0, not 1.0.
- **DEC-048:** 32³ render mesh clusters are the MS-04 default (16³ remains the
  storage/edit/light/network unit); OPEN-001 is closed.
- **DEC-049:** 1.0 clients run no mod or WASM code; mod UIs use server-computed
  data. OPEN-006 is closed and the client mod CPU budget is a reserved envelope.
- **DEC-050:** translations are not part of the join-critical hash; `content_hash`
  is strict and `view_hash` is advisory unless the server opts into strict view
  matching.
- **DEC-052:** dedicated servers default to a 256-block render distance equal to
  simulation distance; 512 blocks is opt-in with a published cost, and capacity
  claims must state co-located versus distributed players.
- **DEC-053:** the base pack uses original dimension and entity names
  (`openblock:underworld`, `openblock:terminus`); Minecraft names do not appear in
  shipped content identifiers or base-pack data.
- Milestone detail: MS-01 light budget corrected to 8 KiB per mixed section, MS-04
  cluster default, MS-11 scoped to Tier-1 depth, MS-14 redefined as Tier-1 parity
  plus corpus infrastructure and Tier-2 coverage reporting, MS-15 split into
  co-located and distributed capacity rows, MS-16 redefined as the Early Access
  launch, and `MS-17` added for the 2.0 track.
- Documentation index now lists `research/10-custom-content-and-differentiation.md`
  and `plan/07-review-findings.md`; ADR and milestone counts are corrected.

The first playable build is 0.0.1, described above; this 0.0.0 section records
only the specification-stage snapshot.

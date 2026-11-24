# Changelog

All notable changes to OpenBlock are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/) from `1.0.0` onward. Pre-1.0
releases may change APIs, save formats and the protocol between minors; see
[docs/plan/00-roadmap.md](docs/plan/00-roadmap.md) §2.

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

## [Unreleased]

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

No playable build exists yet; the first one is milestone `MS-03` (`0.0.1`).

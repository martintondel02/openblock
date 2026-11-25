# Release screenshots

The `v1.0.0-*.png` images in this directory are the 1.0.0 release captures, the
`v1.0.1-*.png` images are the post-release GUI-scale/connect-polish captures and
the `v1.0.2-*.png` images show the Minecraft-style UI font. They are PNGs
rendered by the offscreen client with the **classic procedural textures**
(`--no-texture-pack`) and no third-party resource-pack assets. Menus use the
deterministic temporary data set built into `--screenshot-*`; gameplay captures
use a fresh `--data-dir` so no local save can leak into a shot.

Every capture is windowless and needs a working Vulkan driver, but no display:

```sh
# Build once; the commands below assume `openblock` is on PATH.
./dev-cargo build --release -p ob_client --bin openblock
# Rootless/isolated checkouts can redirect the build:
CARGO_TARGET_DIR=/path/to/target ./dev-cargo build --release -p ob_client --bin openblock
```

## Reproduce the set

```sh
# Front-end (deterministic temporary worlds/settings)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-title.png       --screenshot-title
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-new-world.png   --screenshot-new-world
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-worlds.png      --screenshot-worlds
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-connect.png     --screenshot-connect
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-controls.png    --screenshot-settings=controls --data-dir /tmp/ob-shots/controls

# Survival / world screens (fresh data dirs)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-survival.png    --screenshot-items --data-dir /tmp/ob-shots/survival
openblock --no-audio --no-texture-pack --screenshot-inventory docs/img/v1.0.0-inventory.png --screenshot-items --data-dir /tmp/ob-shots/inventory
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-crafting.png    --screenshot-crafting --data-dir /tmp/ob-shots/crafting
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-furnace.png     --screenshot-furnace --data-dir /tmp/ob-shots/furnace
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-chest.png       --screenshot-chest --data-dir /tmp/ob-shots/chest
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-mobs.png        --screenshot-mobs --data-dir /tmp/ob-shots/mobs
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-death.png       --screenshot-dead --data-dir /tmp/ob-shots/death
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-debug.png       --screenshot-debug --screenshot-items --data-dir /tmp/ob-shots/debug
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-settings.png    --screenshot-settings=display --data-dir /tmp/ob-shots/settings

# Multiplayer (in-process loopback server; no external process)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-multiplayer.png --screenshot-network
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.0-name-tags.png   --screenshot-name-tags
```

`--screenshot-network` starts a real dedicated server on `127.0.0.1:0`, joins it
with the network client, and renders the same HUD the windowed client uses.
`--screenshot-name-tags` injects four synthetic remote players (including a long
name) into the shared name-tag projection/fade/overlap logic.

The four images attached to the GitHub v1.0.0 release (`title`, `survival`,
`crafting`, `multiplayer`) are byte-identical copies of the files here; the
remaining captures are documentation-only. `--title`/`--menu` force the
front-end on a normal launch, but the screenshot modes above already select the
screen explicitly, so they are not needed in these commands.

## v1.0.1 screenshots (GUI scale and connect polish)

The v1.0.1 set is 1280x720 at 100% GUI scale except for the explicit
`960x540-125` sample. `--ui-scale auto|75|100|125|150` is a run-only audit
override; the settings row persists the user choice. Every capture is still
windowless and uses a fresh data directory.

```sh
# Front-end
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-title.png      --screenshot-title
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-connect.png    --screenshot-connect

# Survival / menus (fresh data dirs)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-survival.png   --screenshot-items --data-dir /tmp/ob-shots/v1.0.1-survival
openblock --no-audio --no-texture-pack --screenshot-inventory docs/img/v1.0.1-inventory.png --screenshot-items --data-dir /tmp/ob-shots/v1.0.1-inventory
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-crafting.png   --screenshot-crafting --data-dir /tmp/ob-shots/v1.0.1-crafting
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-settings.png   --screenshot-settings=display --screenshot-items --data-dir /tmp/ob-shots/v1.0.1-settings

# Connect/loading/failure overlays (in-process deterministic network UI)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-loading.png    --screenshot-net-ui=loading --data-dir /tmp/ob-shots/v1.0.1-loading
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.1-failure.png    --screenshot-net-ui=failed --data-dir /tmp/ob-shots/v1.0.1-failure

# 960x540 with the GUI scale at 125% (settings Display tab shows the row)
openblock --no-audio --no-texture-pack --screenshot-size 960x540 --ui-scale 125 \
  --screenshot docs/img/v1.0.1-960x540-125.png --screenshot-settings=display --screenshot-items \
  --data-dir /tmp/ob-shots/v1.0.1-540-125
```

The 1.0.1 layout audit covers 75/100/125/150% logical targets for 1280x720 and
960x540 in `ui_state::tests::gui_scale_matrix_keeps_every_screen_inside_the_logical_target`.
At 960x540 with 150% scale the logical target is 640x360; settings, death,
front-end forms/inventory use compact metric variants, and the F3 overlay drops
to a 10-line compact set. The 960x540 125% capture is included because that is
the combination most likely to be used on a small laptop. Implementation and
layout notes are in [client GUI scale](../client-gui-scale.md).

## v1.0.2 screenshots (Minecraft-style UI font)

The v1.0.2 set uses the bundled Monocraft UI font for body text and keeps the
original 8×8 bitmap font for the title logo and every screen/panel heading.
All captures are 1280x720 at 100% GUI scale except the explicit 960x540@125%
sample. They are windowless, use classic procedural textures and a fresh data
directory. No Minecraftia font is required or redistributed; see
[client UI font](../client-ui-font.md) for using a personal `Minecraftia.ttf`.

```sh
# Front-end (deterministic temporary data; built-in Monocraft)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-title.png   --screenshot-title
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-connect.png --screenshot-connect

# Survival / menus (fresh data dirs)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-survival.png   --screenshot-items --data-dir /tmp/ob-shots/v1.0.2-survival
openblock --no-audio --no-texture-pack --screenshot-inventory docs/img/v1.0.2-inventory.png --screenshot-items --data-dir /tmp/ob-shots/v1.0.2-inventory
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-crafting.png   --screenshot-crafting --data-dir /tmp/ob-shots/v1.0.2-crafting
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-settings.png   --screenshot-settings=display --screenshot-items --data-dir /tmp/ob-shots/v1.0.2-settings

# F3 overlay (right column reports the active UI font)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-debug.png      --screenshot-debug --screenshot-items --data-dir /tmp/ob-shots/v1.0.2-debug

# Loading overlay (in-process deterministic network UI)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.0.2-loading.png    --screenshot-net-ui=loading --data-dir /tmp/ob-shots/v1.0.2-loading

# 960x540 at 125% GUI scale (the Display tab still shows the UI-font row)
openblock --no-audio --no-texture-pack --screenshot-size 960x540 --ui-scale 125 \
  --screenshot docs/img/v1.0.2-960x540-125.png --screenshot-settings=display --screenshot-items \
  --data-dir /tmp/ob-shots/v1.0.2-960x540-125
```

The settings capture focuses **Display → UI font** and shows
`Monocraft (built-in)` plus the Reset/Reload action rows and the
`<data>/fonts/` hint. To reproduce the same shots with exact Minecraftia,
obtain the font yourself and add `--ui-font /path/to/Minecraftia.ttf` to the
commands (the committed images use the bundled Monocraft so they remain
redistributable).

## 960x540 fallback audit

Add `--screenshot-size 960x540` to any of the commands above. The front-end,
settings, inventory, crafting, containers, death, F3 and HUD layouts are tested
for overflow at both sizes. The multiplayer loading/failure/roster overlays have
their own audit captures:

```sh
openblock --screenshot /tmp/loading.png --screenshot-net-ui=loading --no-audio --no-texture-pack
openblock --screenshot /tmp/failed.png --screenshot-net-ui=failed --no-audio --no-texture-pack
openblock --screenshot /tmp/disconnected.png --screenshot-net-ui=disconnected --no-audio --no-texture-pack
openblock --screenshot /tmp/roster.png --screenshot-net-ui=roster --no-audio --no-texture-pack
```

## State variants (audit)

```sh
# Empty / full containers and hand-crafting table gating
openblock --screenshot /tmp/furnace-empty.png --screenshot-furnace=empty --no-audio --no-texture-pack
openblock --screenshot /tmp/furnace-full.png  --screenshot-furnace=full  --no-audio --no-texture-pack
openblock --screenshot /tmp/chest-empty.png   --screenshot-chest=empty   --no-audio --no-texture-pack
openblock --screenshot /tmp/chest-full.png    --screenshot-chest=full    --no-audio --no-texture-pack
openblock --screenshot /tmp/craft-hand.png    --screenshot-crafting=hand --no-audio --no-texture-pack
openblock --screenshot /tmp/mobs-plain.png    --screenshot-mobs=plain    --no-audio --no-texture-pack
```

## UI workload

Each offscreen capture logs a UI geometry line such as:

```text
ui frame Crafting: 989 quads, 5934 vertices, <=4 texture draws
```

The UI renderer batches by texture (solid rectangles, Monocraft glyphs, bitmap
title glyphs and block icons), so a frame is at most four draw calls;
quads/vertices are the useful per-screen workload numbers.

## v1.1.0 screenshots (512-block reach and shaders)

The v1.1.0 set shows the 512-block distance-LOD view and the new HDR shader
pipeline using classic procedural textures. The vista captures come from the
`render_distance_bench` tool (seed 42, 512-block radius, High preset); gameplay
and menu captures use the offscreen client as above.

```sh
# 512-block vistas and shader features (windowless; render_distance_bench)
render-distance-bench --render-distance 32 --graphics high --time 0.5 --png docs/img/v1.1.0-vista-day.png
render-distance-bench --render-distance 32 --graphics high --time 0.82 --png docs/img/v1.1.0-vista-dusk.png
render-distance-bench --render-distance 32 --graphics high --time 0.12 --png docs/img/v1.1.0-vista-night.png
render-distance-bench --render-distance 32 --graphics high --scene-time 12 --png docs/img/v1.1.0-clouds.png
# The water scene is a dedicated offscreen scene from the shader GPU tests.
# UI/gameplay captures (classic textures, fresh data dirs)
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.1.0-title.png --screenshot-title
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.1.0-survival.png --screenshot-items --render-distance 12 --time-of-day 0.38 --data-dir /tmp/ob-shots/v1.1.0-survival
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.1.0-settings-display.png --screenshot-settings=display --data-dir /tmp/ob-shots/v1.1.0-settings
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.1.0-settings-graphics.png --screenshot-settings=graphics --data-dir /tmp/ob-shots/v1.1.0-settings-g
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.1.0-f3.png --screenshot-debug --screenshot-items --render-distance 12 --time-of-day 0.38 --data-dir /tmp/ob-shots/v1.1.0-debug
```

Note: `render-distance-bench` must be built from `ob_tools`; the screenshots
here were captured with the release `openblock` 1.1.0 binary. The water and F3
captures are documentation-only.

## v1.2.0 screenshots (Light & Home)

Offscreen captures with classic textures and fresh data dirs. The torch/farm/
sleep modes are offscreen-only flags in the client (`--screenshot-torch`,
`--screenshot-farm`, `--screenshot-sleep`).

| Capture | Path |
| --- | --- |
| Torch-lit sealed room (block light 14 falloff) | `docs/img/v1.2.0-torch.png` |
| 7×7 watered wheat farm with growth stages and iron hoe | `docs/img/v1.2.0-farm.png` |
| Night bed and sleep overlay ("1/1 players sleeping") | `docs/img/v1.2.0-sleep.png` |
| 36-slot inventory (hotbar/main/armor) with 1.2 items | `docs/img/v1.2.0-inventory.png` |
| Compact 960×540@125 inventory check | `docs/img/v1.2.0-inventory-960x540-125.png` |

```sh
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.2.0-torch.png --screenshot-torch
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.2.0-farm.png --screenshot-farm
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.2.0-sleep.png --screenshot-sleep
openblock --no-audio --no-texture-pack --screenshot docs/img/v1.2.0-inventory.png --screenshot-items --data-dir /tmp/ob-shots/v1.2.0-inventory
```

## v1.3.0 screenshots (512-chunk far terrain and FSR)

The v1.3.0 set shows the visual 512-chunk (8,192-block) render distance with
tiered far-terrain LOD, plus the optional FSR 1.0 upscaler. Native captures are
the recommended default; FSR quality/performance panels are documentation of
the measured option.

| Capture | Path |
| --- | --- |
| 512-chunk vista, day, High | `docs/img/v1.3.0-vista-day.png` |
| 512-chunk vista, dusk | `docs/img/v1.3.0-vista-dusk.png` |
| High-angle (900-block eye) far terrain | `docs/img/v1.3.0-high-angle.png` |
| 1080p far-terrain vista | `docs/img/v1.3.0-vista-1080p.png` |
| F3 with view/simulation/far counters | `docs/img/v1.3.0-f3.png` |
| FSR native / Quality / Performance (1080p) | `docs/img/v1.3.0-fsr-{native,quality,performance}.png` |
| FSR settings row and F3 line | `docs/img/v1.3.0-fsr-{settings,f3}.png` |

```sh
render-distance-bench --view-distance 512 --render-distance 32 --graphics high \
  --no-png --frames 120
render-distance-bench --view-distance 512 --render-distance 32 --graphics high \
  --fsr quality --no-png --frames 120
```

Far terrain is generated by the client from `Generator::surface_sample` (visual
only, seed from the handshake in multiplayer); FSR is off by default on the
reference iGPU because it measured slower than native there.

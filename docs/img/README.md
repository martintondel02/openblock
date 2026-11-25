# Release screenshots

The `v1.0.0-*.png` images in this directory are the 1.0.0 release captures. They
are 1280x720 PNGs rendered by the offscreen client with the **classic procedural
textures** (`--no-texture-pack`) and no third-party resource-pack assets. Menus
use the deterministic temporary data set built into `--screenshot-*`; gameplay
captures use a fresh `--data-dir` so no local save can leak into a shot.

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
ui frame Crafting: 989 quads, 5934 vertices, <=3 texture draws
```

The UI renderer batches by texture (solid rectangles, font glyphs and block
icons), so a frame is at most three draw calls; quads/vertices are the useful
per-screen workload numbers.

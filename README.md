<p align="center">
  <h1 align="center">OpenBlock</h1>
  <p align="center"><strong>A fast, self-hostable voxel sandbox — written in Rust.</strong></p>
</p>

<p align="center">
  <a href=".github/workflows/ci.yml"><img alt="CI" src="https://github.com/martintondel02/openblock/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE-MIT"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg"></a>
  <img alt="Version" src="https://img.shields.io/badge/version-1.0.0%20early%20access-orange.svg">
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.100%2B-B7410E.svg?logo=rust">
  <img alt="Primary platform" src="https://img.shields.io/badge/platform-Fedora%2044%2F45%20%7C%20Linux-51A2DA.svg?logo=fedora">
</p>

OpenBlock is an open-source voxel sandbox: mine, build, explore and survive alone
or with friends on servers you host yourself. **1.0.0 is an Early Access
release**: the core survival game and self-hosted multiplayer are playable and
tested end-to-end, while crafting-grid, lighting, farming, weather and the
modding track are explicitly post-1.0 (see [Known limitations](#known-limitations)).

![OpenBlock 1.0 title screen](docs/img/v1.0.0-title.png)

## What's in 1.0.0

- **Survival core** — procedural world with five biomes, caves/ravines and
  coal/iron/copper ore veins; health/hunger, fall damage, starvation and
  regeneration; mining with tool requirements, speeds and durability; block
  drops as visible item entities with a pickup magnet; death/respawn with
  persisted vitals and inventory.
- **Progression** — 25 recipes (planks, sticks, crafting table, wooden/stone/
  iron tools, leather/iron armor), furnace smelting with fuel, chests, 27-slot
  container windows; iron tools and armor with real stat/durability changes.
- **Mobs and combat** — cows, pigs, sheep, chickens and zombies with
  deterministic AI, day/night/depth spawning, sun burn, melee combat, knockback,
  armor mitigation (up to 80%), mob drops and client-side blocky avatars.
- **Self-hosted multiplayer** — dedicated server with world streaming, chat,
  player roster, avatars/name tags and **server-authoritative survival**
  (inventories, vitals, equipment, actions, containers, death/respawn,
  persistence); protocol v2 over TCP with strict limits. No TLS yet: use it on a
  LAN, VPN or SSH tunnel.
- **Audio** — 79 procedurally synthesized clips (material footsteps, break/place,
  pickup, hurt/death, eat, craft, tool break, splash, UI) plus wind/music loops;
  master/SFX/music volumes and mute in the Audio tab. Real output on Linux via
  PulseAudio loaded at runtime; Windows/macOS build with a silent stub in 1.0.
- **Front-end and accessibility** — title screen (Continue/New World/Load World/
  Multiplayer/Settings/Quit), world browser, direct connect with recent servers,
  interactive key rebinding for 17 actions, reduced motion, persisted HUD
  visibility and readable spaced UI at 1280×720 and 960×540.
- **Resource packs** — load Minecraft-format pack directories at runtime
  (`--texture-pack PATH`), with biome tinting, animated first frames and
  per-layer fallback. No third-party assets are bundled or redistributed.
- **Server operations** — hardened systemd unit, dependency-free container image
  + compose file, backup/restore scripts, load-test bots, metrics endpoint,
  admin handbook and graceful SIGINT/SIGTERM saves.

| Survival | Crafting | Mobs | Multiplayer |
| --- | --- | --- | --- |
| ![Survival HUD](docs/img/v1.0.0-survival.png) | ![Crafting](docs/img/v1.0.0-crafting.png) | ![Mobs](docs/img/v1.0.0-mobs.png) | ![Multiplayer](docs/img/v1.0.0-multiplayer.png) |

More captures (new world, world browser, connect, controls, settings, furnace,
chest, death, F3, name tags) are in [`docs/img/`](docs/img/README.md).

## Quick start

Build requirements: Rust 1.100+, a working Vulkan driver, and on Linux
Wayland/X11 libraries. The repository's `./dev-cargo` wrapper pins a workspace
`CARGO_HOME` and a Zig-based linker for systems without a C compiler; plain
`cargo` works on normal Fedora/Ubuntu installs.

```sh
# single player (title screen)
cargo run --release -p ob_client --bin openblock

# host a survival world (no GPU or audio device needed)
cargo run --release -p ob_server --bin openblock-server -- \
  --listen 0.0.0.0:25565 --world myworld --motd "My server"

# join it
cargo run --release -p ob_client --bin openblock -- \
  --connect 127.0.0.1:25565 --name alice
```

Pre-built packages: see [packaging/](packaging/) (Flatpak for the client, COPR
RPM for Fedora, systemd/container for servers) and
[docs/admin/](docs/admin/README.md) for hosting, backups, upgrades and security.

## Controls

| Input | Action |
| --- | --- |
| WASD / mouse | move / look |
| Space | jump (swim up in water) |
| Shift | crouch/sneak (edge-safe, lower camera) |
| Ctrl | sprint |
| F | toggle flight |
| Hold LMB | mine; attacks a mob under the crosshair |
| RMB | place / eat / open furnace or chest / use |
| 1–9, wheel | select hotbar slot |
| E | inventory (armor slots included) |
| C | crafting |
| T / Tab | chat / player roster (multiplayer) |
| F1 / F2 / F3 | hide HUD / screenshot / debug overlay |
| Esc | settings (or back) |

All gameplay keys can be rebound in **Settings → Controls**.

## Multiplayer and servers

The dedicated `openblock-server` ships with a flat `server.toml`, CLI flags,
console commands (`help`, `list`, `say`, `kick`, `save`, `tps`, `stop`), a
whitelist, autosave, per-player persistence by name and a metrics JSON stream
(`--metrics-json`). The interim wire protocol is **plain TCP + JSON without
TLS or authentication**, and movement is client-authoritative within server
bounds/reach/rate checks — do not expose it directly to the public internet.
Full guidance: [docs/admin/security.md](docs/admin/security.md).

The load gate in CI runs four bot clients on every push; the full 8-client load
gate and a 10-minute soak are available locally:

```sh
python3 scripts/load-gate.py --quick     # CI profile
python3 scripts/load-gate.py             # 8 clients, 60 s
python3 scripts/load-gate.py --soak      # 8 clients, 10 min
```

## Performance

The benchmark suite (`worldgen_bench`, `server_bench`, `save_bench`, `soak`)
runs quick profiles in CI; measured reference numbers live in
[docs/plan/08-measurements.md](docs/plan/08-measurements.md) and
[docs/plan/09-benchmarks.md](docs/plan/09-benchmarks.md).

- Worldgen: **0.92 ms/chunk** mean (p99 1.15), caves + ores included.
- Server simulation: tick p99 **≤0.03 ms** single-player baseline; 8-client load
  gate tick p99 **0.163 ms**, 20.0 TPS, RSS 22.3 MiB, 0 overruns >35 ms.
- Saves: 21 ms at 289 columns, 156 ms at 2,025 columns (synchronous save is the
  known pause gap below); v1–v4 migrations verified.
- Test suite: 733+ tests (including adversarial protocol/save/pack tests,
  network loopback survival tests, a full acceptance scenario and GPU probes).

## Known limitations (1.0.0)

- Crafting is **recipe-book based**; a spatial 3×3 grid is post-1.0.
- No torches/block lighting, farming, weather, beds, dimensions or redstone;
  mob spawning uses time-of-day and depth.
- Multiplayer has no TLS/auth/anti-cheat and client-authoritative movement;
  crafting is not yet available on servers (single-player only).
- Windows/macOS audio is a silent stub in 1.0; Linux audio is real.
- Whole-world saves are synchronous (autosave can cause a >50 ms tick spike on
  large worlds; 2,025-column save ≈156 ms).
- The 4-vCPU/8-GiB reference capacity claim, 16/50-player rows and async save are
  planned for the 1.0.x hardening line.
- Only the base 20 blocks / 52 items / 5 mobs / 25 recipes are shipped; the WASM
  modding track, shader packs beyond the built-in styles and controller support
  remain post-1.0.

The full 1.0 checklist, accepted limitations and audit results:
[docs/plan/1.0-scope.md](docs/plan/1.0-scope.md),
[docs/plan/1.0-acceptance.md](docs/plan/1.0-acceptance.md).

## Documentation

- [docs/README.md](docs/README.md) — specification, research and project plan.
- [docs/spec/01-technical-architecture.md](docs/spec/01-technical-architecture.md) — crate graph and architecture.
- [docs/spec/18-networking-multiplayer.md](docs/spec/18-networking-multiplayer.md) — protocol v2 and multiplayer design.
- [docs/admin/](docs/admin/README.md) — server quickstart, configuration, admin, security, backup, upgrade, troubleshooting.
- [CONTRIBUTING.md](CONTRIBUTING.md) — build, test and review workflow.

## Architecture at a glance

A Cargo workspace of 16 crates: `ob_math`/`ob_core` (foundation),
`ob_content`/`ob_world`/`ob_worldgen`/`ob_sim`/`ob_protocol` (headless data and
simulation), `ob_render`/`ob_audio` (presentation), `ob_server`/`ob_client`
(hosts), `ob_tools` (benchmarks/soak), plus `xtask`. The dedicated server is
GPU-free (enforced by the architecture guard and a CI dependency check); the
client and renderer never run on the server.

## Contributing

Contributions are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md); the
CI gates are `./dev-cargo fmt`, `clippy -D warnings`, workspace tests, rustdoc,
the architecture guard and the docs checker (see the workflow for the exact
commands). No third-party game assets may be committed; resource packs are
loaded from user-owned files only.

## License and legal

OpenBlock is dual-licensed under MIT or Apache-2.0 (see `LICENSE-MIT` /
`LICENSE-APACHE`). It is not affiliated with Mojang or Microsoft. Minecraft
resource packs are read at runtime from local paths selected by the user and are
never bundled or redistributed by OpenBlock; respect each pack's licence.

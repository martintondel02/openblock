# OpenBlock server operations

This is the operator handbook for the **interim 1.0 dedicated server**: the
headless `openblock-server` binary speaking plain TCP with length-prefixed JSON
(protocol v2). It is deliberately practical and describes what this build does,
not the QUIC/TLS design reserved for a later milestone.

> **Read [security](security.md) before opening a port.** The interim protocol
> has no TLS and no authentication. A player name is a label, not an identity.
> Bind to loopback, a trusted LAN, or a VPN/Tailscale network; do not port-forward
> 25565 to the public internet.

## Documents

| Document | What it answers |
| --- | --- |
| [Quickstart](quickstart.md) | Build/download, first smoke test, start, open a port, join, stop |
| [Configuration](configuration.md) | Every CLI flag and `server.toml` key, defaults, precedence, paths |
| [Administration](administration.md) | Console, whitelist, players, autosave, logs, TPS, world files |
| [Security](security.md) | Why this build must not be public, firewall/VPN/SSH-tunnel recipes |
| [Back up and restore](backup.md) | `server-backup.sh`, retention, verification, `server-restore.sh`, drills |
| [Upgrade](upgrade.md) | Pre-upgrade backup, binary/RPM/container swap, save format, rollback |
| [Troubleshooting](troubleshooting.md) | Symptoms, checks and fixes for ports, configs, saves, systemd |

Deployment-specific notes live next to the packaging:

* [systemd unit and manual install](../../packaging/systemd/README.md)
* [container and Compose](../../packaging/container/README.md)
* [Fedora RPM spec](../../packaging/fedora/openblock.spec) and
  [packaging overview](../../packaging/README.md)

## The one-minute version

```sh
# Build only the server (no GPU/audio/display dependencies)
cargo build --release --locked -p ob_server --bin openblock-server

# Prove the world layer works headlessly
./target/release/openblock-server --smoke --data-dir ./data --seed 42

# Serve on loopback
./target/release/openblock-server --data-dir ./data \
  --listen 127.0.0.1:25565 --max-players 8 --motd "My world"

# In another terminal, join with the desktop client
cargo run --release -p ob_client --bin openblock -- \
  --connect 127.0.0.1:25565 --name alice
```

The server logs a `starting TCP server on ...` line and then a five-second `tps`
line. `Ctrl-C` (or `stop` on the stdin console) disconnects clients, saves the
world and exits.

## What this build is and is not

| Capability | Interim 1.0 build | Notes |
| --- | --- | --- |
| Headless CPU-only server | Yes | No Vulkan, X11, Wayland, PulseAudio or ALSA dependency; verified by [`scripts/check-server-deps.sh`](../../scripts/check-server-deps.sh) |
| TCP JSON protocol v2 | Yes | 2 MiB frames; see [spec 18 §3.13](../spec/18-networking-multiplayer.md#313-interim-jsontcp-protocol-implemented-for-10) |
| Chunk interest streaming, movement, edits, chat, ping | Yes | 20 Hz tick, 4 chunks/tick/client, configurable interest radius |
| Survival state and entity replication | Yes, partial | Vitals/inventory/equipment, drops, mobs; client movement remains client-authoritative |
| Persistent network-player records | Yes | `network_players.txt`; the same name resumes the record, so names are not authentication |
| Whitelist by player name | Yes | Empty list means everyone; restart required |
| TLS, login/auth, encryption | **No** | Firewall/VPN/SSH tunnel only |
| Remote console/RCON, metrics/HTTP health, server browser | **No** | Console is stdin-only; the container healthcheck is a TCP connect |
| Built-in backup scheduler/world management | **No** | Use the shell scripts and systemd timer/cron |
| World-directory lock | **No** | Run exactly one server process per world |
| Live config reload | **No** | Every config change requires a restart |
| TLS/auth for players, anti-cheat, server-authoritative movement | **No** | Movement is validated but client-authoritative; see [spec 18 §3.13.9](../spec/18-networking-multiplayer.md#3139-server-authoritative-survival-protocol-v2) |

The normative target for later releases is
[spec 19](../spec/19-server-administration.md); where that document describes
QUIC, TLS, RPC, metrics or scheduled backups, those features are **not** in this
build. The interim sections of specs 18 and 19 are the contract for today.

## Support boundaries

* Linux x86_64 is the reference platform. The server binary is dynamically
  linked only against libc/libm on glibc distributions; the container build uses
  musl.
* The server is a foreground process. systemd, the container image, or your own
  supervisor provides restart-on-failure and log collection.
* No telemetry and no outbound connections are made by the server.
* World data is the operator's responsibility: schedule and test backups.

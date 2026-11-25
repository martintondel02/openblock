# Quickstart

This gets a world running on a Linux host and joins it from the desktop client.
The server itself needs **no GPU, display server or audio device**; only the
client does.

For a packaged deployment, jump to [systemd](#systemd-fedora--generic-linux) or
[container](#container). For a disposable local test, the build path below is
the shortest.

## 1. Get the binary

### Build from source (any Linux with Rust)

```sh
git clone https://github.com/martintondel02/openblock.git
cd openblock

# The workspace pins the stable toolchain in rust-toolchain.toml.
cargo build --release --locked -p ob_server --bin openblock-server

./target/release/openblock-server --version
```

Expected (version and protocol numbers advance between releases; save format 4
and protocol 2 are the interim 1.0 values):

```text
openblock-server <version> (save format 4, protocol 2, linux/x86_64)
save format: 4
protocol: 2 (interim JSON over TCP)
max frame: 2097152 bytes
networking: enabled with --listen ADDR; disabled by default
```

> The shared version banner and the dedicated `protocol:` line agree at
> protocol 2: the banner uses the engine-core constant and the `protocol:` line
> the wire-protocol constant, and a server integration test asserts the two
> values match. See
> [troubleshooting](troubleshooting.md#client-side-errors).

The server binary links only libc/libm on a glibc host. You do **not** need
Vulkan drivers, Wayland/X11 libraries, PipeWire, PulseAudio or ALSA to build or
run it. If you already have the client dependencies installed, nothing extra is
required; on a minimal server install `gcc`/`make` for the Rust linker.

### Fedora package

When the COPR/release RPM is available in your channel:

```sh
sudo dnf copr enable openblock/openblock
sudo dnf install openblock-server
openblock-server --version
```

The RPM installs the binary, hardened systemd unit, sysusers/tmpfiles entries,
firewalld service and `/etc/openblock/server.toml`. Continue with
[systemd](#systemd-fedora--generic-linux).

### Container

```sh
docker build -f packaging/container/Dockerfile -t openblock-server:local .
docker run --rm openblock-server:local --version
```

See [container](../../packaging/container/README.md) for Compose, volumes and
hardening.

## 2. Prove it works headlessly

`--smoke` generates a small world, ticks it, saves, reloads and verifies that the
state hash is identical. It never opens a listener.

```sh
rm -rf ./data
./target/release/openblock-server --smoke --data-dir ./data --seed 42
```

Expected tail (the hash depends on the exact build and seed):

```text
[INFO ] openblock_server: smoke test: generating, ticking, saving, reloading
[INFO ] openblock_server: smoke OK: state hash 33875a6597c2ce7b
smoke OK hash=33875a6597c2ce7b
```

The world is now at `./data/worlds/world/` with `world.obs` and its sidecars.

## 3. Start the server

For a local-only test, bind loopback:

```sh
./target/release/openblock-server \
  --data-dir ./data \
  --world world \
  --listen 127.0.0.1:25565 \
  --max-players 8 \
  --motd "My first OpenBlock world"
```

Expected:

```text
[INFO ] openblock_server: openblock-server <version> (save format 4, protocol 2, linux/x86_64)
[INFO ] openblock_server: networking: interim JSON/TCP protocol v2 (max frame 2097152 bytes)
[INFO ] ob_server::net: starting TCP server on 127.0.0.1:25565: world 'world' seed ..., max 8 players, interest 6 chunks, default mode survival
[INFO ] openblock_server: dedicated server listening on 127.0.0.1:25565 (max 8 players, 6 chunk interest, autosave 60 s)
[INFO ] openblock_server: commands: help | list | say <msg> | kick <id|name> [reason] | save | tps | stop
```

If you omit `--listen` (and no `listen` key is in a config file), the server
starts in **headless host mode**: it ticks and saves but never accepts TCP
connections. Its console commands are `status`, `save`, `stop`, `help`.

The first start generates the spawn area; the client may spend a few seconds
streaming chunks.

## 4. Open the port (only as far as you trust)

The interim protocol is plaintext and unauthenticated. Start with the local case
above, then choose one of these:

**Same machine or SSH tunnel (recommended for remote play):**

```sh
# On your workstation; keep the server bound to 127.0.0.1
ssh -N -L 25565:127.0.0.1:25565 user@server.example
# Then connect the client to 127.0.0.1:25565
```

**Trusted LAN only:** set `listen = "0.0.0.0:25565"` (or the LAN IP) in
`/etc/openblock/server.toml` and restart, then open TCP/25565 to that subnet:

```sh
# Fedora firewalld
sudo firewall-cmd --permanent --add-service=openblock-server
sudo firewall-cmd --reload

# Or restrict to one LAN interface/subnet
sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address=192.168.1.0/24 port port=25565 protocol=tcp accept'
sudo firewall-cmd --reload

# Ubuntu ufw equivalent
sudo ufw allow from 192.168.1.0/24 to any port 25565 proto tcp
```

**VPN/Tailscale:** bind to the VPN interface address (or `0.0.0.0` with a
firewall restricted to the VPN range) and connect over the VPN. Never publish
25565 on a public interface; see [security](security.md).

Check the listener:

```sh
ss -ltnp | grep ':25565'
# LISTEN 0 128 127.0.0.1:25565 0.0.0.0:* users:(("openblock-server",pid=...,fd=...))
```

## 5. Join with the client

The client needs a Vulkan-capable GPU and a Wayland/X11 session (see the
[README](../../README.md#platforms)). On the same machine:

```sh
cargo run --release -p ob_client --bin openblock -- \
  --connect 127.0.0.1:25565 --name alice
```

Or, with an installed client:

```sh
openblock --connect 127.0.0.1:25565 --name alice
```

The client validates the protocol, content fingerprint and block manifest, then
streams chunks. Multiplayer controls in this build: `T` chat, `Tab` player list,
left-click break, right-click place. `--name` is a display label only; there is
no login or password.

If the client reports `protocol mismatch`, `content mismatch` or
`Connection refused`, see [troubleshooting](troubleshooting.md).

## 6. Stop and inspect

In the foreground, type `stop` (or press `Ctrl-C`):

```text
> stop
[INFO ] openblock_server: stopping: disconnecting clients and saving the world
[INFO ] openblock_server: world saved; shutdown complete
```

Verify the save exists:

```sh
ls -lh ./data/worlds/world/
# world.obs  player.txt  inventory.txt  items.txt  mobs.txt
# block_entities.txt  equipment.txt  network_players.txt
```

Take a backup immediately; see [backup](backup.md):

```sh
scripts/server-backup.sh --world ./data/worlds/world \
  --dest ./backups --retention 7 --verify
```

## systemd (Fedora / generic Linux)

```sh
# Manual install from this checkout; the RPM does the same automatically.
sudo install -Dm755 target/release/openblock-server /usr/bin/openblock-server
sudo install -Dm644 packaging/systemd/openblock-server.sysusers \
  /usr/lib/sysusers.d/openblock-server.conf
sudo systemd-sysusers /usr/lib/sysusers.d/openblock-server.conf
sudo install -Dm644 packaging/systemd/openblock-server.tmpfiles \
  /usr/lib/tmpfiles.d/openblock-server.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/openblock-server.conf
sudo install -Dm644 packaging/systemd/openblock-server.service \
  /usr/lib/systemd/system/openblock-server.service
sudo install -Dm644 packaging/systemd/server.toml /etc/openblock/server.toml
sudo systemctl daemon-reload
sudo systemctl enable --now openblock-server

systemctl status openblock-server --no-pager
journalctl -u openblock-server -n 50 --no-pager
```

The packaged unit reads `/etc/openblock/server.toml`, stores the world under
`/var/lib/openblock`, and runs as the unprivileged `openblock` user. The
[systemd packaging README](../../packaging/systemd/README.md) covers changes to
the port, the missing-console limitation and `systemd-analyze` validation.

## Container

```sh
cd packaging/container
docker compose up -d --build
docker compose ps
docker compose logs -f openblock
```

The example publishes `127.0.0.1:25565` only. See the
[container README](../../packaging/container/README.md) for the volume,
`read_only`/`cap_drop` settings, backup path and image verification.

## Next steps

* [Configuration](configuration.md) — tune `server.toml` and the CLI.
* [Administration](administration.md) — console, whitelist, TPS, player files.
* [Backup](backup.md) — schedule verified snapshots.
* [Security](security.md) — understand the interim trust model before exposing a
  port.

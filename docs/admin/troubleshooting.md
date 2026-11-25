# Troubleshooting

Start with the three universal checks:

```sh
systemctl status openblock-server --no-pager
journalctl -u openblock-server -n 100 --no-pager
/usr/bin/openblock-server --version
```

For a foreground run, the last 20 lines of the terminal are usually enough. The
server logs one line per event; there is no separate log file or HTTP health
endpoint in this build.

> **Banner gotcha.** Some pre-1.0 builds print a stale protocol number in the
> first `--version`/startup banner line while the dedicated `protocol:` line (and
> the wire handshake) says 2. Trust the `protocol: 2` line; a protocol-mismatch
> rejection is still the real test.

## The service will not start

### `Failed to start` with a configuration error

```text
openblock-server: /etc/openblock/server.toml: line 7: unknown key 'server'
```

The interim config is a **flat** key/value file. Table headers (`[server]`),
unquoted strings, unknown keys and out-of-range values are rejected with a line
number and exit code 2. Compare against
[configuration](configuration.md#servertoml-flat-subset) or the packaged
[`server.toml`](../../packaging/systemd/server.toml), then:

```sh
sudo systemd-analyze --man=no verify /usr/lib/systemd/system/openblock-server.service
sudo openblock-server --config /etc/openblock/server.toml --no-listen --ticks 1 \
  --data-dir /var/lib/openblock
sudo systemctl restart openblock-server
```

### Unit starts and immediately stops

* `ConditionPathExists=/etc/openblock/server.toml` failed: the config is
  missing. Reinstall/copy the packaged file.
* The `openblock` user/group does not exist: apply sysusers
  (`sudo systemd-sysusers /usr/lib/sysusers.d/openblock-server.conf`).
* The binary is not at `/usr/bin/openblock-server` or is not executable:
  `ls -l /usr/bin/openblock-server`, reinstall the package/binary, then
  `systemctl daemon-reload`.
* `/var/lib/openblock` is not writable by `openblock`: inspect `ls -ldn`, fix
  ownership (`chown -R openblock:openblock`) or the mount options.
* `EnvironmentFile=/etc/openblock/server.env` has a syntax error: this file is
  optional; remove it or fix `KEY=value` lines.

Read the exact error:

```sh
journalctl -u openblock-server -b --no-pager | tail -50
systemctl show openblock-server -p ExecStart -p User -p StateDirectory -p ConditionResult
```

`systemd-analyze verify` on a workstation may report `/usr/bin/openblock-server`
missing because it is installed only on the server; that is not a unit bug.

## The server starts but nobody can connect

### It is in headless mode, not network mode

If the log says:

```text
[INFO ] openblock_server: commands: status | save | stop | help
```

there is **no TCP listener**. `--listen` was omitted and no `listen` key was
loaded (or `--no-listen` was passed). Fix `server.toml`:

```toml
listen = "0.0.0.0:25565"
```

restart, and look for:

```text
[INFO ] ob_server::net: starting TCP server on 0.0.0.0:25565: ...
[INFO ] openblock_server: dedicated server listening on 0.0.0.0:25565 ...
```

### Connection refused / no listener

```sh
ss -ltnp | grep ':25565'
# If empty, the process is not listening on that port/address.
pgrep -af openblock-server
journalctl -u openblock-server -n 30 --no-pager
```

Check that the client is connecting to the same address/port and that
`listen` is not bound to a different interface (for example `127.0.0.1` while
the client connects to the LAN IP). `0.0.0.0` covers all IPv4 interfaces;
`[::]` does not automatically mean the same policy on every host.

### Connection times out from another machine

* Firewall: `sudo firewall-cmd --list-all`, `sudo ufw status`; open TCP/25565
  only for the intended source range.
* NAT/cloud security group: allow inbound TCP 25565 from the VPN/LAN, not the
  world.
* Server bound to loopback or a VPN-only address; see the bind rules above.
* Routing/VPN is down; `ping`/`nc -vz server 25565` from the client.

```sh
nc -vz 192.168.1.10 25565
# Connection to 192.168.1.10 25565 port [tcp/*] succeeded!
```

### Port already in use

The server logs:

```text
[ERROR] openblock_server: could not start the network server on 0.0.0.0:25565: Address already in use
```

and exits 1. Find the holder and choose a different port or stop the other
process:

```sh
ss -ltnp | grep ':25565'
sudo systemctl stop openblock-server
# or change listen to 25566/tcp and open the new port
```

Do not run two servers against the same world: there is no world lock. Even on
different ports, two processes will overwrite each other's `world.obs`.

## Client-side errors

The client shows the server's machine-readable disconnect reason. Common ones:

| Reason | Meaning / fix |
| --- | --- |
| `protocol_mismatch` | Client and server are from different protocol generations (server protocol v2). Rebuild/update both from the same release |
| `content_mismatch` | Server `--pack`/block manifest differs from the client's registry. Use the same content pack or the default base registry on both |
| `server_full` | `max_players` reached; wait or raise it and restart |
| `name_taken` | Another online connection uses that name (case-insensitive); pick another or wait for the old connection to time out |
| `not_whitelisted` | The name is not in the configured whitelist; add it and restart |
| `invalid_name` | Name is empty, >32 bytes, or contains anything except ASCII letters/digits/`_`/`-`/`.` |
| `server_shutdown` | The server is stopping; reconnect after it restarts |
| `protocol_error: expected hello` | A non-OpenBlock client connected; check you are using the game client, not `nc`/HTTP |
| `Connection refused` | No listener/firewall (client-side message, not a server reason) |

If the client connects but the world stays empty, check the server streams
chunks (log `chunks` count) and the client's render distance/interest radius.
The client rejects a handshake before decoding chunks when the fingerprint or
block manifest differs, so content mismatch is the usual cause of an immediate
disconnect.

## Saves, crashes and corruption

### Restore the newest backup

```sh
sudo systemctl stop openblock-server
sudo scripts/server-restore.sh \
  --archive /var/backups/openblock/openblock-world-<timestamp>.tar.zst \
  --dest /var/lib/openblock/worlds/world --force --yes \
  --owner openblock:openblock
sudo systemctl start openblock-server
```

The restore script refuses unsafe archive members, checks the `OBSV` magic
before replacing anything, and preserves the old world as `world.bak-*`. See
[backup](backup.md) for verification and drills.

### `world.obs` exists but the server reports an error

Typical messages and responses:

* `unsupported save version N`: the world was written by a newer server.
  Restore the pre-upgrade backup and use the matching binary; do not edit the
  header.
* `world was saved with a different content pack (registry fingerprint ...)`:
  the server's `--pack` no longer matches the world. Restore the pack that
  created it, or start a new world; changing block order/types invalidates a v4
  save.
* `wrong magic`: the file is not an OpenBlock snapshot (truncated, zero-filled,
  or overwritten by another file). Restore from backup.
* `truncated`/`checksum`/`invalid world save`: the file is corrupt. The server
  will not start an empty world silently; restore the last verified archive.

Check the magic without starting the server:

```sh
head -c 4 /var/lib/openblock/worlds/world/world.obs; echo
# OBSV
```

### Stale `.tmp` files

The host writes each file via `file.tmp` + rename. A crash can leave
`world.obs.tmp`, `player.txt.tmp`, etc. The real file is the last complete
version and the loader ignores `.tmp`. With the server stopped, inspect and
remove only the stale temp after confirming the real file loads:

```sh
ls -l /var/lib/openblock/worlds/world/*.tmp 2>/dev/null || true
# Restore from backup if the real file fails to load; otherwise:
sudo rm -f /var/lib/openblock/worlds/world/*.tmp
```

Do not delete `world.obs` to "reset" a world unless you intend to lose it; move
the directory aside instead.

### Two processes / split brain

There is no lock file. Symptoms: random rollbacks, two different versions of the
world, `world.obs` timestamp jumping, or a port conflict. Check:

```sh
pgrep -af openblock-server
sudo ss -ltnp | grep ':25565'
```

Stop every instance before restoring or copying the world. If you use
containers, confirm only one compose project mounts the volume.

## GPU, audio and display questions

The dedicated server does **not** need a GPU, Vulkan driver, X11/Wayland,
PipeWire, PulseAudio or ALSA. If a package or guide asks you to install them for
the server, it is wrong.

```sh
ldd /usr/bin/openblock-server
# linux-vdso.so.1
# libm.so.6
# libc.so.6
# /lib64/ld-linux-x86-64.so.2

scripts/check-server-deps.sh
# check-server-deps: cargo graph clean (... no GPU/window/audio crates)
# check-server-deps: linkage clean (... no GPU/audio/display libraries)
```

The client is the opposite: it needs a working Vulkan device and a display
server. A "no adapter" error from the client is a client/GPU issue, not a server
issue.

## Performance and TPS

### What to look at

```sh
journalctl -u openblock-server -f | grep -E 'tps|overran|autosaved|save failed'
```

* `tps 20.0` is healthy. Sustained values below 20 mean the tick cannot finish
  in 50 ms.
* `tick overran the 50 ms budget` means a single tick was slow; occasional
  warnings during world generation are expected, repeated warnings are not.
* `last tick` is simulation time; socket and chunk encoding work happens on
  other threads, so a low tick number with high player latency points at I/O or
  bandwidth, not simulation.
* Check host CPU/steal (`top`, `vmstat 1`), disk latency (`iostat -x 1`) and the
  network. A full disk also shows up as `autosave failed`.

### Tuning order

1. Lower `interest_radius` (per-client chunk radius; default 6).
2. Lower `render_distance` (generation/streaming radius).
3. Lower `max_players` or ask players to spread out.
4. Increase `save_interval_seconds` if autosave I/O spikes.
5. Move the world to faster storage or a host with more CPU.

Restart after changes; there is no reload.

### How do I measure throughput under load?

Use the dependency-free harness against a running server:

```sh
# Fresh test world on a spare port, creative so placement succeeds
./target/release/openblock-server \
  --listen 127.0.0.1:25577 --data-dir /tmp/openblock-load \
  --world load --creative --max-players 8 --save-interval 0

# In another terminal
python3 scripts/load-test.py --clients 4 --seconds 10 \
  --port 25577 --scenario mixed --json /tmp/load.json
```

Expected shape on loopback (not a capacity guarantee):

```text
OpenBlock load test: 4 client(s), 10.0 s, scenario=mixed, 127.0.0.1:25577 (warmup 2.0 s)
connected: 4/4 handshakes, 4 joins, ~700-800 chunks, ~30 pongs
ping: p50=~45 ms p95=~100 ms p99=~150 ms ...
PASS: all clients connected, received chunks and pongs within the latency bound
```

The harness exits non-zero on missing handshakes/chunks/pongs, client errors,
unexpected disconnects, or p95 above `--max-p95-ms` (default 1000 ms). Use
`--scenario idle|movement|chat|interact|mixed`, `--clients N`, longer
`--seconds` for soak runs, and `--json` to record a result. `interact` placement
needs a creative server or a held block in survival; rejections are reported but
do not fail the run by themselves. See the docstring in
[`scripts/load-test.py`](../../scripts/load-test.py).

## Container-specific checks

```sh
docker compose ps                 # healthy? restarting?
docker compose logs --tail=100 openblock
docker inspect --format '{{.State.Health.Status}}' openblock-server
docker exec openblock nc -z 127.0.0.1 25565 && echo listen-ok
```

* `read_only: true` is fine; the only writable path is `/data`. If the server
  logs a permission error, the volume is owned by the wrong UID. The image uses
  UID/GID `10001`; for a bind mount `sudo chown -R 10001:10001 ./data`.
* A healthcheck failure means the TCP listener is not accepting connections; it
  does not prove the world loaded. Read the logs for a config/save error.
* The compose file publishes `127.0.0.1:25565` deliberately. Do not change it to
  a public bind without a VPN/firewall; see [security](security.md).
* The container has no shell-based console. Use `docker compose logs`, stop with
  `docker compose stop` (SIGTERM via `stop_signal`; the save path), and replace the image to upgrade.

## Collect a support report

There is no `diagnose`/support-bundle command in this build. Collect
deliberately, and review for IP addresses/player names before sharing:

```sh
{
  date -u
  uname -a
  /usr/bin/openblock-server --version
  systemctl show openblock-server -p ActiveState -p SubState -p ExecMainStatus -p User
  ss -ltnp | grep ':25565' || true
  ls -l /var/lib/openblock/worlds/world/
  head -c 4 /var/lib/openblock/worlds/world/world.obs; echo
  journalctl -u openblock-server -n 200 --no-pager
} > /tmp/openblock-support.txt 2>&1
```

Do not include `world.obs` or backups in a report; they are large and contain
player state. Redact IP addresses and chat as needed.

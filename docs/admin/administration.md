# Administration

This page covers day-two operation of `openblock-server`: starting and stopping,
the stdin console, player management, whitelisting, autosave, logs, TPS and the
files on disk. Deployment mechanics live in the
[systemd](../../packaging/systemd/README.md) and
[container](../../packaging/container/README.md) guides.

> There is no remote console, RPC or admin socket in this build. The console
> reads **stdin**, so under systemd (where stdin is `/dev/null`) the only admin
> actions are signals and restarting the service. Run the binary in a terminal or
> `tmux` when you need `list`/`say`/`kick`/`save`/`tps`.

## Start, stop and restart

### systemd

```sh
sudo systemctl start openblock-server
sudo systemctl status openblock-server --no-pager
sudo systemctl restart openblock-server
sudo systemctl stop openblock-server

journalctl -u openblock-server -f
journalctl -u openblock-server --since '1 hour ago' --no-pager
```

`stop` sends `SIGTERM`, which takes the graceful save path.
The server disconnects every client, writes a final save, logs `world saved;
shutdown complete`, and exits. `TimeoutStopSec=40` in the unit bounds the wait. Check that the world timestamp actually advanced:

```sh
systemctl show openblock-server -p ExecMainStatus -p ActiveState
ls -l --time-style=full-iso /var/lib/openblock/worlds/world/world.obs
```

### Foreground

```sh
openblock-server --config /etc/openblock/server.toml --data-dir /var/lib/openblock
```

`Ctrl-C` sends `SIGINT` and takes the same graceful path as `stop`/SIGTERM.
terminal attached if you need the console; use `tmux`/`screen` for a long-lived
interactive run. Do not run a second process against the same world.

> **Signal note.** This binary installs SIGINT and SIGTERM handlers through the
> `ctrlc` does not yet enable SIGTERM termination. A direct `kill -TERM` ends the
> process without a final save. The packaged systemd unit and container use
> SIGINT on stop so normal operations save correctly. Once the server installs a
> SIGTERM handler they can return to the systemd/Docker defaults; operator
> commands do not change.

### Container

```sh
docker compose stop openblock       # SIGINT (stop_signal) + up to 40 s grace
docker compose restart openblock    # restart, no config reload
docker compose logs -f openblock
```

The process is PID 1 in the container; the entrypoint is `exec`-form, so Docker's
stop signal reaches the server directly. The image sets `STOPSIGNAL SIGINT` and
the Compose example repeats `stop_signal: SIGINT` because this interim build's
graceful handler is SIGINT, not SIGTERM. There is no `docker exec ... console`
command in this build.

## Console commands

Available only when the server is running in the foreground with a usable stdin.
The network-mode console is:

| Command | Effect | Example output |
| --- | --- | --- |
| `help` | List commands | `commands: help, list, say, kick, save, tps, stop` |
| `list` | Online ids, names and positions | `  1 'alice' at 12.3,68.0,-4.1` |
| `say <msg>` | Broadcast a `server` chat line | `[INFO ] ob_server::net: <server> restarting in 5 minutes` |
| `kick <id-or-name> [reason]` | Disconnect one client; reason is sanitised | `kicked alice` |
| `save` | Save immediately | `saved` |
| `tps` | Current TPS, player count, tick count | `tps 20.0, players 1, ticks 12345` |
| `stop` / `quit` / `exit` | Disconnect all, save, exit | `stopping` |
| `<unknown>` | Warning only | `unknown command 'foo' (try help)` |

All console output is a single log line through the same logger as the rest of
the server (`OPENBLOCK_LOG` level applies). `kick` matches an exact id or a
case-insensitive name; the optional reason defaults to `kicked_by_admin` and is
shown to the client. A kick does **not** delete the player's persisted record.

When the binary is started **without** a listener (headless host mode), the
console is different: `status`, `save`, `stop`, `help`. Running
`openblock-server` with no `--listen` and no config `listen` is that mode, not a
network server with a broken port. Check the logs for `starting TCP server on`
versus `commands: status | save | stop | help`.

## Whitelist and players

The whitelist is a flat config array; see
[configuration](configuration.md#whitelist-semantics). A non-empty list accepts
only matching names:

```toml
whitelist = ["alice", "bob"]
```

Restart the server after editing. Rejected joins receive a `not_whitelisted`
disconnect reason, which the client shows. The list is not hot-reloaded.

Online player state is visible with `list` in the foreground. Player records are
persisted in `<world>/network_players.txt`; the same case-insensitive name
resumes the saved position, vitals, game mode and inventory. This is a
convenience record, **not** authentication: anyone who can reach the port can
claim an allowed name. See [security](security.md).

To remove an offline player record, stop the server, back up the world, edit
`network_players.txt` with the server stopped, and start it again. The file is a
header-free sidecar; malformed lines are warned about and skipped. Removing a
record is not required for a normal join; it only resets that name's saved state.
A corrupt sidecar cannot corrupt `world.obs`; restore the whole directory from a
backup if in doubt.

`network_players.txt` is written by `save`/autosave/shutdown, so a crash can lose
the most recent player actions (up to the autosave interval). Reduce
`save_interval_seconds` if that matters; there is no per-action durability
guarantee.

## Autosave, save and shutdown

* `save_interval_seconds` defaults to `60`; `0` disables periodic autosave.
* Autosave runs only when the server is dirty (for example a block edit, entity
  or inventory change, join/leave, or another persisted action) and the interval
  has elapsed. A player who only walks around may not trigger an autosave until
  another persisted change occurs; shutdown always saves the latest pose.
* `save` in the foreground and `SIGINT` (Ctrl-C, unit/container stop) request a
  full save. Do not send a bare `SIGTERM` to this build: it has no handler and
  will not save.
* The host writes each file with a temporary name and an atomic rename; a crash
  during one file leaves the previous complete file and a stale `.tmp` (ignored
  by the loader). The sidecar set is **not** a multi-file transaction, so a
  crash can leave a newer `world.obs` with an older player/inventory sidecar.
* There is no fsync-after-rename in this build. For a host that can lose power,
  use a filesystem with appropriate barriers and keep recent backups; do not
  treat a successful write as power-loss durable.

Watch autosave in the journal:

```sh
journalctl -u openblock-server -f | grep -E 'autosaved|save failed|shutting down'
```

Expected:

```text
[INFO ] ob_server::net: autosaved world 'world' at tick 2400
[INFO ] openblock_server: stopping: disconnecting clients and saving the world
[INFO ] openblock_server: world saved; shutdown complete
```

A failed autosave is logged as `autosave failed; previous save kept` and does not
stop the tick. Investigate disk space, permissions and `dmesg` immediately.

## Logs, TPS and performance

The server logs to stdout/stderr. There are no server-side log files and no JSON
log mode; systemd journald (or Docker's log driver) handles collection and
rotation. Set `OPENBLOCK_LOG=debug` temporarily for more detail, then return to
`info`.

A TPS line is emitted every five seconds while the network server runs:

```text
[INFO ] ob_server::net: tps 20.0, players 4, chunks 196, edits 32 (0 rejected), last tick 24.75µs
```

* Target is 20 TPS (50 ms/tick). A tick over 45 ms logs
  `tick overran the 50 ms budget`; sustained overruns show up as TPS below 20.
* `chunks` is the loaded world chunk count, not a per-client count.
* `edits` counts accepted/rejected block edits since start.
* `last tick` is the most recent tick's simulation duration; network I/O runs on
  separate threads and is not included.
* In the foreground, `tps` prints the current one-second measurement plus player
  and tick counts.
* Player joins/leaves and chat are logged at info. IP addresses are included in
  join/leave lines; chat text is logged with the sender.

Common tuning, cheapest first:

1. Lower `interest_radius` (fewer chunks per client; default 6).
2. Lower `render_distance` (less generation/streaming).
3. Lower `max_players`.
4. Increase `save_interval_seconds` if saves are causing I/O spikes.
5. Give the host more CPU; the tick loop is single-threaded, while generation
   and socket I/O use worker threads.

A rough local reference on loopback, not a capacity promise: 4 clients with the
`mixed` scenario sustained 20.0 TPS with 0 rejected edits and p50 ping around
45 ms, p95 under 100 ms (the 20 Hz server processes intents in tick batches).
See [scripts/load-test.py](../../scripts/load-test.py) and
[the load test section](troubleshooting.md#how-do-i-measure-throughput-under-load)
for how to reproduce and record numbers. Real capacity depends on world size,
interest radius, view distance and player behaviour.

## Files and process model

```text
<data-dir>/worlds/<world>/
  world.obs              block/world snapshot, save format 4, magic OBSV
  player.txt             internal host player (not a network player)
  inventory.txt          internal host inventory
  items.txt              dropped item entities
  mobs.txt               mob population
  block_entities.txt     furnaces/chests
  equipment.txt          internal host armor
  network_players.txt    persisted network players
```

* Exactly **one server process per world**. There is no lock file; two processes
  can open the same directory and overwrite each other's saves. Check before
  starting a second instance: `pgrep -af openblock-server`.
* The `--world` name selects the directory; changing it switches worlds (or
  creates a new one), it does not rename the old one.
* A world is a directory; copy/backup the whole directory, not just `world.obs`.
* Do not edit `world.obs` by hand. It is a compressed snapshot with a header and
  content fingerprint; manual editing will corrupt it. Use the client and a
  backup instead.
* A stale `*.tmp` file next to a world file is a failed atomic write; the real
  file remains the last good one. Stop the server and remove the `.tmp` only
  after confirming the real file opens (or restore a backup).

## Scheduling backups

There is no built-in backup scheduler. Use the host cron/systemd timer with
[`scripts/server-backup.sh`](../../scripts/server-backup.sh); the script stops
short of touching the service, so schedule a maintenance window or accept the
documented live-copy risk. Example timer:

```ini
# /etc/systemd/system/openblock-backup.service
[Unit]
Description=Back up the OpenBlock world
After=openblock-server.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/server-backup.sh --world /var/lib/openblock/worlds/world --dest /var/backups/openblock --retention 14 --verify
```

```ini
# /etc/systemd/system/openblock-backup.timer
[Unit]
Description=Daily OpenBlock backup

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

Enable with `systemctl enable --now openblock-backup.timer`. The RPM installs
the script as `/usr/bin/openblock-server-backup`; change `ExecStart` if you use
that copy. Stop the service for a fully consistent snapshot, or accept that a
live archive can pair `world.obs` with slightly older sidecars. See
[backup](backup.md) for retention, verification and restore drills.

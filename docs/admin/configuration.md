# Configuration

The interim server has a deliberately small surface: command-line flags and a
**flat** `server.toml` subset. Every config change requires a restart; there is
no live reload, no environment interpolation inside the file, and no generated
schema beyond `--help`.

Resolution order for every setting:

```text
command-line flag  >  server.toml value  >  built-in default
```

`--help` is the source of truth for the flags in your build; this page tracks the
interim 1.0 build. The server work in progress may add options (for example a
`game_mode` config key, which is already accepted below).

## Command-line flags

| Flag | Default | Effect |
| --- | --- | --- |
| `--listen ADDR` | **disabled** | Start the dedicated TCP server on `IP:port`, e.g. `127.0.0.1:25565`, `192.168.1.10:25565` or `[::1]:25565` |
| `--no-listen` | — | Ignore a `listen` value from the config file and stay headless; `--smoke`/`--ticks` also never listen |
| `--config PATH` | none | Load the flat `server.toml` at `PATH` |
| `--world NAME` | `world` | World directory name under `<data-dir>/worlds/` |
| `--seed N` | random; `0` for `--smoke` when unset | Seed used when creating a new world. An existing world keeps its saved seed |
| `--data-dir DIR` | platform data dir | Root for `worlds/`; see [paths](#paths) |
| `--render-distance N` | `6` | Host generation/streaming radius in chunks, clamped to 1–32 |
| `--interest-radius N` | `6` | Per-client chunk interest/send radius, 1–16; if omitted in the file it follows `render_distance` |
| `--max-players N` | `8` | Player cap, 1–500; extra joins get `server_full` |
| `--motd TEXT` | `A self-hosted OpenBlock world` | Handshake MOTD; control characters become spaces, trimmed and cut to 128 UTF-8 bytes |
| `--save-interval SECONDS` | `60` | Autosave when dirty; `0` disables periodic autosave; maximum 86400. Shutdown always saves |
| `--pack PATH` | built-in base | Load a JSON content pack on the server. The client must match the content fingerprint |
| `--survival` | default | Default game mode for newly joining network players |
| `--creative` | — | Creative default for network players; mutually exclusive with `--survival` |
| `--smoke` | — | Generate, tick, save, reload and verify a deterministic hash, then exit; no listener |
| `--ticks N` | — | Run N headless ticks, save and exit; no listener |
| `--version` | — | Print version, save format, protocol and frame limit |
| `--help` | — | Print the flag list and the `server.toml` key list |

Exit codes: `0` success, `1` host/save/network failure, `2` usage or
configuration error (including a malformed `server.toml`). `--smoke` exits
non-zero if the state hash changes across save/load.

Precedence examples:

```sh
# The file says listen = "0.0.0.0:25565"; keep it private for one debugging run:
openblock-server --config /etc/openblock/server.toml --listen 127.0.0.1:25565

# Ignore the file's listener entirely and run the headless tick loop:
openblock-server --config /etc/openblock/server.toml --no-listen

# CLI game mode wins over game_mode in the file:
openblock-server --config /etc/openblock/server.toml --creative
```

`--smoke` and `--ticks` are modes, not toggles: they take precedence over
`--listen` and exit after the requested work.

## `server.toml` (flat subset)

The parser is intentionally **not** a full TOML implementation:

* one `key = value` per line; blank lines and `#` comments are allowed;
* strings must be quoted (`"..."` or `'...'`); escapes supported in double
  quotes are `\n`, `\t`, `\r`, `\\` and `\"`;
* integers may use `_` separators (`1_234`);
* `whitelist` is an array of quoted strings (`["alice", "bob"]`; `[]` means no
  restriction);
* table headers such as `[server]` are rejected;
* unknown keys, unquoted strings and out-of-range values fail at start-up with
  a line number and exit code 2.

The complete accepted surface for this build:

```toml
# Identity / storage
world = "world"                 # world directory name under <data_dir>/worlds/
seed = 42                       # only used when the world is first created
data_dir = "/var/lib/openblock" # root containing worlds/

# Network
listen = "0.0.0.0:25565"        # omit or use --no-listen for headless mode
max_players = 8                 # 1..500
motd = "A self-hosted OpenBlock world"  # <= 128 bytes after sanitising

# Streaming / simulation
render_distance = 6             # host streaming radius, 1..32 (clamped)
interest_radius = 6             # per-client chunk radius, 1..16
save_interval_seconds = 60      # 0 disables autosave; max 86400

# Access
whitelist = []                  # empty = everyone; names are not identities
game_mode = "survival"          # "survival" or "creative"
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `world` | quoted string | `world` | Directory basename; avoid `/`, `..` and spaces |
| `seed` | unsigned integer | random | Ignored once the world exists; the saved world seed wins |
| `data_dir` | quoted string | platform data dir | Relative paths resolve from the server's working directory |
| `listen` | quoted string | none | Must parse as a socket address; `0.0.0.0` means all interfaces |
| `max_players` | integer | `8` | 1–500, checked again after file parsing |
| `motd` | quoted string | `A self-hosted OpenBlock world` | Sanitised/truncated, never an error for length |
| `render_distance` | integer | `6` | Clamped to 1–32 by the binary |
| `interest_radius` | integer | follows `render_distance`, else `6` | 1–16; larger values send more chunks and cost bandwidth |
| `save_interval_seconds` | integer | `60` | `0` disables periodic saves; shutdown still saves |
| `whitelist` | quoted string array | empty | Case-insensitive names; empty means no whitelist |
| `game_mode` | quoted string | `survival` | `survival` or `creative`; CLI wins |

A commented starting point is installed at
[`packaging/systemd/server.toml`](../../packaging/systemd/server.toml). The
container has its own `/data` data directory; see
[container configuration](../../packaging/container/README.md#configuration).

### `whitelist` semantics

* The list is matched case-insensitively against the requested player name.
* An empty list (or no key) allows any name. A non-empty list rejects everyone
  not listed with the `not_whitelisted` reason.
* There is no `/whitelist` command and no live reload: edit the file, restart the
  server, and tell affected players to reconnect.
* Because there is no authentication, a whitelist only prevents accidental
  joins; anyone who knows an allowed name can use it. Do not treat it as an
  access-control boundary on an untrusted network.

## Paths

The data directory is resolved in this order:

1. `--data-dir DIR`;
2. `data_dir` in `server.toml`;
3. `OPENBLOCK_DATA_DIR` environment variable;
4. `$XDG_DATA_HOME/openblock` (when non-empty);
5. `~/.local/share/openblock`;
6. `./run/data` as a last resort.

The world lives at:

```text
<data-dir>/worlds/<world>/
  world.obs              # block/world snapshot, magic "OBSV", save format 4
  player.txt             # host single-player state (not a network player)
  inventory.txt          # host inventory
  items.txt              # dropped item entities
  mobs.txt               # mob population
  block_entities.txt     # furnaces/chests
  equipment.txt          # host armor
  network_players.txt    # persisted network players (name, pose, vitals, inventory)
  spawn.txt              # persisted world-spawn point
```

The packaged systemd unit uses `--data-dir /var/lib/openblock` explicitly, so
`OPENBLOCK_DATA_DIR` is ignored there. The container entrypoint uses
`--data-dir /data`.

## Environment variables

| Variable | Values | Notes |
| --- | --- | --- |
| `OPENBLOCK_LOG` | `trace`, `debug`, `info`, `warn`, `error`, `off` | Read once at start-up; anything else falls back to `info`. Logs go to stdout/stderr and are collected by systemd/Docker |
| `OPENBLOCK_DATA_DIR` | directory | Only used if neither the CLI nor the config file supplies a data directory |
| `RUST_BACKTRACE` | `0`/`1`/`full` | Standard Rust panic backtrace switch; temporary diagnostics only |

There is no `RUST_LOG`, no config-file environment expansion, and no secrets or
tokens in this build. Player names, IP addresses and chat lines can appear in
logs; see [security](security.md#data-handling).

## Changing configuration safely

```sh
# 1. Edit a copy and parse it without starting a listener. A bad config fails
#    with a line number and exit code 2.
sudo cp /etc/openblock/server.toml /etc/openblock/server.toml.new
sudoedit /etc/openblock/server.toml.new
sudo /usr/bin/openblock-server --config /etc/openblock/server.toml.new --no-listen --ticks 1 \
  --data-dir /var/lib/openblock

# 2. Install and restart (restart is required for every key).
sudo install -m 0644 /etc/openblock/server.toml.new /etc/openblock/server.toml
sudo systemctl restart openblock-server

# 3. Confirm the listener and config values in the journal.
journalctl -u openblock-server -n 20 --no-pager
ss -ltnp | grep ':25565'
```

`--ticks 1` also writes the world; for a pure syntax check prefer a throwaway
copy of the data directory. The important part is that a bad file is rejected
before restarting the production service.

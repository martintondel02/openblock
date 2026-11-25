# systemd packaging for the OpenBlock dedicated server

This directory contains the distribution-neutral unit and identity files for
`openblock-server`. The Fedora RPM installs the same files; the container image
is separate and does not use systemd.

| File | Installed as | Purpose |
| --- | --- | --- |
| `openblock-server.service` | `/usr/lib/systemd/system/openblock-server.service` | Hardened service, unprivileged `openblock` user, `StateDirectory=openblock` |
| `openblock-server.sysusers` | `/usr/lib/sysusers.d/openblock-server.conf` | Creates the `openblock` user/group |
| `openblock-server.tmpfiles` | `/usr/lib/tmpfiles.d/openblock-server.conf` | Creates `/var/backups/openblock` (root, 0700) |
| `openblock-server.env.example` | `/etc/openblock/server.env.example` | Optional `OPENBLOCK_LOG` environment; copy to `server.env` to enable |
| `server.toml` | `/etc/openblock/server.toml` | Default flat config; edit for your world |

The unit runs the binary from `/usr/bin/openblock-server`, reads
`/etc/openblock/server.toml`, and keeps all writable state in
`/var/lib/openblock/worlds/<world>/`. It needs no GPU, display, audio device or
root privileges.

## Manual install (non-RPM hosts)

```sh
# 1. Build/install the binary. The release binary is statically linked enough
#    that only libc/libm are needed; see scripts/check-server-deps.sh.
cargo build --release --locked -p ob_server --bin openblock-server
sudo install -Dm755 target/release/openblock-server /usr/bin/openblock-server

# 2. Identity and directories
sudo install -Dm644 packaging/systemd/openblock-server.sysusers \
  /usr/lib/sysusers.d/openblock-server.conf
sudo systemd-sysusers /usr/lib/sysusers.d/openblock-server.conf
sudo install -Dm644 packaging/systemd/openblock-server.tmpfiles \
  /usr/lib/tmpfiles.d/openblock-server.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/openblock-server.conf

# 3. Unit and config
sudo install -Dm644 packaging/systemd/openblock-server.service \
  /usr/lib/systemd/system/openblock-server.service
sudo install -d -m 0755 /etc/openblock
sudo install -Dm644 packaging/systemd/server.toml /etc/openblock/server.toml

# 4. Optional environment (skip unless you need a different log level)
sudo install -Dm644 packaging/systemd/openblock-server.env.example \
  /etc/openblock/server.env.example

# 5. Start and inspect
sudo systemctl daemon-reload
sudo systemctl enable --now openblock-server
systemctl status openblock-server --no-pager
journalctl -u openblock-server -n 50 --no-pager
```

The first start creates `/var/lib/openblock/worlds/world/` and generates the
world. Joining locally needs no firewall change:

```sh
openblock --connect 127.0.0.1:25565 --name alice
```

To allow a trusted LAN/VPN only:

```sh
sudo firewall-cmd --permanent --add-service=openblock-server   # Fedora firewalld
sudo firewall-cmd --reload
# Or restrict the source explicitly, for example a Tailscale/WireGuard subnet:
sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address=100.64.0.0/10 port port=25565 protocol=tcp accept'
```

Do **not** port-forward 25565 from the public internet: the interim protocol has
no TLS and no authentication. See [docs/admin/security.md](../../docs/admin/security.md).

## Changing the listen address or port

`server.toml` ships with `listen = "0.0.0.0:25565"`; edit it and restart. The
firewalld service name `openblock-server` opens TCP/25565. If you change the
port, copy and edit `packaging/fedora/openblock-server.firewalld.xml` or use a
direct `--add-port=NNNNN/tcp` rule. Binding below 1024 additionally needs
`CAP_NET_BIND_SERVICE` in a unit drop-in; the default unit intentionally grants
no capabilities.

## Logs, console and graceful stop

* Logs go to the journal: `journalctl -u openblock-server -f`. The server logs a
  `tps` line every five seconds and an autosave line when it saves.
* The server's admin console reads **stdin**. Under systemd stdin is
  `/dev/null`, so `help`/`list`/`say`/`kick`/`save`/`tps` are only available when
  you run the binary in a terminal or tmux. This build has no remote console.
* `systemctl stop openblock-server` sends `SIGINT` because the unit sets
  `KillSignal=SIGINT`; this interim binary's graceful handler is Ctrl-C/SIGINT,
  while SIGTERM support is pending. The server disconnects clients, saves the
  world and exits. `TimeoutStopSec=40` bounds the wait. Do not override
  `KillSignal=SIGTERM` until the server handles it.
* Config changes always require a restart; there is no live reload.

## Validation

```sh
# Unit syntax (a stub binary is enough; no service is started)
sudo install -Dm755 /bin/true /usr/bin/openblock-server
systemd-analyze --man=no verify packaging/systemd/openblock-server.service
systemd-analyze security --offline=yes openblock-server.service  # optional score

# Config parse and world smoke test without opening a listener
/usr/bin/openblock-server --smoke --data-dir "$PWD/target/systemd-smoke" --seed 42
```

`systemd-analyze verify` may report warnings for features unavailable on the
host (for example `ProtectProc` on very old systemd); the Fedora CI job uses a
Fedora 44/45 container where all directives are supported.

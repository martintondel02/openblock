# Security

**The interim 1.0 server is not safe to expose to the public internet.** It has
no TLS, no authentication, no encryption in transit, and no remote
console authentication because there is no remote console at all. This page
explains the model, the supported private deployments, and the hardening that is
already in place.

The normative future design is in
[spec 18](../spec/18-networking-multiplayer.md) (TLS/QUIC),
[spec 19](../spec/19-server-administration.md) (TLS console, tokens, audit) and
[spec 20](../spec/20-security-and-anti-cheat.md). Those features are **not**
implemented in this build.

## What an attacker on the port can do

| Exposure | Consequence |
| --- | --- |
| Plain TCP JSON | Anyone who can capture traffic can read chat, positions, inventory and world edits |
| No authentication | Anyone who can connect may claim any name, including a whitelisted name |
| No account/identity | A name is a display label and a save-file key, not proof of identity |
| Client-authoritative movement | Clients report positions; the server validates finite values, world bounds, per-message distance and rate, but does not simulate anti-cheat physics |
| Chat rate-limited only | No moderation, mute or ban command in this build; player chat content is never logged by default (a debug line records the sender and message length only) |
| Chunk/edit validation limited | Server checks reach, world bounds, block validity, limits and support rules, but this is not an anti-cheat boundary |

**Privacy default.** Join/leave lines log the player name/id and reason only; the
build does not log or persist peer addresses. Debug logs add the sender and
length of a chat line, never its text (see [Data handling](#data-handling)).

A whitelist is useful to prevent accidental joins on a trusted LAN. It is **not**
a security boundary: an attacker who knows `alice` can join as `alice` and resume
her saved record. Do not rely on it for public access.

## Supported deployment patterns

Use the narrowest option that works for you:

### 1. Loopback plus SSH tunnel (recommended for remote play)

Keep the server on `127.0.0.1` and forward the port over SSH. No inbound game
port is opened and SSH provides authentication and encryption.

```sh
# Client machine:
ssh -N -L 25565:127.0.0.1:25565 user@server.example
openblock --connect 127.0.0.1:25565 --name alice
```

Use `-o ExitOnForwardFailure=yes` and a key-only SSH account for automation. The
server sees the connection as `127.0.0.1`; all player IPs are the tunnel.

### 2. VPN / Tailscale / WireGuard

Bind the server to the VPN address and restrict the port to the VPN interface.
Players need the VPN; the game port is not reachable from the internet.

```sh
# Show the Tailscale address on the host, then use it in server.toml:
tailscale ip -4
# listen = "100.x.y.z:25565"

# Firewalld: allow only the VPN source range on the VPN zone.
sudo firewall-cmd --permanent --zone=trusted --add-source=100.64.0.0/10
sudo firewall-cmd --permanent --zone=trusted --add-port=25565/tcp
sudo firewall-cmd --reload
```

For WireGuard, use its interface address (`wg show` / `ip addr show wg0`) and
allow TCP/25565 only on `wg0`:

```sh
sudo nft add rule inet filter input iifname "wg0" tcp dport 25565 accept
```

### 3. Trusted LAN only

Bind to the LAN IP or `0.0.0.0`, and allow only the local subnet. Understand that
everyone on that subnet can join and spoof names.

```sh
# firewalld
sudo firewall-cmd --permanent --add-rich-rule='rule family=ipv4 source address=192.168.1.0/24 port port=25565 protocol=tcp accept'
sudo firewall-cmd --reload

# ufw
sudo ufw allow from 192.168.1.0/24 to any port 25565 proto tcp
sudo ufw status numbered
```

### 4. Public exposure

Do not. If you must for a temporary test, treat it as a hostile sandbox:
dedicated VM/user, no other services, no reuse of personal names or passwords,
short lifetime, firewall source allowlist, frequent backups, and an isolated
network namespace or container. There is no DDoS protection, rate limiting
beyond per-connection token buckets, or abuse handling in this build. The
handshake limits concurrent handshakes to 32, but that is not a public-facing
defence.

## Firewall and bind checklist

```sh
# What is listening?
ss -ltnp | grep ':25565'

# What does firewalld think is open?
sudo firewall-cmd --list-all
sudo firewall-cmd --list-services

# Open only the packaged TCP service (Fedora), then narrow the source if needed.
sudo firewall-cmd --permanent --add-service=openblock-server
sudo firewall-cmd --reload
```

The packaged firewalld service opens **TCP/25565**, not UDP. If you change the
port in `server.toml`, change the firewall rule too. Never expose the port on a
public interface just because the unit starts successfully.

The server binds to the address in `listen`. `0.0.0.0` means all interfaces,
including public ones; `127.0.0.1` means local only; a specific LAN/VPN address
means only that interface. Choose deliberately and confirm with `ss`.

## What is hardened for you

### systemd unit

The packaged unit (`packaging/systemd/openblock-server.service`) runs as the
unprivileged `openblock` user, with:

* `NoNewPrivileges=true`, empty `CapabilityBoundingSet`/`AmbientCapabilities`;
* `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp=true`,
  `PrivateDevices=true`;
* `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`,
  `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible`;
* `RestrictSUIDSGID`, `RestrictRealtime`, `LockPersonality`,
  `SystemCallArchitectures=native`, address families limited to IPv4/IPv6/Unix;
* writable state limited to `/var/lib/openblock` via `StateDirectory` and
  `ReadWritePaths`; `UMask=0027`, `LimitNOFILE=65536`, `MemoryMax=8G`.

Config is `/etc/openblock/server.toml` (root-owned, not world-writable). Worlds
and sidecars live under `/var/lib/openblock` (mode 0750, owner `openblock`).
Backups created by the script are mode 0600 under `/var/backups/openblock`
(0700 root). See the [systemd guide](../../packaging/systemd/README.md).

If you bind below 1024, add `CAP_NET_BIND_SERVICE` in a drop-in; the default
unit deliberately has no capabilities. Don't run the service as root to avoid
that step.

### Container

The image runs as UID/GID `10001`, with `/data` as the only volume. The Compose
example sets `read_only: true`, `cap_drop: [ALL]`, `no-new-privileges` and
publishes only `127.0.0.1:25565`. Keep those settings; do not mount the Docker
socket, host `/etc`, or the whole host filesystem into the container. See the
[container guide](../../packaging/container/README.md).

### Data handling

* World directories and `network_players.txt` contain player names, positions,
  inventories and chat-adjacent activity. Protect them like any game save.
* Backups can contain the same data. Keep them mode 0600, restrict the backup
  directory, and delete expired copies according to your retention policy.
* Logs go to journald/Docker; at the default `info` level they include player
  names, the world name/seed, the bound listen address, join/leave/kick events,
  edit counts and errors. They do **not** include remote IP addresses or chat
  content. `OPENBLOCK_LOG=debug` adds the chat sender and message length (still
  not the text) and move-rejection diagnostics; treat debug logs as more
  sensitive and keep them short-lived. Apply the host's normal log retention and
  access controls. There is no built-in audit or redaction layer.
* The optional `--metrics-json PATH` snapshot is not an HTTP endpoint: it is a
  file created with the process umask containing the world name/seed, listen
  address, player counts and performance counters. Store it with mode
  `0600`/`0640` (the systemd unit sets `UMask=0027`; the container does not) and
  do not publish it.
* No credentials, tokens or private keys are created by this build. Do not put
  secrets in `server.toml`; it has no secret fields and is readable by the
  service account.

### Limits and rate limits

These bounds are the whole resource-exhaustion story in 1.0; they are not a
substitute for a firewall, but they limit what one connection can consume:

| Bound | 1.0 value |
| --- | --- |
| Players | `max_players` default **8** (1–500) |
| Frame size | **2 MiB** decoded cap; larger declared lengths are rejected before allocation |
| Handshakes | **32** concurrent, 10 s handshake timeout, 120 s idle read timeout |
| Outbound queue | **256** frames per client; a full queue drops rather than grows |
| Movement | 30 moves/s; finite/bounds/per-message distance checks only (client-authoritative) |
| Edits | 20/s, **7 m** server reach, loaded-column/block/support validation |
| Chat | 5 lines per 10 s, 256 characters; 32-character player names |
| Chunks | 16 requests/s, 16-chunk request radius, 6-chunk default interest, 4 chunks/tick/client |
| Persistence | 60 s autosave (configurable; `0` disables); shutdown always saves |
| Unit caps | `MemoryMax=8G`, `TasksMax=4096`, `LimitNOFILE=65536` |

The whitelist only filters accidental joins: because names are not
authenticated, anyone who knows an allowed name can claim it. Use the SSH
tunnel/VPN patterns above when the network is not fully trusted.

## Threat-model notes for operators

* **Name spoofing** is trivial. Use a VPN/allowlist and treat player records as
  convenience, not identity.
* **Griefing** is possible from anyone who joins. Back up before inviting people
  and keep the world directory recoverable.
* **Malicious packets** are decoded defensively (2 MiB frame cap, bounded chunk
  payloads, rate limits, timeouts), but the server is not fuzzed as a hardened
  internet daemon. Keep it behind a trusted network.
* **Resource exhaustion** is bounded by `max_players`, frame size, outbound
  queue limits (256 frames/client), per-op rate limits, `MemoryMax`, and
  `TasksMax`; a determined local user can still consume CPU/bandwidth. Monitor
  the host.
* **Supply chain** is out of scope here; build from a reviewed tag and keep the
  Rust toolchain/CI trusted.

## Future TLS and authentication

The target QUIC/TLS design, authenticated identities, remote-console token/TLS
and audit log are specified in specs 18–20 and planned beyond the interim
milestone. Until they ship, there is no configuration switch that makes public
exposure safe. A TCP reverse proxy in front of this server does not add
authentication: clients speak JSON/TCP directly, not HTTP or TLS. A generic TLS
terminator would break the protocol unless it is a transparent tunnel (for
example WireGuard/SSH), which is what the patterns above use.

## Security checklist before inviting players

- [ ] Server bound to loopback, a VPN address, or a LAN address — not a public
      interface.
- [ ] Firewall allows TCP/25565 only from the intended source range.
- [ ] `max_players` set to the expected group size.
- [ ] Whitelist enabled if the network is shared; remember it is spoofable.
- [ ] Backups scheduled and one restore tested ([backup](backup.md)).
- [ ] Host patched; SSH key-only; no unrelated services on the machine.
- [ ] World/backup directories readable only by the service account and admins.
- [ ] Players told that names are not accounts and that chat is not logged by
      default (debug records only the sender and message length).
- [ ] Server stopped before upgrades and backed up first ([upgrade](upgrade.md)).

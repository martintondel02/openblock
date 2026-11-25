# Security policy

OpenBlock 1.0 is a networked game: a desktop client, a headless dedicated server
and a runtime resource-pack loader. Security reports are taken seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub's
"Report a vulnerability" advisory flow on this repository, or contact the
project lead listed in [MAINTAINERS.md](MAINTAINERS.md).

Include, as far as you can: affected version or commit, component
(client / server / protocol / resource-pack loader / packaging), reproduction
steps, and impact.

The project is currently maintained by one person. Reports are handled on a
best-effort basis: we aim to acknowledge within 72 hours when possible, but we
do not promise a fix or advisory date. Confirmed issues are fixed as capacity
allows, and we coordinate disclosure with the reporter. Please do not publish
details before a fix or an agreed disclosure date.

## Supported versions

The supported line is **OpenBlock 1.0.x**. Only the latest 1.0.x release of the
client and dedicated server receives security fixes; pre-1.0 (`0.x`) builds are
unsupported. Patch releases keep save format 4 and protocol 2; a later 1.0.x
minor may add a save migration or a protocol revision, which the release notes
will call out. Save and network format compatibility follows
[Persistence](docs/spec/17-persistence.md).

## Scope: what 1.0 actually ships

The current attack surface is deliberately small. In scope:

- **Dedicated server:** remote crash, resource exhaustion, save corruption,
  unexpected file access from operator-supplied paths, and a bypass of a
  documented limit (`max_players`, frame size, per-op rate limits, handshake
  cap) that grants more access than the protocol intends. Player names are
  explicitly *not* authentication, so claiming an unauthenticated name or
  defeating a spoofable whitelist is a documented design limitation, not a
  vulnerability by itself.
- **Protocol:** malformed, truncated or oversized frames, decode-time memory
  amplification, frame amplification, rate-limit bypass, and identity spoofing
  that defeats a control the documentation claims is enforced.
- **Resource packs:** path traversal or symlink escape from a user-selected pack
  directory, malformed PNG/tRNS/animation input, memory exhaustion, or a
  panic/UB reachable from pack data. Packs are directories read at runtime; no
  archive/ZIP import exists in 1.0.
- **Client:** local file handling for screenshots, data directories, config and
  pack paths; windowing/GPU input handling. The game protocol client side is in
  scope.
- **Packaging and CI:** leaked credentials in the repository or artifacts,
  release-asset substitution, and workflow-token over-permission.

**Not present in 1.0:** there is no WASM/native mod runtime, account service,
updater/auto-update, telemetry or remote console/RCON. The mod-runtime,
updater, identity and account scope in earlier drafts of this policy describes
the post-1.0 plan; report an unexpected implementation as a security issue.

Out of scope: cheating in unranked multiplayer that has no server-side impact,
self-hosted server misconfiguration that does not affect the software,
vulnerabilities in third-party resource packs (report to that pack's author),
and anything requiring physical access.

## 1.0 hardening and operator guidance

- The dedicated server has **no TLS, authentication or encryption**. Bind it to
  loopback, a trusted LAN or a VPN/Tailscale network, or reach it over an SSH
  tunnel; do not port-forward 25565 to the public internet. See the
  [operator security guide](docs/admin/security.md) for the supported
  deployment patterns and the full limits table.
- Player names are display labels and save-file keys, not identities. The
  whitelist reduces accidental joins but is spoofable.
- At the default `info` level the server does not log remote IP addresses or
  chat content; a `debug` line records only the chat sender and character
  count. See [Data handling](docs/admin/security.md#data-handling) before
  sharing logs or the optional `--metrics-json` file.
- No credentials, tokens or private keys are created by the 1.0 build. The
  tracked repository files were reviewed for committed secrets as part of the
  [1.0.1 audit](docs/plan/1.0.1-audit.md).

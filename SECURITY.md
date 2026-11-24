# Security policy

OpenBlock ships a networked game with a dedicated server, a content-pack and
modding system, and a future account service. Security reports are taken
seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub's
"Report a vulnerability" advisory flow on this repository, or contact the
project lead listed in [MAINTAINERS.md](MAINTAINERS.md).

Include, as far as you can: affected version or commit, component
(client / server / protocol / mod runtime / content pack loader / updater),
reproduction steps, and impact.

The project is currently maintained by one person. Reports are handled on a
best-effort basis: we aim to acknowledge within 72 hours when possible, but we
do not promise a fix or advisory date. Confirmed issues are fixed as capacity
allows, and we coordinate disclosure with the reporter. Please do not publish
details before a fix or an agreed disclosure date.

## Supported versions

Until 1.0, only the latest released version of the client and the dedicated
server receives security fixes. Save and network format compatibility follows
[Persistence](docs/spec/17-persistence.md).

## Scope highlights

- Dedicated server: authentication bypass, remote crash, resource exhaustion,
  save corruption.
- Protocol: memory-safety bugs reachable from network input, packet amplification,
  replay, identity spoofing.
- Mod runtime: WASM sandbox escape, capability bypass, content-pack path
  traversal, native plugin loading of unsigned libraries.
- Mod and content-pack supply chain: a pack or mod that escapes the sandbox,
  bypasses a capability, traverses paths, exhausts memory or disk, mismatches a
  signature or hash, or is installed or updated from an untrusted source,
  including a typosquatted or dependency-confused package. 1.0 clients run no
  client-side mod or WASM code ([DEC-049](docs/decisions.md)); server and
  host-side mod execution is in scope.
- Updates and auto-updates: a forged, replayed or downgraded update manifest,
  a compromised update channel or signing key, TLS or signature-verification
  bypass, and updater/installer privilege issues on any supported platform.
- Asset/content loading: zipped pack exploits, malformed model/shader input.
- Client: browser/OS-level issues in the update mechanism.

Out of scope: cheating in unranked multiplayer without server-side impact,
self-hosted server misconfiguration that does not affect the software,
vulnerabilities in third-party mods (report to that mod's author), and anything
requiring physical access.

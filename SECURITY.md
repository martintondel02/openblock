# Security policy

OpenBlock ships a networked game with a dedicated server, a content-pack and
modding system, and a future account service. Security reports are taken
seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub's
"Report a vulnerability" advisory flow on this repository, or email the
maintainers listed in [MAINTAINERS.md](MAINTAINERS.md).

Include, as far as you can: affected version or commit, component
(client / server / protocol / mod runtime / content pack loader), reproduction
steps, and impact. We aim to acknowledge within 72 hours and to publish a fix
and advisory for confirmed issues.

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
- Asset/content loading: zipped pack exploits, malformed model/shader input.
- Client: browser/OS-level issues in the update mechanism.

Out of scope: cheating in unranked multiplayer without server-side impact,
self-hosted server misconfiguration that does not affect the software,
vulnerabilities in third-party mods (report to that mod's author), and anything
requiring physical access.

# ADR-0005 — Server-authoritative QUIC multiplayer

> Status: accepted · Date: 2026-11-24 · Decisions: DEC-008, DEC-024 … DEC-029

## Context

Multiplayer must support a self-hostable 24/7 world with untrusted clients,
eventually 50+ players per node, mods verified by content hash, and a friend-to-
friend "open to LAN/party" flow. That implies an authoritative server, a
transport with modern security and good behaviour under loss, and a netcode
model that feels responsive without ever trusting the client.

Weft, the closest prior art, takes the opposite bet for single-player-adjacent
play: client-authoritative movement with only a teleport check and client-side
world generation from the seed. That design leaks ungenerated ore and cannot
support fair PvP or public servers.

## Decision

1. **Authoritative host.** The dedicated server or integrated server is the only
   authority; clients send inputs. Client-side prediction and server
   reconciliation cover movement; complex modded behaviour may be
   server-only-with-correction.
2. **QUIC (quinn 0.11 + rustls 0.23)** behind a `Transport` trait:
   - unreliable **datagrams** for input frames, tick snapshots and events,
   - independent **uni streams** for chunk/world streaming and container
     traffic,
   - a bidi **control stream** for join/handshake/admin.
3. **Self-signed Ed25519 identity by default**, SPKI pinning + shareable
   tickets; ACME and an optional central account service are opt-in (DEC-026).
4. **20 Hz tick** default, 30 Hz server option; bit-packed hot-path codecs;
   content-hash handshake (DEC-027).
5. **Interest-managed, progressive join** with per-player bandwidth caps and
   precompressed chunk caches (DEC-025).

## Consequences

- Cheating is bounded by validation rather than by client software, and the
  server can log replay evidence (DEC-029).
- QUIC gives TLS 1.3, migration and multiplexing without writing a reliability
  layer, at the cost of per-connection memory and CPU that must be tuned and
  measured (quinn's defaults are bulk-transfer defaults, not game defaults).
- Datagrams avoid retransmitting obsolete state; per-chunk streams prevent a lost
  chunk from stalling the game. Mixing them incorrectly is the classic failure
  mode and is forbidden by the spec.
- Progressive join is mandatory: a 512-block-radius world is tens of MB
  compressed, so joins are rate-capped with a loading UI and optional HTTP
  download.
- Self-signed certificates are the only zero-configuration default; trust-on-
  first-use is a documented UX trade-off, and browsers/WebTransport are blocked
  until a real-certificate path exists.
- iroh-based friend-to-friend play is a transport implementation, not a fork of
  the architecture.

## Alternatives considered

- **Raw UDP + custom reliability** — more control, much more work and risk;
  kept as an implementation of `Transport` if quinn's cost model disappoints.
- **TCP** — rejected for gameplay state (head-of-line blocking) but acceptable
  for optional world download.
- **WebSocket/WebTransport first** — deferred; browsers cannot pin self-signed
  certificates and the web client is post-1.0.
- **P2P-first** — rejected as the architecture: no authority, no persistence, no
  anti-cheat. Friend-to-friend remains a feature.
- **Minecraft protocol compatibility at launch** — rejected (DEC-005 rejected
  alternatives): pins the data model and the only Rust implementation is stale.

## References

- [Networking research](../research/04-networking-sota.md)
- [Networking chapter](../spec/18-networking-multiplayer.md)
- [Security and anti-cheat](../spec/20-security-and-anti-cheat.md)

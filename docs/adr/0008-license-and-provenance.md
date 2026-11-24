# ADR-0008 — MIT OR Apache-2.0, no Minecraft or Weft code

> Status: accepted · Date: 2026-11-24 · Decision: DEC-009

## Context

OpenBlock is a Minecraft-like game written from scratch, taking inspiration
from the MPL-2.0-licensed Rust engine Weft. Two legal questions dominate the
project's ability to exist publicly:

1. **What license should our code use?** It must allow mods, servers, forks,
   commercial hosting and embedding, while being attractive to contributors and
   compatible with the Rust ecosystem.
2. **How do we avoid copying Minecraft's protected expression or Weft's
   copyleft code?** Minecraft's code, art, audio, names and trade dress are
   protected; Weft's files are MPL-2.0 (file-level copyleft). Meanwhile, game
   mechanics, genre conventions and facts are generally not copyrightable.

The research note on licensing and IP records the detailed analysis (and states
clearly that it is not legal advice).

## Decision

1. **Code: MIT OR Apache-2.0** (Rust convention). Apache-2.0 provides an express
   patent grant; MIT keeps GPLv2-only projects compatible.
2. **Assets: CC BY 4.0** for original art/audio, **CC0 1.0** for placeholders,
   **OFL 1.1** for fonts. Every asset must have a recorded license and source,
   enforced by a check in CI.
3. **DCO 1.1, inbound = outbound.** No CLA initially.
4. **Clean-room provenance policy:**
   - No Minecraft code, assets, sounds, fonts, names or data tables are copied,
     decompiled or transcribed.
   - Weft is read for concepts and measured numbers only. No Weft file, comment
     or data table is copied; any copied file would remain MPL-2.0 and could not
     be relicensed.
   - Implementations are written from behavioural descriptions and independent
     design, and PRs describe techniques in our own words.
5. **Trademark policy:** "OpenBlock" is a working name; a clearance search is a
   launch gate (DEC-037). The logo is copyright-reserved. No Mojang marks are
   used; the README carries a clear non-affiliation disclaimer.
6. **Dependency policy:** `cargo-deny` allows permissive licenses plus MPL-2.0
   dependencies with notices; GPL/AGPL/SSPL/BUSL/Commons-Clause are denied.
   Binary-only SDKs (FSR/XeSS/DLSS) are never vendored (DEC-016).

## Consequences

- The code stays maximally reusable: mods can be any license, servers can be
  commercial, forks can be proprietary.
- We carry the cost of writing our own content and art instead of "adapting"
  Minecraft's; this also makes the game legally defensible and artistically ours.
- We must maintain provenance hygiene continuously (PR checklist, asset
  manifests, dependency audits, ADR/decision citations), not as a one-off.
- MPL-2.0 dependencies must have notices and source offers; `cargo-about` output
  ships with the binary.
- The name may change before 1.0 if clearance fails; the mod namespace and repo
  URL would migrate with documented redirects.

## Alternatives considered

- **MPL-2.0 for our code** — rejected: file-level copyleft discourages some
  commercial forks and mod embedding without a benefit to our goals.
- **GPL-3/AGPL** — rejected: restricts the server-hosting and modding ecosystems
  we want, and AGPL creates ambiguity for hosted game servers.
- **Public domain / Unlicense** — rejected: no patent grant, poor enterprise and
  contributor signalling.
- **Copying Weft code and staying MPL** — rejected: contaminates the permissive
  dual license and the project's provenance story.

## References

- [Legal and licensing research](../research/07-legal-licensing-and-ip.md)
- [LICENSE-MIT](../../LICENSE-MIT), [LICENSE-APACHE](../../LICENSE-APACHE)
- [CONTRIBUTING](../../CONTRIBUTING.md), [SECURITY](../../SECURITY.md)

# Changelog

All notable changes to OpenBlock are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres
to [Semantic Versioning](https://semver.org/) from `1.0.0` onward. Pre-1.0
releases may change APIs, save formats and the protocol between minors; see
[docs/plan/00-roadmap.md](docs/plan/00-roadmap.md) §2.

## [Unreleased]

### Added

- Project specification (`docs/spec/`), research (`docs/research/`), roadmap and
  milestones (`docs/plan/`), decision register (`docs/decisions.md`) and ADRs
  (`docs/adr/`).
- Cargo workspace skeleton for the crate topology in DEC-003, CI, license and
  provenance policy.
- **Fedora-first platform decision (DEC-054)**: Fedora 44/45 x86_64 (Wayland,
  Vulkan, PipeWire) is the primary platform and reference distribution; Flathub
  Flatpak, COPR RPM, a hardened Fedora Server RPM/systemd/firewalld package, and
  required `fedora:44`/`fedora:45` CI jobs. Packaging scaffolding in `packaging/`.
- Linux-first platform section in the README, Fedora build prerequisites, and
  Fedora-specific requirements in spec/02, 03, 14, 19, 21, 22, 24 and 25.

### Changed

- **Documentation re-baseline after the 2026-11-24 review board.** The plan is now
  two tracks: 1.0 Early Access at Tier-1 parity (~200–240 engineering
  dev-weeks) and full Tier-2 parity at 2.0 (`MS-17`, ~500–800 dev-weeks). The
  previous 159-dev-week total was 5–8× below the project's own research
  inventory. Roadmap effort/calendar/content/critical-path sections, milestones,
  MVP, work breakdown and risks are reconciled; the README separates 1.0 from
  2.0 and post-1.0.
- **DEC-051:** the parity gate is defined once as two numbers — 100% of in-scope
  checks pass, and ≥ 85% vanilla coverage from a hand-authored behavioural
  checklist. The "≥ 90% coverage" pass claim is retired; coverage and pass rate
  are reported separately, and full Tier-2 parity is 2.0, not 1.0.
- **DEC-048:** 32³ render mesh clusters are the MS-04 default (16³ remains the
  storage/edit/light/network unit); OPEN-001 is closed.
- **DEC-049:** 1.0 clients run no mod or WASM code; mod UIs use server-computed
  data. OPEN-006 is closed and the client mod CPU budget is a reserved envelope.
- **DEC-050:** translations are not part of the join-critical hash; `content_hash`
  is strict and `view_hash` is advisory unless the server opts into strict view
  matching.
- **DEC-052:** dedicated servers default to a 256-block render distance equal to
  simulation distance; 512 blocks is opt-in with a published cost, and capacity
  claims must state co-located versus distributed players.
- **DEC-053:** the base pack uses original dimension and entity names
  (`openblock:underworld`, `openblock:terminus`); Minecraft names do not appear in
  shipped content identifiers or base-pack data.
- Milestone detail: MS-01 light budget corrected to 8 KiB per mixed section, MS-04
  cluster default, MS-11 scoped to Tier-1 depth, MS-14 redefined as Tier-1 parity
  plus corpus infrastructure and Tier-2 coverage reporting, MS-15 split into
  co-located and distributed capacity rows, MS-16 redefined as the Early Access
  launch, and `MS-17` added for the 2.0 track.
- Documentation index now lists `research/10-custom-content-and-differentiation.md`
  and `plan/07-review-findings.md`; ADR and milestone counts are corrected.

No playable build exists yet; the first one is milestone `MS-03` (`0.0.1`).

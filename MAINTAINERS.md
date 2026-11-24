# Maintainers

## Current maintainers

| Role | Name | Areas |
| --- | --- | --- |
| Project lead and interim maintainer for all areas | Martintondel ([@martintondel02](https://github.com/martintondel02)) | product, architecture, roadmap, engine core, rendering, simulation, content, modding, networking, server operations, security, provenance, legal/trademark, releases |

There are no other appointed maintainers yet. Until they are appointed, the lead
owns every area in the table. The list is deliberately not padded with `TBD`
rows: an empty row would imply an area has an owner when it does not.

## Responsibilities

Maintainers:

- review pull requests and guard the normative documents and the decision
  register;
- enforce DCO sign-off on every commit and the clean-room, naming and asset
  provenance rules;
- triage security reports under [SECURITY.md](SECURITY.md), coordinate fixes and
  advisories, and keep the published response expectations honest;
- handle Code of Conduct reports confidentially and appoint independent
  moderators when a report involves a maintainer;
- maintain `REUSE.toml`, `deny.toml`, license notices, `architecture.toml` and
  this file, and cut and sign releases.

## Appointment and review

New maintainers are appointed after sustained, high-quality contributions and a
track record of good review judgment. While the project has one maintainer the
lead appoints; once it has two or more, appointment is by consensus of the
existing maintainers.

Once the project has at least two maintainers, **security- and
provenance-sensitive changes require a second human reviewer**. That includes
authentication and sandbox code, the updater and signing keys, `ob_net` and
`ob_modding` security boundaries, the DCO and license files, `REUSE.toml`,
`deny.toml`, `architecture.toml` and asset manifests. The lead is not exempt.

### Solo-maintainer exception

Until a second maintainer is appointed, there is no second human reviewer. Such
changes may be merged by the lead only with the security or provenance rationale
written in the pull request and the exception recorded there as
`solo-maintainer exception; second review pending`. No automated agent, bot or
AI reviewer counts as the second human. The exception ends when a second
maintainer is appointed; changes merged under it remain open to later audit.

## Contact

Use GitHub issues for anything public. Security reports follow
[SECURITY.md](SECURITY.md). Code of Conduct reports go to the project lead
listed above.

# Review findings — final specification review board (2026-11-24)

> Status: normative planning record · Last updated: 2026-11-24
> Reviews: `docs/spec/00–25`, `docs/plan/00–06`, vision, README, legal/provenance files.
> Result: **all blocker findings fixed in the same pass**; majors fixed or carried as tracked open decisions.

## 1. Why this document exists

Before any engine code was written, the full specification was reviewed by four
independent reviewers (technical depth, networking/content depth, product and
planning, adversarial/legal). This document records what they found, what was
changed, and what remains deliberately open. It is the audit trail for the
re-baseline that followed: promising things we cannot build is worse than
promising less and delivering it.

The review was documentation-only (there is no playable build yet), so findings
concern design correctness, internal consistency and planning honesty.

## 2. Headline findings

| # | Severity | Finding | Disposition |
| --- | --- | --- | --- |
| 1 | **blocker** | **Effort model off by 5–8×.** The roadmap budgeted 159 dev-weeks for Tier-2 parity + modding + visuals + servers + launch, while the project's own inventory estimated Tier-2 alone at 190–300 person-months. | **Re-baselined** into two tracks: 1.0 Early Access (Tier 1, 200–240 dev-weeks) and 2.0 full Tier-2 parity (additional 500–800 dev-weeks, funded 6–10 person team). DEC-051; roadmap §11, vision §6/§8, README, milestones. |
| 2 | **blocker** | **Four conflicting definitions of the parity gate** ("all features", "Tier 2", "≥90% coverage", "≥90% pass"). | **One definition:** 100% of in-scope checks pass, plus ≥85% coverage of a hand-authored behavioural checklist; full Tier-2 parity is a 2.0 promise. DEC-051; DEC-022 retitled. |
| 3 | **blocker** | **Replication distance contradicted render distance** (interest set = simulation distance 256, but chunks up to a 512-block radius were documented as networked), making the 50-player capacity claim impossible. | **Interest set = render distance**; dedicated servers default to 256 blocks (= simulation distance); 512 is opt-in with published cost; capacity claims state co-located vs distributed players. DEC-052; ch05, ch18, ch19, ch25, vision, milestones. |
| 4 | **blocker** | **Per-section meshing could not meet its own cull budget** (101,400 sections at a 512-block radius against a 0.30 ms visibility budget). | **2×2×2 / 32³ mesh clusters are the default from MS-04** (~12,700 clusters), 16³ remains the storage/edit/light/network unit. DEC-048; ch03, ch25, ADR-0004. |
| 5 | **blocker** | **Sky-light seeding bug**: the algorithm re-seeded every transparent cell below an opaque roof at level 15, fully lighting sealed caves. | **Fixed algorithm**: carry the running level down with `saturating_sub(attenuation)`; `max(1, attenuation)` applies only to lateral propagation; sealed-room and sky-shaft fixtures added. ch07. |
| 6 | **blocker** | **Performance CI could not detect FPS regressions**: comparison ignored `direction`, so a 15% FPS drop passed, defeating the mandatory negative control. | **Direction-normalised comparison** with directional CI tails and a negative-control statement. ch25 §4.3. |
| 7 | **blocker** | **The save format lost world time and weather**: `level.json` had no `total_ticks`/`day_time`/`day_length`/weather state, so scheduled ticks were rebased onto an unknown clock and the day cycle reset. | **World-state section added** to `level.json`, tick-delay rebasing defined, included in the save manifest hash and migration fixtures. ch17. |

## 3. Major findings and dispositions

| Finding | Disposition |
| --- | --- |
| Scene catalogue contradictions (missing `redstone_stress`/`ui_stress`, mismatched durations) | `perf/scenes.toml` is the single catalogue; chapters link to it; a budget naming an absent scene fails the manifest load. |
| Client-mod model simultaneously forbidden and budgeted | DEC-049: **no client mod code at 1.0**; mod UIs use server-computed data; client budgets are reserved post-1.0 envelopes. ch13, ch16, ch25, ch14. |
| `content_hash` / `view_hash` ambiguity; translations would block joins | DEC-050: content hash is join-critical and strict; view hash is advisory by default; translations live in the view hash only. ch15, ch18, ch19, ch23. |
| Tick packet fragments (4.64 KB max) vs 16 KB/tick budget; entity bandwidth 52 KB/s vs 20 KB/s cap | 16-fragment / 1,160 B framing inside the 16 KB budget; entity traffic modelled at ~60 of 200 entities changing per tick (15.6–18 KB/s), with priority deferral. ch18, ch25. |
| Missing protocol messages (scoreboard, advancements, boss bars, effects, equipment, world border, abilities) | Generic typed `StateChannel` message with channel registry added. ch18. |
| Inventory expected-state CRC32C forgeable by a malicious client | Keyed BLAKE3-128 expected-state digest (or exact slot comparison); CRC32C demoted to corruption detection. ch11. |
| Save format could not round-trip modded multi-state blocks | State dictionary entries are `(block id, canonical property word)`; unknown content round-trips as opaque placeholders. ch17. |
| Backup hard-link premise wrong (region files are mutable after commit) | Backups are reflink/copy/OS snapshot; hard links explicitly unsupported. ch17, ch19. |
| Piston entity displacement order disagreed between chapters | One ordered list owned by ch12; ch10 cites it; two-free-face fixture added. |
| Scheduled-tick sequence persisted in one chapter, reassigned in another | Canonical `(due_tick, packed block key, kind)` order with no persisted sequence; equal-due save/load test. ch05, ch12, ch17. |
| Cross-region redstone determinism test asserted two opposite things | Region deferral is unconditional (partitions defined even for one worker), so on/off hashes match. ch12. |
| Generator fingerprint omitted the code version, so terrain fixes could silently mix chunks | `generator_version` added to the fingerprint descriptor with a bump rule and a DEC-047 warning test. ch06. |
| Light memory stated as 2 KiB/section instead of 8 KiB (2 B/block on 16³) | All figures corrected; the non-uniform-section assumption is stated wherever a total depends on it. ch05, ch07, ch25, milestones, WBS. |
| 1% low ≥ 90 FPS paired with p99 ≤ 12 ms (arithmetically 83.3) | p99 tightened to 11.1 ms at 120 FPS and 8.3 ms at 144 FPS, keeping the 1000/p99 definition. ch25, ch03, manifests. |
| 144 FPS "performance" preset undefined in the settings schema | Explicit preset row added (0.85 render scale, one 1024² cascade at 48 blocks, etc.). ch03, ch04, ch25. |
| 1024-block 60 FPS promised at 1.0 with no delivering milestone | Moved to post-1.0 (MS-20); marked Report, not a 1.0 gate. ch25, vision, README, milestones. |
| Minecraft proper nouns in shipped identifiers (`openblock:nether`, `openblock:end`) and a parity checklist described as generated from registry dumps | DEC-053: original names (`openblock:underworld`, `openblock:terminus`); DEC-051: hand-authored checklist from published facts, never a copied registry dump. ch05, ch06, vision, README, CONTRIBUTING, ADR-0008. |
| Architecture manifest did not match the documented schema and missed the server→worldgen edge | `architecture.toml` rewritten as explicit per-crate edges with `gpu_free`; the checker parses it and `cargo metadata`; ch01 documents exactly that schema. |
| DCO required but no DCO file or check | `DCO` + license text added, referenced from CONTRIBUTING/PR template; PR DCO check added to CI. |
| Maintainer/identity placeholders and an unsupported solo review rule | Interim lead ownership documented; solo-maintainer exception for second-review rules; security response is best-effort until staffing exists. |
| Success metrics not measurable under the no-telemetry rule | Crash-free sessions redefined as an opt-in crash-signature rate; community metrics labelled aspirations with post-1.0 baselines. vision §8, plan/06. |
| Docs index stale (missing research/10, plan/07, wrong milestone counts) | Index regenerated against the actual file set. |

## 4. Deliberately deferred (tracked, not forgotten)

| Item | Owner | Where tracked |
| --- | --- | --- |
| Metal 64³ cluster escalation (DEC-004 caps clusters at 32³) | MS-04 | OPEN-009 |
| Dynamic-light binning pass has no GPU budget row | MS-04/MS-12 | OPEN-010 |
| Anti-cheat enforcement defaults and auto-ban policy | MS-15 | OPEN-011 |
| Ore obfuscation as an opt-in anti-cheat setting | MS-15 | OPEN-012 |
| Performance waiver policy | MS-00 | OPEN-013 |
| Release channels/support windows become normative | MS-16 | OPEN-014 |
| Unified incident severity scale | MS-16 | OPEN-015 |
| Per-PR gated benchmark scene subset | MS-02 | OPEN-016 |
| Anti-cheat evidence retention/access | MS-15 | OPEN-017 |
| Forward-compatibility promise start date | MS-13 | OPEN-018 |
| 2.0 staffing and funding | Project lead | OPEN-019 |
| Modern Delta: re-baseline parity to the then-current Minecraft release before 2.0 | MS-17 | OPEN-020 |

## 5. What this review changed about the promise

The repository still targets the same destination — a fast, deeply moddable,
server-first voxel sandbox with Minecraft's feature set as the floor — but the
promises are now separated by release:

- **1.0 Early Access** (Track 1): creative + survival Tier 1, multiplayer and
  self-hosted servers, the modding platform, modern visuals, and the parity
  corpus infrastructure. Estimated 200–240 engineering dev-weeks plus content
  and QA. Solo full-time is a 4.5–5.5 year project; a three-person team is
  roughly 20–23 months plus risk allowance.
- **2.0** (Track 2): full Tier-2 parity (all vanilla-scale blocks, items, mobs,
  biomes, structures, mechanics) as an explicit 500–800 dev-week programme with
  a funded 6–10 person team.

This is the honest version of "all the features of Minecraft". Anything less
than saying it out loud would have been the plan's biggest risk.

## 6. Next review gates

| Gate | When | What must be true |
| --- | --- | --- |
| Architecture review | MS-00 exit | CI gates, architecture guard, benchmark harness run; the DEC register has no contradictory entries |
| Playability review | MS-03 exit | A stranger can play the vertical slice; the playable-bar tests pass |
| Performance review | MS-04 exit | 512-block radius 120/144 FPS gates measured on the bench box, negative control passing |
| Security review | MS-06 exit | Protocol fuzz corpus, threat model walkthrough, no high-severity findings |
| Scope review | MS-14 exit | Tier-1 in-scope pass rate 100%, hand-authored coverage reported, 2.0 estimate re-measured |

## 7. Open questions

1. **Where exactly does Track 1 land?** The 200–240 dev-week figure is a
   bottom-up scoped estimate with ±30% uncertainty; it is re-measured at MS-03
   and again at MS-06. Owner: project lead.
2. **2.0 staffing and funding** must be secured before parity work starts, or
   the Tier-2 promise stays a 2.0 intent rather than a commitment.
   → OPEN-019.
3. **Modern Delta timing**: before 2.0, re-baseline the parity reference to the
   then-current Minecraft release (the pinned 1.21.11 baseline will be years old
   by then). → OPEN-020.
4. **Hand-authored checklist weights**: the ≥85% coverage floor only means
   something once behaviour-family weights exist; they are MS-14 work.
   Owner: MS-14.
5. **Benchmark hardware**: the performance gates need a dedicated bench box and
   a self-hosted runner; hosting is not yet budgeted. Owner: MS-00/MS-04.

## 8. References

- [Decision register](../decisions.md) — DEC-048 … DEC-053 and OPEN-009 … OPEN-020
- [Roadmap](00-roadmap.md) §11 (two-track effort model) and [milestones](01-milestones.md)
- [Minecraft feature inventory](../research/05-minecraft-feature-inventory.md) —
  baseline counts used to re-baseline the plan
- [Custom content research](../research/10-custom-content-and-differentiation.md) —
  differentiation evidence and flagship scoping
- [Risks](05-risks.md) — the register's post-review exposure

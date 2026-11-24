# Risk Register

> Status: normative planning - Last updated: 2026-11-24
> Depends on: [DEC-009](../decisions.md), [DEC-022](../decisions.md), [DEC-034](../decisions.md), [DEC-035](../decisions.md), [DEC-036](../decisions.md), [DEC-037](../decisions.md), [DEC-048](../decisions.md), [DEC-051](../decisions.md), [DEC-052](../decisions.md), [roadmap](00-roadmap.md), [milestones](01-milestones.md), [team and process](04-team-and-process.md)
> Implements: MS-00 onward; the risk gate at every milestone, the MS-16 launch sign-off and the escalation rules of §7. Re-scored against the two-track re-baseline in [review findings](07-review-findings.md).

## 1. Purpose and how to use this register

This is the canonical risk register for OpenBlock. A risk that matters to the project appears here
once, with an owner milestone, a leading indicator and a plan for both outcomes. Chapters may keep
a local risk table next to the design it belongs to; those local IDs are provisional and map to
this register in §8. Where a chapter and this register disagree, this register wins on likelihood,
impact and ownership, and the chapter keeps the technical detail.

The register has three jobs:

1. **Make the top of the list visible.** A solo or small team cannot hold forty risks in
   mind. §5 is the ten that get reviewed every week; the rest are reviewed monthly.
2. **Turn opinions into triggers.** "The renderer might slip" is not actionable; "MS-04 exit slips
   three weeks, or GPU p95 stays above 8 ms after two optimisation passes" is.
3. **Price the risk.** Each row carries an estimated schedule impact in dev-weeks so the roadmap
   can be re-baselined with a number instead of a feeling.

**2026-11-24 review re-baseline.** The review board found the plan's 159 dev-week total to be
5–8× below the project's own research inventory and re-based it as two tracks: 1.0 Early Access at
**200–240 dev-weeks** and 2.0 Tier-2 parity at **~500–800** (roadmap §11,
[review findings](07-review-findings.md)). Several rows below are re-scored against that change:
RISK-011 is now a Tier-1-at-1.0 plus 2.0-funding risk, RISK-004 is mitigated by DEC-048, RISK-001's
schedule impact is re-priced, and RISK-010 must state its player distribution (DEC-052). RISK-013
and RISK-018 are unchanged in cause and larger in consequence.

**Status vocabulary.** `open` (live, not yet triggered), `monitoring` (trigger partially visible),
`occurred` (trigger fired; a contingency or recovery plan is running), `mitigated` (the cause is
gone; kept for history), `retired` (no longer relevant; reason recorded). Risks are never deleted,
only retired.

## 2. Scoring method

**Likelihood (L)** is the chance the trigger fires before the owner milestone exits:
1 rare (< 10%), 2 unlikely (10–30%), 3 possible (30–60%), 4 likely (60–85%), 5 near-certain (> 85%).

**Impact (I)** is the worse of schedule, quality and trust damage:
1 negligible (< 1 dev-week), 2 minor (1–2), 3 moderate (3–6), 4 major (7–12, or a release slip),
5 severe (> 12 dev-weeks, project-threatening, or irreversible trust/legal damage).

**Exposure** is `L × I` (1–25). The band drives the response:

| Exposure | Band | Response |
| ---: | --- | --- |
| 20–25 | Critical | Stop-the-line review at the next weekly; the lead decides within 48 h whether the roadmap changes |
| 15–19 | High | On the weekly watchlist; mitigation has a named owner and a date |
| 8–14 | Medium | Monthly review; mitigation funded inside the milestone |
| 1–7 | Low | Accepted; reviewed when the owner milestone starts |

**Schedule impact (Δ)** is the estimated delay in dev-weeks **if the risk occurs and the mitigation
does not prevent it**, before a contingency. It is a planning estimate (±50%), not a measurement,
and it is not additive: several risks share the same recovery work.

## 3. Risk matrix

Rows are impact, columns are likelihood; cells list risk IDs and bold marks the critical band
(exposure 16 or more).

| Impact \ Likelihood | 1 rare | 2 unlikely | 3 possible | 4 likely | 5 near-certain |
| --- | --- | --- | --- | --- | --- |
| **5 severe** | — | 006, 008, 015, 032, 040 | 005, 018 | 013 | — |
| **4 major** | — | 023, 024, 030, 031, 035 | 002, 010, 014, 016, 019, 020, 034, 037 | **001, 003, 009, 012** | **011** |
| **3 moderate** | — | 029, 036 | 007, 021, 022, 025, 026, 027, 028, 033, 038, 039 | 017 | — |
| **2 minor** | — | — | — | — | — |
| **1 negligible** | — | — | — | — | — |

The shape is the message: the severe-impact risks are mostly unlikely, while the critical
band is dominated by scope and people risks that are likely and slow-moving. The project is more
likely to be ended by **011** (parity scope) or **013** (burnout) than by any single bug.

## 4. The register

`Cat` is T technical, S schedule, Sc scope, L legal, C community, Sec security, Q quality.

### 4.1 Risk summary

| ID | Risk | Cat | L | I | E | Δ dev-wk | Owner | Status |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| RISK-001 | The custom renderer takes longer than planned, delaying Phase B and every dependent milestone; the two-track re-baseline carries the overrun instead of hiding it | S | 4 | 4 | 16 | 8–12 | MS-04 | monitoring |
| RISK-002 | The 120/144 FPS gate at 512 blocks is missed, forcing a public re-baseline of the headline promise | T | 3 | 4 | 12 | 4–8 | MS-04/MS-15 | open |
| RISK-003 | Netcode "feel" needs iteration: corrections, rubber-banding or input lag at 60–100 ms RTT | T | 4 | 4 | 16 | 5–6 | MS-06 | open |
| RISK-004 | 16³ storage or the 32³ mesh cluster is revisited late, reopening save, mesh and perf work (mitigated: DEC-048 settles the 32³ cluster default and 16³ storage is fixed) | T | 2 | 5 | 10 | 8–12 | MS-04 | mitigated |
| RISK-005 | Determinism drifts across platforms, forcing fixed-point simulation maths (OPEN-002) | T | 3 | 5 | 15 | 8–12 | MS-02/MS-08 | monitoring |
| RISK-006 | A save is corrupted or lost in the field, destroying the product's most valuable promise | Q | 2 | 5 | 10 | 4–6 | MS-13 | open |
| RISK-007 | A wgpu or GPU-driver change regresses Metal or Vulkan p95 on a supported platform | T | 3 | 3 | 9 | 2–4 | MS-04/MS-13 | open |
| RISK-008 | A WASM mod escapes the sandbox or exhausts host resources | Sec | 2 | 5 | 10 | 4–8 | MS-09 | open |
| RISK-009 | The mod ecosystem does not adopt the API, leaving the differentiation promise unproven | C | 4 | 4 | 16 | 4–8 | MS-09/MS-16 | open |
| RISK-010 | Server scale or hosting cost misses: 50 **co-located** players per 4 vCPU, $0.02 per player-hour; the distributed row is lower by design | T | 3 | 4 | 12 | 3–6 | MS-15/MS-07 | open |
| RISK-011 | Parity scope and content velocity outrun the team: Tier-1 in-scope parity at 1.0 or the funded Tier-2 track at 2.0 is not reached | Sc | 5 | 4 | 20 | 15–25 | MS-14/MS-17 | open |
| RISK-012 | Art or audio production blocks a release, or the commissioned pack does not arrive in time | S | 4 | 4 | 16 | 6–10 | MS-12/MS-16 | open |
| RISK-013 | Solo-founder burnout or bus factor stops the project | S | 4 | 5 | 20 | 12–24 | Lead/MS-16 | open |
| RISK-014 | Trademark clearance fails and the project must rename before 1.0 (DEC-037) | L | 3 | 4 | 12 | 2–4 | MS-16 | open |
| RISK-015 | Minecraft/rightsholder legal pressure: takedown, trademark claim or asset dispute | L | 2 | 5 | 10 | 4–12 | Lead/MS-16 | open |
| RISK-016 | AI-generated contributions bring copied code, hallucinated claims or unreviewable volume | Q | 3 | 4 | 12 | 3–6 | MS-00 | open |
| RISK-017 | Hiring or staffing shortfall leaves a track unowned | S | 4 | 3 | 12 | 8–16 | Lead | open |
| RISK-018 | Funding or runway ends before 1.0 | S | 3 | 5 | 15 | 12–24 | Lead | open |
| RISK-019 | A community, UGC or moderation incident damages trust or creates liability | C | 3 | 4 | 12 | 2–6 | MS-15/MS-16 | open |
| RISK-020 | Mod API churn breaks the ecosystem the project depends on | C | 3 | 4 | 12 | 4–8 | MS-09/MS-16 | open |
| RISK-021 | A critical dependency (wgpu, wasmtime, quinn) breaks, is abandoned or ships a CVE | T | 3 | 3 | 9 | 2–5 | MS-00/MS-13 | open |
| RISK-022 | Benchmark infrastructure becomes flaky or gameable, so the perf gate stops meaning anything | Q | 3 | 3 | 9 | 2–4 | MS-00/MS-04 | open |
| RISK-023 | Localization or accessibility is retrofitted late and forces UI rework | Q | 2 | 4 | 8 | 3–6 | MS-05/MS-14 | open |
| RISK-024 | The integrated server and the dedicated server diverge, so single-player rots | T | 2 | 4 | 8 | 3–6 | MS-03/MS-07 | open |
| RISK-025 | Specification and code drift apart, and the docs stop being a contract | Q | 3 | 3 | 9 | 2–5 | MS-00 | open |
| RISK-026 | Anti-cheat produces false positives or misses obvious cheats, harming fair play | Q | 3 | 3 | 9 | 2–5 | MS-15 | open |
| RISK-027 | DDoS or abuse takes a public server down or makes hosting unattractive | Sec | 3 | 3 | 9 | 1–3 | MS-15 | open |
| RISK-028 | Storefront certification, notarisation or code signing delays launch | S | 3 | 3 | 9 | 2–6 | MS-16 | open |
| RISK-029 | The Android port cannot hold 30 FPS or passes Play Store review late | T | 2 | 3 | 6 | 4–8 | MS-19 | open |
| RISK-030 | An asset's licence or provenance is wrong, forcing a recall or relicensing | L | 2 | 4 | 8 | 2–5 | MS-00/MS-12 | open |
| RISK-031 | World height changes after the format freeze (OPEN-005), or 1,024 blocks is adopted late | T | 2 | 4 | 8 | 3–6 | MS-01/MS-13 | monitoring |
| RISK-032 | The save format must change after `0.1`, breaking the migration promise | Q | 2 | 5 | 10 | 4–8 | MS-13 | open |
| RISK-033 | A launch-day operations incident: bad update, CDN failure or crash spike | S | 3 | 3 | 9 | 1–4 | MS-16 | open |
| RISK-034 | Post-1.0 flagship content expands beyond its gate and pulls engineers off parity or fixes | Sc | 3 | 4 | 12 | 6–12 | MS-24 | open |
| RISK-035 | A privacy incident from crash reports, logs or telemetry | L | 2 | 4 | 8 | 2–5 | MS-16 | open |
| RISK-036 | Architecture erosion: the GPU-free server rule or the platform boundary is bypassed | T | 2 | 3 | 6 | 3–6 | MS-00 | open |
| RISK-037 | An item duplication or economy exploit reaches a public server | Q | 3 | 4 | 12 | 3–6 | MS-10/MS-15 | open |
| RISK-038 | A physics or collision correctness bug reaches players: tunnelling, clipping, vehicles that derail | Q | 3 | 3 | 9 | 2–4 | MS-10 | open |
| RISK-039 | The self-signed default trains players to click through identity warnings, enabling a man-in-the-middle | Sec | 3 | 3 | 9 | 1–3 | MS-07 | open |
| RISK-040 | A release signing key is compromised or lost, blocking updates or shipping a malicious build | Sec | 2 | 5 | 10 | 1–4 | MS-16 | open |

### 4.2 Trigger, mitigation and contingency

| ID | Trigger / early warning | Mitigation (prevention) | Contingency (if it fires) |
| --- | --- | --- | --- |
| RISK-001 | MS-04 slips three weeks, or GPU p95 stays above 8 ms after two optimisation passes | Vertical slice first; render graph before effects; presets as the tuning surface; buy UI/audio; weekly frame-time dashboard; the 200–240 dev-week 1.0 track is the re-baselined budget and carries an 8–12 week allowance | Ship 384 blocks at the 120 FPS gate, cut shadows/post to `medium`, publish measured numbers; `ash` backend only with a > 20% measured gap (DEC-002) |
| RISK-002 | Two consecutive nightly gates red; p99 above 10 ms for a week | Budgets from MS-04 (DEC-035); bench box; per-pass attribution; early preset work | Re-baseline the default distance with data (OPEN-003), make 144 FPS a mode, publish the honest matrix |
| RISK-003 | More than one correction per 10 s of sprinting at 80 ms RTT; input-to-photon penalty above one frame | In-process network simulator from day one; prediction and lag compensation in MS-06; recorded feel sessions | Cut fast features (boats, item entities, elytra) before cutting prediction; cap supported RTT; document the correction budget |
| RISK-004 | The MS-04 cluster default fails the 512-block visibility budget (> 0.30 ms) or corner-edit latency exceeds 100 ms | 32³ cluster default settled by DEC-048; 16³ storage fixed; the MS-04 benchmark tunes implementation, eviction and the Metal fallback (OPEN-009) | Per-section fallback behind a setting; re-run the memory and draw-call budgets; escalate the Metal path only with measured data |
| RISK-005 | Cross-platform hash mismatch in nightly; drift after a toolchain or dependency bump | Pinned transcendentals, integer worldgen, golden hashes, three-OS CI, negative tests | Fixed-point maths for the divergent subsystem only (OPEN-002); freeze features until green |
| RISK-006 | Any CRC or quarantine event in soak; a migration fixture failure; `kill -9` test failing | Double-buffered headers, CRC per record, 1,000-iteration fault injection, migrations on copies, backups | Halt rollout, ship `world repair`, publish recovery steps; rollback requires the pre-update backup |
| RISK-007 | macOS or Vulkan p95 regression above 10% after a driver or wgpu bump | Pin wgpu and drivers on the bench box; per-adapter baselines; nightly macOS smoke | Hold the old wgpu for a release; ship a platform-specific submission path; mark macOS a known issue |
| RISK-008 | Sandbox escape reproduced; wasmtime advisory; quota bypass in fuzz | Capabilities, fuel/memory/epoch quotas, pinned wasmtime, external review before `0.4`, escape corpus | Disable the WASM tier by config and in the container; emergency patch and advisory; preserve evidence |
| RISK-009 | Fewer than 20 external packs after `0.4`; template-to-block time above 30 min | SDK, template, docs, examples, base game dogfooding, content index, showcase packs | Invest in first-party content built on the API; commission seed mods; simplify the API surface |
| RISK-010 | 50 co-located bots below 20 TPS; cost per player-hour above $0.02; RSS above budget; no measured distributed row | Parallel region ticking, interest tuning, replication batching, published capacity matrix with co-located and distributed rows (DEC-052) | Publish a lower supported count for each distribution (the distributed row is lower by design), raise the recommended vCPU, lower the default render distance |
| RISK-011 | Tier-1 in-scope pass rate below 100% or behavioural coverage below 85% at MS-14; more than two unfixed deltas per upstream drop; no funded 2.0 plan by MS-16 | Pinned 1.21.11 baseline (DEC-022), hand-authored checklist (DEC-051), tier gates, cut lines, two-track plan with the 2.0 track costed | Ship 1.0 Early Access with a documented Tier-1 gap, defer Tier-2 content to 2.0, never claim full parity; publish the delta |
| RISK-012 | Under 60% of block textures final at MS-12; music not delivered by MS-14 | Placeholder pack at MS-03; commission in Phase D; CC0 fallback; style guide; asset register | Ship with the placeholder/CC0 subset and add art in `1.1`; state it in the release notes |
| RISK-013 | Two consecutive missed weeks; devlog gap over three weeks; stated loss of motivation | Public milestones, playable builds, scope tiers, weekly cadence, 20% slack, delegate early | Publicly pause the roadmap, recruit a co-maintainer, move to maintenance and LTS the last release |
| RISK-014 | Clearance search shows a conflict; domain or org dispute; store rejection on name grounds | Clearance as a 1.0 launch gate (DEC-037); reserve the mod namespace; migration plan | Rename before 1.0 with redirects, reserve the new namespace and a migration note; rebrand marketing |
| RISK-015 | Any C&D or trademark complaint; store takedown; press coverage framed as a clone | Clean-room provenance, original names/art, disclaimer, counsel review, no Minecraft compatibility | Engage counsel, swap contested content or name, remove the compatibility gateway from scope, communicate |
| RISK-016 | Large unreviewed AI PRs; copied code detected; hallucinated citations in docs; AI-attributed regression | DCO, disclosure trailers, review rules, CI gates, provenance and similarity scans, three-open-PR cap | Revert, tighten the policy, freeze AI-assisted work on security paths, publish a correction |
| RISK-017 | Fewer than two FTE after six months; a track has no owner for four weeks | Good-first-issues, docs, maintainer ladder, paid contractors for art/QA, public roadmap | Reduce to Tier 1 plus modding, extend the timeline, switch to the solo plan |
| RISK-018 | Less than six months of runway; no grant or sponsorship pipeline | Grants, sponsorships, no VC and no open-core, low burn, paid support only if it does not fork the product | Part-time mode, pause contractors, ship an earlier `1.0-lite` and continue on the community plan |
| RISK-019 | First harassment, doxxing, spam or DMCA incident; server-list abuse; pressure for voice chat | Code of conduct, report tooling, curated server list, no voice at 1.0, default-off UGC sharing | Close the server list, add moderators, disable chat features, publish an incident note |
| RISK-020 | Two breaking minor API bumps in a quarter; flagship pack needs an API edit | API 1.0 freeze, two-minor deprecation window, compatibility tests, base-game dogfooding | LTS branch with backports, migration guide, direct help to flagship modders |
| RISK-021 | Upstream breaking release, CVE, or an unmaintained crate in the tree | Pin versions, `cargo-deny`/`cargo-vet`, traits at the boundary, vendoring policy | Fork and patch, hold the version for a release, use the `ash` escape hatch |
| RISK-022 | More than one flaky perf gate per week; players report stutter while the gate is green | Bench box, noise floor, hidden counters, three scenes, artifact retention, adversarial review | Mark the gate `Warn` for at most seven days with a repair owner, then re-baseline with a DEC |
| RISK-023 | More than 5% of strings hard-coded at MS-14; RTL layout breaks; screen-reader audit fails | Fluent from day one, string lint, pseudo-locale in CI, font fallback, a11y checklists | Ship fewer locales, fix the critical screens, defer narrator depth past 1.0 |
| RISK-024 | A bug reproduces only on one server path; `cargo tree` guard bypassed | One `ob_sim`, CI dependency guard, loopback tests, shared integration test | Block the release until the parity test is green; refactor before adding features |
| RISK-025 | Code PR without a spec update; broken links; stale numbers quoted in issues | Docs in the same PR, link check, spec review, generated protocol docs | Documentation sprint before release; freeze the spec at the release tag |
| RISK-026 | False-positive rate above 0.1% on the legitimate corpus; cheater bot undetected for 5 s | Server validation, replay evidence, configurable enforcement, scripted cheat corpus | Log-only mode, manual review queue, patch, publish the policy change |
| RISK-027 | Server down from floods; handshake exhaustion; bandwidth bill spike | Rate limits, QUIC Retry, connection caps, per-IP quotas, upstream filtering guidance | Upstream DDoS protection, proxy, temporary allowlist, status page |
| RISK-028 | Notarisation or signing failure; store review hold; Steam Deck verification fail | Signing infrastructure early, CI packaging, requirement checklists | Ship GitHub/itch first, Steam later; macOS from source as the fallback |
| RISK-029 | Renderer cannot hold 30 FPS on the mobile reference; touch UI late; Play policy rejection | Post-1.0 milestone, mobile preset and budgets, safe areas and 48 dp targets from the start | Delay `1.3`, ship tablets only, skip Play Store initially |
| RISK-030 | Asset without a register row; similarity flag; missing CC-BY attribution | Asset register, CI licence check, REUSE metadata, similarity review, placeholders | Replace the asset, re-license, publish a correction and credit |
| RISK-031 | Content needs more than 384 blocks; 1,024 adopted after the format freeze | Engine ceiling 1,024, per-dimension height field, memory measured before freeze | Per-dimension height with a migration; delay the format freeze rather than force a rewrite |
| RISK-032 | A migration fixture fails after a data-model change; a released save cannot open | Format freeze at MS-13, migrations from `0.1`, fixture saves in the repo | Migration plus a published repair tool; keep the last compatible branch for reads |
| RISK-033 | Updater serves a bad build; CDN down; crash spike on launch day | Staged rollout, health check, N−1/N−2 retention, signed manifests, runbook and status page | Pull the artifact, revert the manifest, publish rollback instructions, postmortem |
| RISK-034 | A flagship exceeds 18 EM; a gate slips; parity engineers are pulled | Three flagships maximum, explicit gates and cut order, no new systems after the gate; the 2.0 track is staffed separately (OPEN-019) | Ship Workshop + Hearth, move Delve to `1.1`; re-baseline publicly |
| RISK-035 | A crash report contains paths, chat or identities; telemetry enabled against policy | Opt-in only, redaction fixtures, self-hosted collector, no server minidumps by default | Purge the data, notify affected users, audit retention, publish the incident |
| RISK-036 | `wgpu`/`winit` appear in the server tree; platform types leak upward | Dependency guard in CI on all targets and features; trait boundary; review checklist | Revert the offending change, add a regression test, refactor before the next milestone |
| RISK-037 | One fuzz seed duplicates an item; a public server reports a dupe | One host transaction, conservation assertion, 100k-sequence fuzz, audit trail | Roll back the affected inventory range from backups, patch, publish the policy and compensation |
| RISK-038 | A tunnelling or clipping report reproduces; a vehicle "feels wrong" in recorded sessions | Sub-step cap, continuous projectile DDA, conformance maps, penetration fuzz, recorded feel tests | Block the release, fix the sweep and add the fixture; document a temporary server workaround |
| RISK-039 | Players ignore the fingerprint prompt; a shared ticket is reused after a server key change | Pin in the ticket and UI, explicit re-pin flow, ACME documented, short-lived key rotation | Publish a key-change advisory, re-issue tickets, offer ACME guidance, add an in-client warning |
| RISK-040 | Key custody incident, HSM failure, or a signing step that cannot be reproduced | HSM custody, dual control, rotation list, re-sign procedure, rehearsed offline backup | Revoke and rotate, re-sign the previous release, publish an advisory, keep the old key for verification |

## 5. Top-10 watchlist

Reviewed every week, in this order. A row leaves the watchlist only by being mitigated or retired,
not by being forgotten.

| # | Risk | Exposure | Current signal | Threshold to escalate | Next decision |
| ---: | --- | ---: | --- | --- | --- |
| 1 | RISK-013 burnout / bus factor | 20 | Solo, full-time, public cadence | Two missed weeks or a devlog gap > 3 weeks | Recruit a co-maintainer; cut scope to `0.1` plus modding |
| 2 | RISK-011 parity scope | 20 | Tier-1 checklist and Tier-2 coverage report being built | Tier-1 in-scope pass < 100% or behavioural coverage < 85% at MS-14; no funded 2.0 plan at MS-16 | Ship Tier-1 Early Access with a published gap; fund or descope the 2.0 track |
| 3 | RISK-001 renderer schedule | 16 | MS-04 at 120 FPS prototype | Slip > 3 weeks; p95 > 8 ms after two passes | Apply the cut order; the 200–240 dev-week track already carries the overrun |
| 4 | RISK-003 netcode feel | 16 | Simulator and prediction design | > 1 correction per 10 s sprinting | Cut fast features, cap supported RTT |
| 5 | RISK-009 mod adoption | 16 | SDK and template planned | < 20 external packs after `0.4` | Commission seed mods; simplify the API |
| 6 | RISK-012 art/audio | 16 | Placeholder pack first | < 60% textures at MS-12 | Ship CC0 subset; delay art to `1.1` |
| 7 | RISK-005 determinism drift | 15 | Harness at MS-02 | Any cross-platform hash mismatch | Fixed-point for the divergent subsystem |
| 8 | RISK-018 funding | 15 | Grants to be applied for | Runway < 6 months | Pause contractors; part-time mode; earlier release |
| 9 | RISK-002 120/144 FPS gate | 12 | Budgets and bench box planned | Two red nightlies | Re-baseline distance with data |
| 10 | RISK-006 save corruption | 12 | Format not yet frozen | Any quarantine event in soak | Halt, repair tool, honest postmortem |

## 6. Schedule exposure

Expected delay is approximated as `P(occur) × Δ`, with `P = L / 5`. This is an order-of-magnitude
estimate to compare risks, not a budget line. The point is not the exact total; it is that the
technical and scope risks alone exceed the roadmap's 20% coordination allowance.

| Risk | P | Δ mid | Expected dev-weeks |
| --- | ---: | ---: | ---: |
| RISK-011 parity scope (Tier 1 at 1.0 + funded 2.0) | 1.00 | 20 | 20.0 |
| RISK-001 renderer | 0.80 | 10 | 8.0 |
| RISK-005 determinism | 0.60 | 10 | 6.0 |
| RISK-012 art/audio | 0.80 | 8 | 6.4 |
| RISK-003 netcode | 0.80 | 5.5 | 4.4 |
| RISK-002 perf gate | 0.60 | 6 | 3.6 |
| RISK-010 server scale | 0.60 | 4.5 | 2.7 |
| **Technical, scope and content total** | | | **≈ 51.1** |

That is roughly 21–26% of the **200–240 dev-week** 1.0 track (it was ~30% of the old
159-week figure) and a smaller share of the combined ~700–1,040 two-track total. The
roadmap's 20% figure is coordination overhead, not contingency, so **the cut lines in
[the MVP definition](02-mvp.md), the tier gates and the two-track split are the actual
contingency**. RISK-004 is excluded from the total because DEC-048 settles the cluster
default; its remaining implementation risk is carried inside MS-04. The people risks
(013, 017, 018) are excluded because they do not delay the project so much as stop it;
they are tracked as viability risks, not schedule risks.

## 7. Review cadence and escalation

| When | What | Output |
| --- | --- | --- |
| Weekly, 15 min | Top-10 watchlist: signal, threshold, next decision | Updated statuses; new triggers |
| Monthly, 45 min | Full register: re-score L and I, close retired risks, add new ones | Register diff in a PR; roadmap delta if the total moved |
| Per milestone gate | Every risk owned by the exiting milestone is reviewed with evidence | Sign-off or explicit carry-forward |
| Post-incident, within 7 days | The occurred risk becomes a blameless postmortem with actions | New or amended risk if the cause remains |

**Escalation rules.**

1. A trigger firing sets status `occurred`, names an incident lead and produces a recovery plan
   within 48 hours.
2. Exposure of 16 or more is reviewed by the lead within 24 hours; exposure of 20 or more stops
   feature work until the roadmap is re-baselined or the risk is mitigated.
3. Any security, legal or privacy risk is escalated within 24 hours regardless of score, and is
   never discussed in a public issue before the advisory is ready.
4. A new risk enters the register within five working days of being identified, with an owner and
   an initial score; an unowned risk is not allowed to sit in a chat thread.
5. Re-scoring a risk by more than one band in L or I is a roadmap-relevant event and is recorded in
   the monthly delta.

## 8. Mapping from chapter-local risk IDs

Local IDs are provisional. They exist so a chapter can discuss its own risks without waiting for a
register update; this table is the join.

| Source | Local ID | Canonical | Note |
| --- | --- | --- | --- |
| [spec/01](../spec/01-technical-architecture.md) | RISK-101 | RISK-002 | Device-thread handoff latency shows up as frame time |
| [spec/01](../spec/01-technical-architecture.md) | RISK-102 | RISK-036 | Dependency rule erosion |
| [spec/01](../spec/01-technical-architecture.md) | RISK-103 | RISK-024 | Loopback slower than a direct call |
| [spec/01](../spec/01-technical-architecture.md) | RISK-104 | RISK-002 | Snapshot copying grows with entity count |
| [spec/01](../spec/01-technical-architecture.md) | RISK-105 | RISK-035 | Crash reports leak paths or identities |
| [spec/01](../spec/01-technical-architecture.md) | RISK-106 | RISK-008 | Queues and mods exhaust server memory |
| [spec/02](../spec/02-engine-core.md) | RISK-111 | RISK-005 | Scheduler rules bypassed, determinism erodes |
| [spec/02](../spec/02-engine-core.md) | RISK-112 | RISK-002 | Long job slices defeat priorities |
| [spec/02](../spec/02-engine-core.md) | RISK-113 | RISK-002 | Archetype churn returns with entity states |
| [spec/02](../spec/02-engine-core.md) | RISK-114 | RISK-020 | Hot reload applies a partial document |
| [spec/02](../spec/02-engine-core.md) | RISK-115 | RISK-036 | `winit` types leak upward |
| [spec/02](../spec/02-engine-core.md) | RISK-116 | RISK-035 | Crash reports grow or leak host data |
| [spec/03](../spec/03-rendering.md) | RISK-201 | RISK-007 | Metal lagged-count clamp mispredicts |
| [spec/03](../spec/03-rendering.md) | RISK-202 | RISK-001 | Arena fragments and defrag hitches |
| [spec/03](../spec/03-rendering.md) | RISK-203 | RISK-004 | Argument capacity versus cluster default |
| [spec/03](../spec/03-rendering.md) | RISK-204 | RISK-007 | wgpu Metal emulation changes with a driver |
| [spec/03](../spec/03-rendering.md) | RISK-205 | RISK-022 | Golden images turn flaky and block PRs |
| [spec/04](../spec/04-shaders-and-visuals.md) | RISK-301 | RISK-002 | Shadow and shading cost exceed budget |
| [spec/04](../spec/04-shaders-and-visuals.md) | RISK-302 | RISK-026 | Shader hooks become a world-query API |
| [spec/04](../spec/04-shaders-and-visuals.md) | RISK-303 | RISK-001 | Pipeline permutation explosion |
| [spec/04](../spec/04-shaders-and-visuals.md) | RISK-304 | RISK-009 | TAA blurs the pixel-art look |
| [spec/04](../spec/04-shaders-and-visuals.md) | RISK-305 | RISK-002 | Water and OIT spike on integrated GPUs |
| [spec/08](../spec/08-gameplay-survival.md) | RISK-041 | RISK-011 | Combat numbers differ from parity |
| [spec/08](../spec/08-gameplay-survival.md) | RISK-042 | RISK-005 | Tick-rate pacing distorts durations |
| [spec/08](../spec/08-gameplay-survival.md) | RISK-043 | RISK-037 | Death/portal transfer loses or duplicates items |
| [spec/08](../spec/08-gameplay-survival.md) | RISK-044 | RISK-026 | Portal linking is exploited |
| [spec/08](../spec/08-gameplay-survival.md) | RISK-045 | RISK-020 | Balance profiles become incompatible |
| [spec/11](../spec/11-inventory-crafting-items.md) | RISK-046 | RISK-037 | Transaction edge permits duplication |
| [spec/11](../spec/11-inventory-crafting-items.md) | RISK-047 | RISK-002 | Recipe matching degrades with modded counts |
| [spec/11](../spec/11-inventory-crafting-items.md) | RISK-048 | RISK-020 | Component schema sprawls across packs |
| [spec/11](../spec/11-inventory-crafting-items.md) | RISK-049 | RISK-002 | Hopper and item load at scale |
| [spec/11](../spec/11-inventory-crafting-items.md) | RISK-050 | RISK-003 | Predicted inventory ghosts on a bad link |
| [spec/09](../spec/09-entities-ai.md) | RISK-051 | RISK-002 | AI cost scales with mob count |
| [spec/09](../spec/09-entities-ai.md) | RISK-052 | RISK-005 | Path results arrive out of order |
| [spec/09](../spec/09-entities-ai.md) | RISK-053 | RISK-006 | UUID/NetId confusion leaks session ids into saves |
| [spec/09](../spec/09-entities-ai.md) | RISK-054 | RISK-020 | Spawn rules become engine constants |
| [spec/09](../spec/09-entities-ai.md) | RISK-055 | RISK-037 | Villager restock rerolls exploited |
| [spec/09](../spec/09-entities-ai.md) | RISK-056 | RISK-020 | Boss scripting becomes a private path |
| [spec/09](../spec/09-entities-ai.md) | RISK-057 | RISK-006 | Entity data growth makes saves unbounded |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-058 | RISK-002 | Physics cost grows with entities and projectiles |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-059 | RISK-002 | Pathological mod shapes: collision cost, visual mismatch |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-060 | RISK-038 | Tunnelling under lag or high speed |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-061 | RISK-002 | Fluid simulation spikes or duplicates |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-062 | RISK-026 | Entity pushing used to grief or clip |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-063 | RISK-038 | Vehicle physics feels wrong or derails |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-064 | RISK-005 | Determinism drift from math or order |
| [spec/10](../spec/10-physics-and-collision.md) | RISK-065 | RISK-037 | Piston/entity transaction duplication |
| [spec/14](../spec/14-audio.md) | RISK-141 | RISK-002 | Mixer work migrates onto the main thread |
| [spec/14](../spec/14-audio.md) | RISK-142 | RISK-012 | Unnormalised clips make the limiter pump |
| [spec/14](../spec/14-audio.md) | RISK-143 | RISK-034 | Voice chat scope grows into 1.0 |
| [spec/14](../spec/14-audio.md) | RISK-144 | RISK-007 | Platform audio backends behave differently under device churn |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-251 | RISK-010 | Entity bandwidth grows in crowded fights |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-252 | RISK-010 | 512-block join breaks slow links |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-253 | RISK-039 | Self-signed default trains warning click-through |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-254 | RISK-003 | Prediction drifts from the host |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-255 | RISK-020 | Protocol churn breaks third-party tools |
| [spec/18](../spec/18-networking-multiplayer.md) | RISK-256 | RISK-010 | QUIC cost at 200+ peers exceeds the model |
| [spec/21](../spec/21-tooling-and-pipelines.md) | RISK-401 | RISK-025 | Pipeline drifts from runtime interpretation |
| [spec/21](../spec/21-tooling-and-pipelines.md) | RISK-402 | RISK-022 | Build cache makes wrong reuse the normal case |
| [spec/21](../spec/21-tooling-and-pipelines.md) | RISK-403 | RISK-034 | Tooling grows into a second product |
| [spec/21](../spec/21-tooling-and-pipelines.md) | RISK-404 | RISK-020 | Validator accepts what the host rejects |
| [spec/21](../spec/21-tooling-and-pipelines.md) | RISK-405 | RISK-036 | Debug tools leak into simulation and break replays |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-601 | RISK-040 | Signing key compromise or loss |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-602 | RISK-033 | Updater bricks installs on one OS |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-603 | RISK-028 | Store delisting removes distribution |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-604 | RISK-007 | Linux library drift breaks the binary |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-605 | RISK-035 | Crash uploads leak personal data |
| [spec/22](../spec/22-platforms-and-distribution.md) | RISK-606 | RISK-002 | Hardware auto-detect picks an unplayable preset |
| [spec/23](../spec/23-accessibility-localization.md) | RISK-231 | RISK-023 | Localization deferred, content hard-codes text |
| [spec/23](../spec/23-accessibility-localization.md) | RISK-232 | RISK-023 | Accessibility retrofitted after the UI exists |
| [spec/23](../spec/23-accessibility-localization.md) | RISK-233 | RISK-030 | Font licensing or size blocks a locale at release |
| [spec/23](../spec/23-accessibility-localization.md) | RISK-234 | RISK-023 | Translation workflow stalls; assists mistaken for cheats |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-101 | RISK-034 | Flagship scope explosion |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-102 | RISK-009 | Incumbent gravity |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-103 | RISK-020 | First-party maintenance across API changes |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-104 | RISK-034 | Balance economies interact |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-105 | RISK-020 | Platform capture by one mod |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-106 | RISK-019 | Voice and UGC invite abuse |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-107 | RISK-005 | Moving grids and networks are float-heavy |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-108 | RISK-015 | Names and art drift toward the incumbent |
| [research/10](../research/10-custom-content-and-differentiation.md) | RISK-109 | RISK-011 | Flagships pull engineers off parity |

Note the collisions: `RISK-101` means "device-thread handoff" in spec/01 and "scope explosion" in
research/10, and `RISK-201…206` are rendering risks in spec/03 but networking risks in spec/18. The
source column disambiguates; canonical IDs never collide, and a local block is never reused by a
different chapter.

## 9. Open questions

1. **Risk appetite and the stop rule.** Exposure 20 currently stops feature work. Whether a
   funding shortfall or a legal threat should stop the roadmap outright, or run in parallel with a
   mitigation plan, is a lead decision. Owner: MS-16.
2. **Where the register lives.** This document is canonical; the issue tracker could mirror it for
   visibility. Mirroring risks two sources of truth. Owner: MS-00.
3. **Funding model.** Grants and sponsorships are assumed; a services or support business, and
   whether either forks the product, is undecided and changes RISK-018. Owner: Lead.
4. **Bus-factor plan.** Co-maintainer recruitment, shared release keys and documented access are
   mitigation, but the trigger for hiring or revenue-sharing is not set. Owner: Lead with MS-16.
5. **Trademark clearance budget and timing.** A clearance search and any office actions cost money
   and calendar time that the plan does not carry; DEC-037 makes it a launch gate. Owner: MS-16.
6. **Legal entity and insurance.** Who signs releases, holds the mark and carries liability before
   1.0 is unstated and bears on RISK-014/015/035. Owner: MS-16.
7. **Register item needed:** the risk review cadence and escalation thresholds become a normative
   process, so a missed monthly review is visible rather than quiet. Owner: MS-00.
8. **Register item needed:** an incident-response severity scale shared by security, save
   corruption and bad updates, referenced by both this register and release operations. Owner:
   MS-16.
9. **2.0 funding (OPEN-019, owner: lead).** RISK-011 is now partly a funding risk: the
   ~500–800 dev-week Tier-2 track needs 6–10 funded people for 2–3 years. Decide the
   funding path before MS-16, or descope the 2.0 claim explicitly.
10. **Modern Delta (OPEN-020, owner: MS-17).** The pinned 1.21.11 baseline is a 1.0
    decision; re-baselining to a newer release before the 2.0 claim changes RISK-011's
    trigger and delta size.

## 10. References

Decisions: [DEC-009](../decisions.md), [DEC-022](../decisions.md), [DEC-034](../decisions.md),
[DEC-035](../decisions.md), [DEC-036](../decisions.md), [DEC-037](../decisions.md),
[DEC-048](../decisions.md), [DEC-051](../decisions.md), [DEC-052](../decisions.md).

Planning: [roadmap](00-roadmap.md) · [milestones](01-milestones.md) · [MVP](02-mvp.md) ·
[work breakdown](03-work-breakdown.md) · [team and process](04-team-and-process.md) ·
[release and operations](06-release-and-operations.md) ·
[review findings](07-review-findings.md).

Chapters with local risk tables: [01](../spec/01-technical-architecture.md),
[02](../spec/02-engine-core.md), [03](../spec/03-rendering.md), [04](../spec/04-shaders-and-visuals.md),
[08](../spec/08-gameplay-survival.md), [11](../spec/11-inventory-crafting-items.md).

Research: [Weft teardown](../research/01-weft-teardown.md) (schedule and maintenance evidence) ·
[networking](../research/04-networking-sota.md) (scale and feel) ·
[Minecraft feature inventory](../research/05-minecraft-feature-inventory.md) (content velocity) ·
[legal and licensing](../research/07-legal-licensing-and-ip.md) (provenance and trademark) ·
[persistence and packaging](../research/09-persistence-assets-packaging.md) (distribution and
update risk) · [custom content](../research/10-custom-content-and-differentiation.md)
(flagship scope and incumbent gravity).

External:

- Project Management Institute, *Practice Standard for Project Risk Management* (2009) — the
  likelihood × impact convention and trigger vocabulary used here, adapted with our own scales.
- NIST, *SP 800-30 Rev. 1, Guide for Conducting Risk Assessments* (2012) — likelihood/impact
  framing for the security risks.
- GitHub, *Security Advisories* — coordinated disclosure mechanics used in RISK-008/015/035:
  <https://docs.github.com/en/code-security/security-advisories>

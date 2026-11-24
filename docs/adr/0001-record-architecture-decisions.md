# ADR-0001 — Record architecture decisions

> Status: accepted · Date: 2026-11-24 · Decision: DEC-010

## Context

OpenBlock is a multi-year project with a small core team, community
contributors, and heavy AI-assisted research and drafting. Architectural
choices will outlive the people who made them, and the reasoning behind them is
the most expensive thing to reconstruct later. Rejected alternatives are
especially valuable: without a record, they get relitigated every few months.

## Decision

We keep two complementary artefacts:

1. **[The decision register](../decisions.md)** — the current, binding state of
   every decision (`DEC-xxx`), with status, reversal cost, owner milestone, a
   short rationale and links. It is append-only: decisions are superseded, never
   deleted.
2. **Architecture Decision Records (ADRs)** — one numbered file per significant
   decision, capturing context, alternatives, consequences and the story around
   the decision.

Rules:

- A change to a normative specification chapter cites an existing `DEC-*` or
  adds one in the same pull request.
- Superseding a decision requires a new `DEC-*` that says
  "supersedes DEC-xxx" and an ADR that explains the new evidence.
- ADRs are immutable once accepted except for status changes
  (`accepted → superseded`), typo fixes and added links.
- Small, local decisions do not need an ADR; they need a code comment or a
  chapter section. The bar is "would a new contributor be surprised years from
  now?".

## Consequences

- Slightly more writing per significant decision, in exchange for much less
  re-litigation, better onboarding, and a defensible answer to "why is it like
  this?".
- The register can drift from reality if PRs change behaviour without updating
  it; review checklists and the docs CI jobs mitigate this.
- The number of ADRs must stay manageable — we prefer the register for most
  decisions and reserve ADRs for the ones with real alternatives and long
  consequences.

## Alternatives considered

- **Comments and commit messages only** — cheap, but unsearchable and invisible
  to new contributors.
- **A wiki** — drifts, no review, no version history tied to code.
- **Only a register, no ADRs** — loses the context and alternatives that make a
  decision understandable years later.

## References

- [Decision register](../decisions.md)
- [Spec conventions §9](../spec/00-conventions.md#9-ownership-and-change-control)
- Michael Nygard, *Documenting Architecture Decisions* (2011)

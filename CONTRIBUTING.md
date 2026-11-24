# Contributing to OpenBlock

Thanks for wanting to help build an open, fast, deeply moddable block game.
OpenBlock is early: the specification is being written before the engine. That
makes **design contributions as valuable as code** right now.

## Read this first

1. [Vision](docs/vision.md) — what we are and are not building.
2. [Spec conventions](docs/spec/00-conventions.md) — terminology and how to
   write a spec chapter.
3. [Milestones](docs/plan/01-milestones.md) — what is being built next.
4. [Decision register](docs/decisions.md) — the decisions you must not
   contradict without opening a DEC.

## Legal and provenance

- By contributing you agree your work is licensed under **MIT OR Apache-2.0**
  (see [ADR-0008](docs/adr/0008-license-and-provenance.md)).
- **Do not copy code from Minecraft, Weft, Luanti, or any other project.**
  Reading code to understand a technique is fine; reproducing it is not.
  Describe the idea in your own words in the PR, then write your own
  implementation.
- Do not commit Minecraft assets, names, sounds, or textures. Every asset in
  the repository must have a recorded license and source.
- AI-assisted contributions are welcome under the same rules: you are
  responsible for the provenance of what you submit.

## Development setup

```sh
# Toolchain is pinned by rust-toolchain.toml (stable, 2024 edition).
rustup show
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo doc --workspace --no-deps
```

Optional tooling used by CI:

```sh
cargo install cargo-deny --locked
cargo deny check
npx markdownlint-cli2 "**/*.md"
```

## What to work on

- Issues labelled `good first issue` and `spec: review` are the best entry
  points.
- A design PR should add or modify a document in `docs/` and follow the chapter
  shape in [Spec conventions](docs/spec/00-conventions.md#7-document-conventions).
- A code PR should cite the `REQ-*` IDs it satisfies and include or update the
  tests that verify them.

## Pull request checklist

- [ ] The change is scoped to one milestone or one specification area.
- [ ] Normative documents changed? A `DEC-*` is added or cited.
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test` pass.
- [ ] New links resolve; markdown lint passes.
- [ ] No copied code, no Minecraft assets, provenance recorded for assets.
- [ ] Performance-sensitive paths include a benchmark or a budget note.

## Commit messages

Conventional Commits, scoped by area:

```
feat(render): add section mesh arena with defragmentation
fix(net): correct interest-set shrink on dimension change
docs(spec): define redstone update ordering
chore(ci): cache target dir between jobs
```

## Code of conduct

Participation is covered by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Security

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

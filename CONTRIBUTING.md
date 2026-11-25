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
  (see [ADR-0008](docs/adr/0008-license-and-provenance.md)). Every commit must
  carry a `Signed-off-by` trailer certifying the [Developer Certificate of
  Origin 1.1](DCO); use `git commit -s`. There is no CLA.
- **Do not copy code from Minecraft, Weft, Luanti, or any other project.**
  Reading code to understand a technique is fine; reproducing it is not.
  Describe the idea in your own words in the PR, then write your own
  implementation.
- **Clean-room naming (DEC-053).** Shipped base-pack identifiers use original
  names: `openblock:underworld` for the nether-like dimension,
  `openblock:terminus` for the end-like dimension, and original mob names (for
  example the Blaster). Minecraft-specific proper nouns may appear only in
  cited research prose; they must not appear in content identifiers, item or
  mob names, save fields, protocol strings, or base-pack data.
- Do not commit Minecraft assets, sounds, textures, or names. Every asset in
  the repository must have a recorded license and source.
- AI-assisted contributions are welcome under the same rules: you are
  responsible for the provenance of what you submit.

## Development setup

OpenBlock is Fedora-first ([DEC-054](docs/decisions.md)). On Fedora 44/45 the
build dependencies are:

```sh
sudo dnf install gcc gcc-c++ pkgconf-pkg-config git \
  wayland-devel libxkbcommon-devel vulkan-loader-devel fontconfig-devel \
  freetype-devel systemd-devel
```

Then install the pinned toolchain and build:

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
- [ ] Every commit carries a DCO `Signed-off-by` trailer (`git commit -s`).
- [ ] No copied code, no Minecraft assets or proper nouns, provenance recorded.
- [ ] Performance-sensitive paths include a benchmark or a budget note.

## Commit messages

Conventional Commits, scoped by area:

```
feat(render): add section mesh arena with defragmentation
fix(net): correct interest-set shrink on dimension change
docs(spec): define redstone update ordering
chore(ci): cache target dir between jobs
```

## Commit sign-off (DCO)

Every commit in a pull request must end with a `Signed-off-by: Name <email>`
trailer. The trailer is your attestation of the [Developer Certificate of
Origin 1.1](DCO), whose canonical text is at the repository root. Use your real
name and an email you can receive at.

```sh
git commit -s -m "feat(render): add section mesh arena"
git commit --amend -s              # sign the current commit
git rebase --signoff origin/main   # sign commits already on your branch
```

Keep the sign-off on every commit, including fixup commits: the DCO requirement
covers the whole commit range of the pull request, not just the merge commit. A
missing trailer is a blocking review defect.

## Continuous integration

CI runs formatting, clippy, tests, docs, the architecture guard
(`.github/scripts/check-arch.sh`), `cargo deny`, and markdown/link checks. The
DCO requirement covers the whole commit range of the pull request: **every
commit must carry a `Signed-off-by` trailer**; a PR with an unsigned commit is
not merged until the history is amended. Only a human contributor can sign off;
an AI agent cannot sign on your behalf.

## Code of conduct

Participation is covered by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Security

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

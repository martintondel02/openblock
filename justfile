# OpenBlock workspace tasks. Requires just(1); everything also works via cargo.
set shell := ["bash", "-c"]

default:
    @just --list

check:
    cargo check --workspace --all-targets

test:
    cargo test --workspace --all-targets

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

deny:
    cargo deny check

md-lint:
    npx markdownlint-cli2 "**/*.md"

# Everything CI runs on a single machine.
ci: fmt-check lint test doc

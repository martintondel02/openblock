# syntax=docker/dockerfile:1
#
# Multi-stage image for the OpenBlock dedicated server.
#
# The server is headless: ob_server does not depend on wgpu, winit, X11,
# Wayland, Vulkan, PulseAudio or ALSA. The runtime image therefore contains only
# musl libc, the server binary and a tiny TCP healthcheck. The client is a
# separate Flatpak/RPM artifact and is NOT built here.
#
# Build from the repository root (the build context must include Cargo.toml,
# Cargo.lock and crates/):
#   docker build -f packaging/container/Dockerfile -t openblock-server:local .
#
# The Rust builder uses Alpine/musl, so the resulting binary links against musl
# only (no glibc compatibility package is needed in the runtime).

ARG RUST_VERSION=1.100
ARG ALPINE_VERSION=3.22

FROM rust:${RUST_VERSION}-alpine AS builder

# The server dependencies are pure Rust, but keep a C toolchain available for
# any transitive build script. No Vulkan/audio/X11 development packages are
# installed, which also proves the server does not need them at build time.
RUN apk add --no-cache build-base

WORKDIR /build

# Copy only what the workspace build needs. Do not copy rust-toolchain.toml:
# the builder image already pins the Rust version through RUST_VERSION.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask

# Build only the server binary. There are no Cargo features to disable:
# client/GPU/audio crates are not in ob_server's dependency graph.
RUN cargo build --release --locked -p ob_server --bin openblock-server

FROM alpine:${ALPINE_VERSION} AS runtime

# netcat-openbsd provides the `nc -z` health probe. It is the only runtime
# package added and has no GPU/audio/display dependency.
RUN apk add --no-cache netcat-openbsd \
    && addgroup -S -g 10001 openblock \
    && adduser -S -D -H -u 10001 -G openblock -s /sbin/nologin openblock \
    && mkdir -p /data \
    && chown openblock:openblock /data

COPY --from=builder /build/target/release/openblock-server /usr/local/bin/openblock-server

# Non-root, no shell login, single writable volume. `read_only: true` and
# `cap_drop: [ALL]` are set by the compose example (and should be set by any
# other orchestrator).
# SIGTERM (and SIGINT) trigger the graceful save path.
STOPSIGNAL SIGTERM

USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
EXPOSE 25565/tcp

ENV OPENBLOCK_LOG=info

# TCP liveness only: the interim server has no HTTP health endpoint. This proves
# the listener is accepting connections, not that a world finished loading;
# the server logs "starting TCP server on ..." when it is ready for clients.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD nc -z -w 2 127.0.0.1 25565 || exit 1

# Explicit flags keep the image useful with no config file. Mount a config at
# /data/server.toml and append `--config /data/server.toml` to override the
# defaults; the server's CLI flags take precedence over the file.
ENTRYPOINT ["openblock-server", "--data-dir", "/data", "--listen", "0.0.0.0:25565"]

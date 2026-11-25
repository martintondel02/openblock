#!/usr/bin/env bash
# CI guard: the dedicated server must not depend on GPU, window or audio code.
#
# Checks:
#   1. `cargo tree -p ob_server` contains no wgpu/winit/X11/Wayland/Vulkan/audio
#      package (including ob_render/ob_audio/ob_client and their transitive
#      crates).
#   2. If a built server binary is available, `ldd` reports no Vulkan/X11/
#      Wayland/GL/PulseAudio/ALSA shared libraries.
#
# The Fedora RPM still builds client and server from one workspace, so the
# client's dependencies remain in Cargo.lock; only the server's graph is
# checked here.
#
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$root"

cargo_bin=${CARGO:-cargo}
if ! command -v "$cargo_bin" >/dev/null 2>&1; then
    printf 'check-server-deps: error: cargo not found (set CARGO)\n' >&2
    exit 2
fi

if ! tree=$("$cargo_bin" tree -p ob_server --edges normal --prefix none 2>&1); then
    printf 'check-server-deps: error: cargo tree failed:\n%s\n' "$tree" >&2
    exit 2
fi

# Exact package names only: a substring match would false-positive on names
# such as `libc`, `rustc-hash` or `calloop`.
forbidden='wgpu|wgpu-core|wgpu-hal|wgpu-types|winit|naga|gpu-alloc|gpu-descriptor|ash|vulkano|ob_render|ob_audio|ob_client|cpal|alsa|alsa-sys|libpulse|libpulse-sys|pulse|wayland-client|wayland-server|wayland-sys|wayland-backend|wayland-protocols|smithay-client-toolkit|calloop|x11|x11-dl|x11rb|xkbcommon|libxkbcommon|raw-window-handle|glutin|winit'

bad=$(printf '%s\n' "$tree" | awk '{print $1}' | sort -u | grep -Ex -- "$forbidden" || true)
if [[ -n $bad ]]; then
    printf 'check-server-deps: FAIL: forbidden crates in the ob_server graph:\n' >&2
    printf '  %s\n' $bad >&2
    printf '\nFull graph:\n%s\n' "$tree" >&2
    exit 1
fi

package_count=$(printf '%s\n' "$tree" | awk 'NF >= 2 {print $1}' | sort -u | wc -l)
printf 'check-server-deps: cargo graph clean (%s packages; no GPU/window/audio crates)\n' "$package_count"

# Optional linkage check. CI may not have built the binary; skip cleanly.
bin=${OPENBLOCK_SERVER_BIN:-}
if [[ -z $bin ]]; then
    for candidate in \
        "${CARGO_TARGET_DIR:-}/release/openblock-server" \
        "$root/target/release/openblock-server" \
        "$root/target-1.0-ops/release/openblock-server"; do
        [[ -n $candidate && -x $candidate ]] && bin=$candidate && break
    done
fi

if [[ -n $bin && -x $bin ]]; then
    if ! command -v ldd >/dev/null 2>&1; then
        printf 'check-server-deps: ldd unavailable; skipped linkage check\n'
    else
        # ldd exits non-zero for static binaries; that is a pass here.
        deps=$(ldd -- "$bin" 2>&1 || true)
        if grep -q 'not found' <<<"$deps"; then
            printf 'check-server-deps: FAIL: missing shared libraries for %s:\n%s\n' "$bin" "$deps" >&2
            exit 1
        fi
        bad_libs=$(printf '%s\n' "$deps" | grep -Ei 'libvulkan|libX11|libwayland|libxkbcommon|libGL|libEGL|libpulse|libasound' || true)
        if [[ -n $bad_libs ]]; then
            printf 'check-server-deps: FAIL: forbidden shared libraries for %s:\n%s\n' "$bin" "$bad_libs" >&2
            exit 1
        fi
        if [[ $deps == *"not a dynamic executable"* || $deps == *"statically linked"* ]]; then
            printf 'check-server-deps: linkage clean (%s is static)\n' "$bin"
        else
            printf 'check-server-deps: linkage clean (%s has no GPU/audio/display libraries)\n' "$bin"
        fi
    fi
else
    printf 'check-server-deps: no server binary built; skipped linkage check\n'
fi

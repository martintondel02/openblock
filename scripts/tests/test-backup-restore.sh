#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Test server-backup.sh and server-restore.sh against a fresh smoke world.
#
# Usage:
#   OPENBLOCK_SERVER=/path/to/openblock-server scripts/tests/test-backup-restore.sh
#
# If OPENBLOCK_SERVER is unset, common cargo target locations are tried. The
# script is non-root and works entirely under a temporary directory.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
backup="$root/scripts/server-backup.sh"
restore="$root/scripts/server-restore.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/openblock-backup-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

pass() {
    printf 'ok: %s\n' "$*"
}

server=${OPENBLOCK_SERVER:-}
if [[ -z $server ]]; then
    for candidate in \
        "${CARGO_TARGET_DIR:-}/release/openblock-server" \
        "$root/target/release/openblock-server" \
        "$root/target-1.0-ops/release/openblock-server" \
        "/home/martintondel/Projects/openblock/target-1.0-ops/release/openblock-server"; do
        [[ -n $candidate && -x $candidate ]] && server=$candidate && break
    done
fi
if [[ -z $server || ! -x $server ]]; then
    printf 'SKIP: openblock-server binary not found; set OPENBLOCK_SERVER\n'
    exit 0
fi

printf 'using server: %s\n' "$server"

# 1. Generate a real world with deterministic data.
"$server" --smoke --data-dir "$work/data" --seed 7 >"$work/smoke.log" 2>&1 \
    || { cat "$work/smoke.log" >&2; fail "smoke world generation"; }
world="$work/data/worlds/world"
[[ -f $world/world.obs ]] || fail "smoke did not create world.obs"
pass "smoke world generated"

# 2. Three verified backups with retention 2: newest two survive.
for _ in 1 2 3; do
    "$backup" --world "$world" --dest "$work/backups" --retention 2 --verify --quiet
done
mapfile -t archives < <(find "$work/backups" -maxdepth 1 -type f -name '*.tar.*' -print | sort)
((${#archives[@]} == 2)) || fail "retention kept ${#archives[@]} archives, expected 2"
pass "retention kept 2 archives"
for archive in "${archives[@]}"; do
    [[ $(stat -c '%a' "$archive") == 600 ]] || fail "archive mode is not 600: $archive"
done
pass "archives are mode 600"

newest=${archives[1]}
tar -tf "$newest" > "$work/listing.txt" || fail "cannot list archive"
grep -Fxq 'world/world.obs' "$work/listing.txt" || fail "archive lacks world/world.obs"
pass "archive contains world/world.obs"

# 3. Restore and compare the world payload byte-for-byte.
"$restore" --archive "$newest" --dest "$work/restore/world" --yes >"$work/restore.log" 2>&1 \
    || { cat "$work/restore.log" >&2; fail "restore"; }
[[ $(head -c 4 "$work/restore/world/world.obs") == OBSV ]] || fail "restored magic is not OBSV"
pass "restore writes OBSV world.obs"
mkdir -p "$work/compare"
tar -xf "$newest" -C "$work/compare" world/world.obs
cmp "$work/compare/world/world.obs" "$work/restore/world/world.obs" \
    || fail "restored world.obs differs from archive"
pass "restored world.obs matches archive"

# 4. Existing destination must be protected without --force.
if "$restore" --archive "$newest" --dest "$work/restore/world" --yes >"$work/noforce.log" 2>&1; then
    fail "restore unexpectedly replaced an existing world without --force"
fi
grep -q 'destination exists' "$work/noforce.log" || fail "no-force error did not explain the refusal"
[[ -f $work/restore/world/world.obs ]] || fail "no-force restore damaged the existing world"
pass "restore refuses to overwrite without --force"

# 5. --force preserves the old world as a timestamped sibling.
"$restore" --archive "$newest" --dest "$work/restore/world" --force --yes >"$work/force.log" 2>&1 \
    || { cat "$work/force.log" >&2; fail "forced restore"; }
compgen -G "$work/restore/world.bak-*" >/dev/null || fail "forced restore did not preserve the old world"
pass "forced restore preserves the previous world"
[[ $(head -c 4 "$work/restore/world/world.obs") == OBSV ]] || fail "forced restore magic is not OBSV"

# 6. Gzip fallback path. Hide zstd from PATH (tar/gzip/coreutils remain).
fakebin="$work/fakebin"
mkdir -p "$fakebin"
for cmd in tar gzip date mktemp dirname basename du cut sort find grep head chmod mv rm mkdir pwd cat stat cmp wc realpath; do
    path=$(command -v "$cmd" 2>/dev/null || true)
    [[ -n $path ]] && ln -s "$path" "$fakebin/$cmd"
done
mkdir -p "$work/gzip-backups"
PATH="$fakebin" /bin/bash "$backup" --world "$world" --dest "$work/gzip-backups" --retention 1 --verify --quiet
gzip_archive=$(find "$work/gzip-backups" -maxdepth 1 -name '*.tar.gz' -print -quit)
[[ -n $gzip_archive ]] || fail "gzip fallback did not create a .tar.gz archive"
pass "gzip fallback creates .tar.gz"
"$restore" --archive "$gzip_archive" --dest "$work/gzip-restore/world" --yes >"$work/gzip-restore.log" 2>&1 \
    || { cat "$work/gzip-restore.log" >&2; fail "gzip restore"; }
[[ $(head -c 4 "$work/gzip-restore/world/world.obs") == OBSV ]] || fail "gzip restore magic is not OBSV"
pass "gzip archive restores"

printf '\nPASS: backup/restore tests\n'

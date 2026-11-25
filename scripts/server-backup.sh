#!/usr/bin/env bash
# server-backup.sh — consistent, verified snapshot of one OpenBlock world.
#
# The archive contains the whole world directory (world.obs plus player,
# inventory, entity and network-player sidecars) with the world directory as
# its top-level entry, so it can be restored directly with server-restore.sh.
#
# Stop the server first for a fully consistent snapshot. The host saves one
# file at a time with an atomic rename, but the sidecars are not a single
# transaction; a live copy can pair a new world.obs with an older inventory.
#
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail
umask 077

usage() {
    cat <<'EOF'
Usage: server-backup.sh [options]

Options:
  --world DIR       World directory containing world.obs
                    (default: /var/lib/openblock/worlds/world)
  --dest DIR        Directory for archives (default: /var/backups/openblock)
  --retention N     Keep the N newest archives for this world (default: 7)
  --verify          Extract world.obs from the finished archive and check OBSV
  --quiet           Print only errors
  -h, --help         Show this help

Archive names are openblock-<world>-<UTC timestamp>.tar.zst (or .tar.gz when
zstd is unavailable). Archives are written to a temporary file, verified, and
atomically renamed. Only archives matching this world's prefix are pruned.

Examples:
  sudo server-backup.sh --world /var/lib/openblock/worlds/world \
      --dest /var/backups/openblock --retention 7 --verify
  server-backup.sh --world ./data/worlds/demo --dest ./backups --retention 3
EOF
}

die() {
    printf 'server-backup: error: %s\n' "$*" >&2
    exit 1
}

usage_die() {
    printf 'server-backup: error: %s\n' "$*" >&2
    printf 'Try --help for usage.\n' >&2
    exit 2
}

world=/var/lib/openblock/worlds/world
dest=/var/backups/openblock
retention=7
verify=0
quiet=0

while (($#)); do
    case "$1" in
        --world)
            (($# >= 2)) || usage_die "--world requires a value"
            world=$2
            shift 2
            ;;
        --dest)
            (($# >= 2)) || usage_die "--dest requires a value"
            dest=$2
            shift 2
            ;;
        --retention)
            (($# >= 2)) || usage_die "--retention requires a value"
            retention=$2
            shift 2
            ;;
        --verify)
            verify=1
            shift
            ;;
        --quiet)
            quiet=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            usage_die "unknown option '$1'"
            ;;
        *)
            usage_die "unexpected argument '$1'"
            ;;
    esac
done
(($# == 0)) || usage_die "unexpected argument '$1'"

[[ $retention =~ ^[0-9]+$ ]] || usage_die "--retention must be a positive integer"
((retention >= 1)) || usage_die "--retention must be at least 1"

[[ -d $world ]] || die "world directory not found: $world"
[[ -f $world/world.obs ]] || die "not an OpenBlock world (missing world.obs): $world"

# Canonicalise paths after the existence checks so relative invocations work
# and archive entries have a stable top-level name.
world=$(cd -- "$world" && pwd -P)
world_name=$(basename -- "$world")
[[ $world_name != *$'\n'* ]] || die "world directory name must not contain a newline"
# The tar member keeps the real directory name (spaces are fine because tar and
# the restore script quote it). The filename/retention prefix is sanitised so
# globs and shell-sensitive characters never appear in archive names.
archive_name=${world_name//[^A-Za-z0-9._-]/_}
[[ -n $archive_name ]] || archive_name=world

mkdir -p -- "$dest" || die "cannot create backup directory: $dest"
dest=$(cd -- "$dest" && pwd -P)

if command -v zstd >/dev/null 2>&1 && tar --help 2>/dev/null > "$tmp/help.txt" && grep -q -- '--zstd' "$tmp/help.txt"; then
    ext=tar.zst
    compression=zstd
    list_cmd=(tar --zstd -tf)
    extract_cmd=(tar --zstd -xf)
    create_cmd=(tar --zstd -cf)
else
    ext=tar.gz
    compression=gzip
    list_cmd=(tar -tzf)
    extract_cmd=(tar -xzf)
    create_cmd=(tar -czf)
fi

timestamp=$(date -u +%Y%m%dT%H%M%S%NZ)
archive="$dest/openblock-${archive_name}-${timestamp}.${ext}"
suffix=0
while [[ -e $archive ]]; do
    suffix=$((suffix + 1))
    archive="$dest/openblock-${archive_name}-${timestamp}-${suffix}.${ext}"
done

tmp=$(mktemp "${dest}/.openblock-backup.XXXXXX") || die "cannot create a temporary archive in $dest"
verify_dir=
cleanup() {
    [[ -n ${tmp:-} ]] && rm -f -- "$tmp"
    [[ -n ${verify_dir:-} ]] && rm -rf -- "$verify_dir"
    return 0
}
trap cleanup EXIT

log() {
    if ((quiet == 0)); then
        printf '%s\n' "$*"
    fi
    return 0
}

log "backup: $world -> $archive ($compression)"

# Archive the world directory itself, not just its contents, so a restore is a
# single top-level move and does not scatter files into the data root.
"${create_cmd[@]}" "$tmp" -C "$(dirname -- "$world")" -- "$world_name" \
    || die "tar failed; no archive was published"

# Listing check: non-empty, contains this world's world.obs, no absolute or
# parent-relative members. This catches a truncated or unexpectedly rooted tar
# before the atomic rename.
listing=$("${list_cmd[@]}" "$tmp") || die "cannot read the temporary archive"
[[ -n $listing ]] || die "temporary archive is empty"
grep -Fxq -- "${world_name}/world.obs" <<<"$listing" \
    || die "archive does not contain ${world_name}/world.obs"
if grep -Eq -- '(^/|(^|/)\.\.(/|$))' <<<"$listing"; then
    die "archive contains an unsafe absolute or parent-relative path"
fi

if ((verify)); then
    verify_dir=$(mktemp -d "${TMPDIR:-/tmp}/openblock-backup-verify.XXXXXX") \
        || die "cannot create verification directory"
    "${extract_cmd[@]}" "$tmp" -C "$verify_dir" -- "${world_name}/world.obs" \
        || die "cannot extract world.obs for verification"
    magic=$(head -c 4 -- "$verify_dir/${world_name}/world.obs")
    [[ $magic == "OBSV" ]] \
        || die "world.obs magic is '$magic', expected OBSV"
    log "verify: world.obs magic OBSV"
else
    log "listing: ${world_name}/world.obs present"
fi

mv -- "$tmp" "$archive" || die "cannot publish archive: $archive"
chmod 0600 -- "$archive" || die "cannot set archive permissions: $archive"
tmp=

size=$(du -h -- "$archive" | cut -f1)
log "created: $archive ($size)"

# Retention is per world prefix and per compression extension, newest first by
# timestamp. Unrelated files in --dest are never touched.
shopt -s nullglob
archives=("$dest/openblock-${archive_name}-"*.tar.zst "$dest/openblock-${archive_name}-"*.tar.gz)
shopt -u nullglob
mapfile -t sorted < <(printf '%s\n' "${archives[@]}" | sort -r)
pruned=0
kept=${#sorted[@]}
if ((kept > retention)); then
    for old in "${sorted[@]:retention}"; do
        rm -f -- "$old"
        pruned=$((pruned + 1))
        log "pruned: $old"
    done
    kept=$retention
fi
log "retention: keeping $kept archive(s) (limit $retention), pruned $pruned"

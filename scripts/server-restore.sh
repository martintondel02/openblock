#!/usr/bin/env bash
# server-restore.sh — verify and restore an OpenBlock world archive.
#
# Safety model:
#   * the archive listing is checked for absolute/parent-relative paths;
#   * world.obs magic (OBSV) is checked before anything is replaced;
#   * an existing destination is renamed aside, never deleted, and only with
#     --force;
#   * without --yes the replacement asks for confirmation;
#   * extraction happens in a staging directory next to the destination, then
#     moves into place on the same filesystem.
#
# The destination is the exact world directory, for example
# /var/lib/openblock/worlds/world. The server must be stopped first.
#
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail
umask 027

usage() {
    cat <<'EOF'
Usage: server-restore.sh --archive FILE --dest DIR [options]

Options:
  --archive FILE    Backup created by server-backup.sh (.tar.zst or .tar.gz)
  --dest DIR        Target world directory, e.g. /var/lib/openblock/worlds/world
  --force           If DIR exists, rename it to DIR.bak-<timestamp> first
  --yes             Do not prompt (required with --force in non-interactive use)
  --owner USER:GROUP
                    chown -R the restored directory (usually openblock:openblock)
  -h, --help        Show this help

Examples:
  sudo systemctl stop openblock-server
  sudo scripts/server-restore.sh \
      --archive /var/backups/openblock/openblock-world-20261125T....tar.zst \
      --dest /var/lib/openblock/worlds/world --force --yes \
      --owner openblock:openblock
  sudo systemctl start openblock-server
EOF
}

die() {
    printf 'server-restore: error: %s\n' "$*" >&2
    exit 1
}

usage_die() {
    printf 'server-restore: error: %s\n' "$*" >&2
    printf 'Try --help for usage.\n' >&2
    exit 2
}

archive=
dest=
force=0
assume_yes=0
owner=

while (($#)); do
    case "$1" in
        --archive)
            (($# >= 2)) || usage_die "--archive requires a value"
            archive=$2
            shift 2
            ;;
        --dest)
            (($# >= 2)) || usage_die "--dest requires a value"
            dest=$2
            shift 2
            ;;
        --force)
            force=1
            shift
            ;;
        --yes)
            assume_yes=1
            shift
            ;;
        --owner)
            (($# >= 2)) || usage_die "--owner requires USER:GROUP"
            owner=$2
            shift 2
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

[[ -n $archive ]] || usage_die "--archive is required"
[[ -n $dest ]] || usage_die "--dest is required"
[[ -f $archive ]] || die "archive not found: $archive"
[[ -s $archive ]] || die "archive is empty: $archive"

archive=$(cd -- "$(dirname -- "$archive")" && pwd -P)/$(basename -- "$archive")
dest=$(realpath -m -- "$dest")
[[ $dest != / ]] || die "refusing to restore over /"
[[ $dest != "$(realpath -m -- "$archive")" ]] || die "destination must not be the archive"

parent=$(dirname -- "$dest")
mkdir -p -- "$parent" || die "cannot create destination parent: $parent"

# Refuse unsafe member names before extraction. GNU tar also removes leading
# slashes, but this check makes the intent explicit and fails early.
listing=$(tar -tf "$archive") || die "cannot list archive (is it a supported tar.zst/tar.gz?)"
[[ -n $listing ]] || die "archive listing is empty"
if grep -Eq -- '(^/|(^|/)\.\.(/|$))' <<<"$listing"; then
    die "archive contains an absolute or parent-relative path"
fi

mapfile -t world_members < <(grep -E '(^|/)world\.obs$' <<<"$listing")
if ((${#world_members[@]} == 0)); then
    die "archive does not contain world.obs"
fi
if ((${#world_members[@]} > 1)); then
    die "archive contains more than one world.obs; refusing to guess"
fi
member=${world_members[0]}
case "$member" in
    world.obs) root="" ;;
    */world.obs) root=${member%/world.obs} ;;
    *) die "unsupported world.obs path: $member" ;;
esac

# Show exactly what will happen before touching anything.
archive_size=$(du -h -- "$archive" | cut -f1)
printf 'archive: %s (%s)\n' "$archive" "$archive_size"
printf 'target:  %s\n' "$dest"
if [[ -e $dest ]]; then
    printf 'existing target will be preserved as: %s.bak-<timestamp>\n' "$dest"
fi

if [[ -e $dest ]]; then
    if ((!force)); then
        die "destination exists; stop the server and re-run with --force to preserve it and restore"
    fi
    if ((!assume_yes)); then
        if [[ ! -t 0 ]]; then
            die "--force needs --yes when stdin is not a terminal"
        fi
        read -r -p "Replace '$dest' with '$archive'? [y/N] " reply
        case "$reply" in
            y|Y|yes|YES) ;;
            *) die "cancelled; nothing changed" ;;
        esac
    fi
fi

stage=$(mktemp -d "${parent}/.openblock-restore.XXXXXX") \
    || die "cannot create staging directory in $parent"
cleanup() {
    [[ -n ${stage:-} ]] && rm -rf -- "$stage"
    return 0
}
trap cleanup EXIT

tar -xf "$archive" -C "$stage" --no-same-owner \
    || die "extraction failed; destination was not changed"

if [[ -n $root ]]; then
    src="$stage/$root"
else
    src=$stage
fi
[[ -f $src/world.obs ]] || die "staged restore has no world.obs; destination was not changed"
magic=$(head -c 4 -- "$src/world.obs")
[[ $magic == "OBSV" ]] || die "staged world.obs magic is '$magic', expected OBSV; destination was not changed"

old=
if [[ -e $dest ]]; then
    old="${dest}.bak-$(date -u +%Y%m%dT%H%M%S%NZ)"
    mv -- "$dest" "$old" || die "cannot move the existing world aside: $dest"
    printf 'preserved: %s\n' "$old"
fi

if ! mv -- "$src" "$dest"; then
    printf 'server-restore: error: could not move the restored world into place\n' >&2
    if [[ -n $old ]]; then
        printf 'server-restore: the previous world is still at %s\n' "$old" >&2
    fi
    exit 1
fi
# For a single top-level archive entry src is stage/<world>; remove the now
# empty staging directory. For a root-level archive src was stage itself, whose
# old path no longer exists after the move.
rm -rf -- "$stage" || true
stage=

if [[ -n $owner ]]; then
    chown -R -- "$owner" "$dest" || die "restored, but chown -R $owner $dest failed"
fi

file_count=$(find "$dest" -type f | wc -l)
total_size=$(du -sh -- "$dest" | cut -f1)
printf 'restored: %s (%s files, %s)\n' "$dest" "$file_count" "$total_size"
printf 'world.obs magic: OBSV\n'
if [[ -n $old ]]; then
    printf 'previous world: %s\n' "$old"
fi
cat <<EOF

Next:
  1. If not using --owner: sudo chown -R openblock:openblock '$dest'
  2. sudo systemctl start openblock-server
  3. journalctl -u openblock-server -n 30 --no-pager
  4. Join and confirm the expected spawn/build; keep the previous world and
     archive until then.
EOF

# Back up and restore

The world is a directory, not a database: back up the **whole**
`<data-dir>/worlds/<world>/` directory, including every sidecar. The repository
ships two tested scripts for this:

* [`scripts/server-backup.sh`](../../scripts/server-backup.sh) — timestamped,
  verified, atomic archive with retention;
* [`scripts/server-restore.sh`](../../scripts/server-restore.sh) — safety-checked
  extraction that preserves the existing world.

There is **no built-in backup scheduler** in this build. Schedule the script
with cron or a systemd timer. See [administration](administration.md#scheduling-backups).
The Fedora `openblock-server` RPM installs the same scripts as
`/usr/bin/openblock-server-backup` and `/usr/bin/openblock-server-restore`; the
commands below use the checkout paths, so substitute the packaged names if you
installed the RPM.

## Consistency first

`world.obs` and each sidecar are written with a temporary file plus atomic
rename, so an individual file is either the old or the new complete version. The
files are **not** updated as one transaction, and there is no fsync. A live copy
can therefore pair a new `world.obs` with an older `inventory.txt`,
`network_players.txt` or `items.txt`. It is also possible (though unlikely) to
capture a file mid-write on filesystems without the expected rename semantics.

For a guaranteed-consistent backup:

```sh
sudo systemctl stop openblock-server
sudo scripts/server-backup.sh --world /var/lib/openblock/worlds/world \
  --dest /var/backups/openblock --retention 14 --verify
sudo systemctl start openblock-server
```

If a maintenance window is impossible, use the console `save` command first
(foreground/tmux only) and accept the sidecar risk. The container has the same
constraint: stop the container for a clean archive.

## Create a backup

```sh
scripts/server-backup.sh \
  --world /var/lib/openblock/worlds/world \
  --dest /var/backups/openblock \
  --retention 7 \
  --verify
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--world DIR` | `/var/lib/openblock/worlds/world` | World directory containing `world.obs` |
| `--dest DIR` | `/var/backups/openblock` | Archive destination (created if missing) |
| `--retention N` | `7` | Keep the N newest `openblock-<world>-*` archives; minimum 1 |
| `--verify` | off | Extract `world.obs` from the archive and check the `OBSV` magic |
| `--quiet` | off | Errors only |

What the script does:

1. Validates the world directory and `world.obs`;
2. derives the world basename and a UTC timestamp;
3. creates a temporary archive with `tar --zstd` (or `tar -z` when zstd is not
   available) containing the world directory as its top-level entry;
4. lists the archive, requires `<world>/world.obs`, and rejects absolute or
   `..` member paths;
5. with `--verify`, extracts `world.obs` and checks the first four bytes are
   `OBSV`;
6. atomically renames the temporary file to
   `openblock-<world>-<timestamp>.tar.zst` (or `.tar.gz`) and sets mode `0600`;
7. prunes older archives for the same world prefix, keeping `--retention`.

Expected output:

```text
backup: /var/lib/openblock/worlds/world -> /var/backups/openblock/openblock-world-20261125T030000123456789Z.tar.zst (zstd)
verify: world.obs magic OBSV
created: /var/backups/openblock/openblock-world-20261125T030000123456789Z.tar.zst (392K)
pruned: /var/backups/openblock/openblock-world-20261118T030000123456789Z.tar.zst
retention: keeping 7 archive(s) (limit 7), pruned 1
```

Archive naming is per world basename, and characters outside
`[A-Za-z0-9._-]` in the filename prefix are replaced with `_`. The world
directory itself keeps its real name inside the archive, so directories with
spaces restore correctly. Two different world directories with the same
basename should use different `--dest` directories; otherwise their retention
sets are merged.

### Verify an existing archive

`--verify` applies to the archive just created. To verify an old archive:

```sh
archive=/var/backups/openblock/openblock-world-20261125T030000123456789Z.tar.zst
tar --zstd -tf "$archive" | head
# world/
# world/world.obs
# world/player.txt
# ...

mkdir -p /tmp/archive-check
tar --zstd -xf "$archive" -C /tmp/archive-check world/world.obs
head -c 4 /tmp/archive-check/world/world.obs; echo
# OBSV
```

For a gzip archive, replace `--zstd` with `-z` (or use `tar -xf`, which
auto-detects on GNU tar). `tar -tf` alone proves the archive is readable; the
`OBSV` check proves the payload is an OpenBlock snapshot, not an arbitrary file.
Neither replaces a real restore test.

## Restore

Always stop the server first and take a backup of the **current** world before
replacing it, even if the archive is known-good.

```sh
sudo systemctl stop openblock-server

sudo scripts/server-restore.sh \
  --archive /var/backups/openblock/openblock-world-20261125T030000123456789Z.tar.zst \
  --dest /var/lib/openblock/worlds/world \
  --force --yes \
  --owner openblock:openblock

sudo systemctl start openblock-server
journalctl -u openblock-server -n 30 --no-pager
```

| Option | Meaning |
| --- | --- |
| `--archive FILE` | Archive created by `server-backup.sh` |
| `--dest DIR` | Exact target world directory (not the data root) |
| `--force` | If `DIR` exists, rename it to `DIR.bak-<timestamp>` before restoring |
| `--yes` | Skip the confirmation prompt (required with `--force` when stdin is not a terminal) |
| `--owner USER:GROUP` | `chown -R` the restored directory, normally `openblock:openblock` |
| `--help` | Usage |

The script lists the archive, rejects absolute/`..` paths, requires exactly one
`world.obs`, extracts into a staging directory next to the destination, checks
the `OBSV` magic, and only then moves the world into place. With `--force`, the
old world is renamed aside and never deleted by the script. Without `--force`, an
existing destination is refused.

Expected output:

```text
archive: /var/backups/openblock/openblock-world-20261125T030000123456789Z.tar.zst (392K)
target:  /var/lib/openblock/worlds/world
existing target will be preserved as: /var/lib/openblock/worlds/world.bak-<timestamp>
preserved: /var/lib/openblock/worlds/world.bak-20261125T031500987654321Z
restored: /var/lib/openblock/worlds/world (8 files, 396K)
world.obs magic: OBSV
previous world: /var/lib/openblock/worlds/world.bak-20261125T031500987654321Z

Next:
  1. If not using --owner: sudo chown -R openblock:openblock '/var/lib/openblock/worlds/world'
  ...
```

For a non-root test, restore into a temporary data root you own:

```sh
scripts/server-restore.sh --archive ./backups/openblock-world-....tar.zst \
  --dest /tmp/restore-check/worlds/world --yes
./target/release/openblock-server --data-dir /tmp/restore-check --world world --ticks 20
```

The `--ticks` run loads the world, ticks it and saves once without opening a
listener. A successful run logs the loaded chunk count and exits 0; a corrupt or
newer-format world fails with an error instead of silently starting a new one.

## Retention and disk planning

* `--retention` applies to `openblock-<world>-*.tar.zst` and
  `openblock-<world>-*.tar.gz` in `--dest`; unrelated files are untouched.
* The newest archive is always kept; `--retention 1` keeps exactly one.
* A 169-chunk reference world compresses to roughly 0.4 MiB; worlds grow with
  exploration and build size. Measure after a week and size retention
  accordingly. Never let backups fill the same filesystem as the world: a full
  disk turns the next autosave into a failure.
* Keep at least one archive off the host (rsync/rclone/object storage) so a host
  loss is recoverable.

## Test the backup path (repository test)

The scripts have a non-root integration test that generates a smoke world,
creates three archives with `--retention 2`, verifies magic/mode/listing,
restores and compares bytes, checks the no-`--force` refusal and forced-restore
preservation, and exercises the gzip fallback:

```sh
OPENBLOCK_SERVER=./target/release/openblock-server \
  scripts/tests/test-backup-restore.sh
```

Expected tail:

```text
ok: smoke world generated
ok: retention kept 2 archives
ok: archives are mode 600
ok: archive contains world/world.obs
ok: restore writes OBSV world.obs
ok: restored world.obs matches archive
ok: restore refuses to overwrite without --force
ok: forced restore preserves the previous world
ok: gzip fallback creates .tar.gz
ok: gzip archive restores

PASS: backup/restore tests
```

Run it after changing either script, on a host with `tar`, `zstd` or `gzip`, and
the server binary. It needs no root and leaves no state outside `mktemp -d`.

## Restore drill before you need it

1. Stop the server.
2. Create a fresh backup and verify it.
3. Restore the **previous** archive into `/tmp/restore-drill/worlds/world`.
4. Run `openblock-server --data-dir /tmp/restore-drill --world world --ticks 20`.
5. Optionally start a loopback listener on a spare port and join with a client.
6. Delete `/tmp/restore-drill` and record the archive name, date and result in
   your runbook.

If step 4 fails, do not overwrite the production world; investigate the archive
or choose an older one. Keep the old world directory (`.bak-*`) until a player
has joined and confirmed the expected build.

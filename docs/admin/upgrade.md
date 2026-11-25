# Upgrade and downgrade

The server is a single binary plus one world directory. Upgrades are therefore
mostly "stop, back up, replace the binary, start" — but the save format and
content fingerprint are compatibility boundaries, so do not skip the backup.

There is **no self-update** in this build. The server makes no outbound
connections and will not fetch a new version. systemd, `dnf`, Docker or your own
deployment tool performs the upgrade.

## Before every upgrade

```sh
# 1. Record what is running.
/usr/bin/openblock-server --version
# openblock-server ... (save format 4, protocol ..., linux/x86_64)
# save format: 4
# protocol: 2 (interim JSON over TCP)

# 2. Stop cleanly; this writes a final save.
sudo systemctl stop openblock-server

# 3. Take and verify a backup (see backup.md).
sudo scripts/server-backup.sh \
  --world /var/lib/openblock/worlds/world \
  --dest /var/backups/openblock --retention 14 --verify

# 4. Keep the previous binary and its build info.
sudo cp -a /usr/bin/openblock-server /usr/bin/openblock-server.previous
```

Also read the release notes/CHANGELOG for the target version. Within 1.0.x a
patch release keeps save format 4 and protocol 4; a later minor may add a save
migration or protocol revision, and the changelog says which. Do not mix a new
server with an old client if the protocol version changed; the server rejects
mismatched clients with `protocol_mismatch`. Pre-1.0 builds are unsupported and
may not read a 1.0 world.

## Upgrade from source

```sh
sudo systemctl stop openblock-server          # already stopped if you did the preflight
git fetch --tags
git switch --detach v<target>                 # or build a reviewed tag
cargo build --release --locked -p ob_server --bin openblock-server

# Keep the old binary until the new one is proven.
sudo install -m 0755 target/release/openblock-server /usr/bin/openblock-server
sudo systemctl daemon-reload                  # only needed for unit changes
sudo systemctl start openblock-server
journalctl -u openblock-server -n 50 --no-pager
```

Verify the loaded version and world before inviting players:

```sh
/usr/bin/openblock-server --version
ss -ltnp | grep ':25565'
journalctl -u openblock-server -n 20 --no-pager | grep -E 'world|starting TCP|tps'
```

There is no HTTP health endpoint; the listener plus a client join is the real
readiness check. A quick load test is cheap:

```sh
python3 scripts/load-test.py --clients 4 --seconds 10 --port 25565
```

### Fedora RPM

```sh
sudo systemctl stop openblock-server
# Back up first, as above.
sudo dnf upgrade openblock-server
sudo systemctl status openblock-server --no-pager
journalctl -u openblock-server -n 50 --no-pager
```

The packaged config is `%config(noreplace)`: local edits are kept, and a new
default config is installed as `.rpmnew` if the packaged file changed. Compare
the two before merging:

```sh
sudo diff -u /etc/openblock/server.toml /etc/openblock/server.toml.rpmnew
```

RPM scriptlets reload/restart the unit on upgrade (the spec uses
`%systemd_post`, `%systemd_preun` and `%systemd_postun_with_restart`; the unit
must be stopped cleanly first). The world lives under `/var/lib/openblock`, not
in the package, so an RPM upgrade does not replace it.

### Container

```sh
# Stop, back up the volume, then recreate with the new image.
cd packaging/container
docker compose stop openblock
# Back up $VOLUME/worlds/world from the host or a helper container; see backup.md.
docker compose pull        # if the image comes from a registry
docker compose up -d --force-recreate
docker compose ps
docker compose logs --tail=100 openblock
```

Pin a digest/tag for production; do not use a floating `latest` tag for a server
you cannot roll back. Keep the previous image ID:

```sh
docker image inspect openblock-server:local --format '{{.Id}} {{.Created}}'
```

To roll back a container, stop it, start the previous image ID, and restore the
pre-upgrade volume if the new version already loaded/saved the world.

## Save-format compatibility

The current save format is **4** (`--version` prints it). The loader:

* reads v1–v3 worlds and migrates them in memory on load; the next save writes
  the current format;
* rejects a world written by a **newer** format with an unsupported-version
  error instead of silently discarding data;
* validates the snapshot magic (`OBSV`) and structure; a corrupt file fails to
  load rather than starting an empty world.

Consequences:

* **Upgrading** an old world is usually automatic. Back it up anyway; the
  migration is first exercised when the new binary loads it.
* **Downgrading** after the new binary saved a v4 world may be refused by an
  older binary that does not understand the format. Restore the pre-upgrade
  backup and the previous binary together; do not point an old binary at the
  migrated world and expect it to work.
* A crash during migration/save leaves the previous complete `world.obs` and a
  `.tmp` file (per-file atomic writes). Restore from backup if the loader
  reports corruption.

The world seed is stored in the world; changing `--seed` on an existing world
does not regenerate it. A v4 save records the registry/content fingerprint and
the loader rejects a mismatch with a clear error; changing the server's content
pack can therefore make an existing world fail to open. Keep the pack that
matches the world, or intentionally start a new world with the new pack. Clients
must also use the matching pack or they reject the handshake with a fingerprint
mismatch. Older v1/v2 saves carry no fingerprint; v3 saves with an append-only
mismatch are accepted with a warning.

## Rollback procedure

```sh
sudo systemctl stop openblock-server

# 1. Put the previous binary back.
sudo install -m 0755 /usr/bin/openblock-server.previous /usr/bin/openblock-server

# 2. Put the pre-upgrade world back if the new binary saved it.
sudo scripts/server-restore.sh \
  --archive /var/backups/openblock/openblock-world-<pre-upgrade>.tar.zst \
  --dest /var/lib/openblock/worlds/world --force --yes \
  --owner openblock:openblock

# 3. Start the old version and confirm.
sudo systemctl start openblock-server
sudo /usr/bin/openblock-server --version
journalctl -u openblock-server -n 30 --no-pager
```

Keep the `.bak-*` directory and the archive until the rollback is confirmed. The
restore script never deletes the replaced world.

## Post-upgrade checklist

- [ ] `--version` shows the expected version, save format and protocol.
- [ ] The service is active and the TCP listener exists (`ss -ltnp`).
- [ ] Journal has no `unknown key`, `unsupported save version`, content
      fingerprint or permission errors.
- [ ] `world.obs` timestamp advances after an autosave/stop.
- [ ] A real client joins, sees the expected spawn/build, and can edit a block.
- [ ] `tps` is at the target (20.0) and no tick-overrun warnings are repeating.
- [ ] A fresh backup from the upgraded world verifies and can be restored into a
      temporary directory.
- [ ] The previous binary and pre-upgrade backup are retained for the rollback
      window.

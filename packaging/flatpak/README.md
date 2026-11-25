# Flatpak manifest (staged)

`io.github.martintondel02.OpenBlock.yml` is the **staged** client manifest for
the first Flathub submission. It is versioned with the source so the package can
be reproduced from a reviewed tag, but it is **not published for v1.0.0 and is
not yet buildable as committed**.

## Blockers

1. **`cargo-sources.json` is missing.** The sandbox has no network during a
   Flatpak build and the manifest sets `CARGO_NET_OFFLINE=true`, so the Cargo
   dependency graph must be vendored as a generated source file. The commented
   source entry in the manifest is the place to add it.
2. **First Flathub review has not happened.** The app ID, metainfo, screenshots
   and permissions need a reviewer; the release notes should not claim a
   Flathub package until it is live.
3. **No `.obworld` archive import.** The desktop file deliberately no longer
   advertises a file type the client cannot open, and the MIME definition is not
   installed. Add both only when archive import exists.

## Generate the cargo sources

Run this from the repository root at the release tag, on a machine with network
access:

```sh
flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json
# Keep the file next to this manifest, uncomment the `type: file` source, and
# commit both in the packaging change.
```

`flatpak-cargo-generator.py` is part of
[flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools); pin the
tool commit in the packaging PR description so the generated file is reviewable.

## Build and validate

```sh
flatpak-builder --user --install --force-clean build-dir \
  packaging/flatpak/io.github.martintondel02.OpenBlock.yml
flatpak run io.github.martintondel02.OpenBlock
# Optional sandbox/metadata checks:
flatpak-builder --run build-dir packaging/flatpak/io.github.martintondel02.OpenBlock.yml openblock --version
flatpak run --command=sh io.github.martintondel02.OpenBlock -c 'ls /app/bin /app/share/metainfo'
```

The manifest builds only `ob_client`/`openblock`; the dedicated server is not
part of this Flatpak.

## Permissions

`finish-args` is limited to what the client needs: Wayland with an explicit
XWayland fallback, DRI for Vulkan, PipeWire through `--socket=pulseaudio`,
shared IPC, and network for multiplayer. There is deliberately no `--filesystem`
or `--device=all`: saves/config live in the sandbox XDG directories, and a
resource pack is selected from a path the user grants through a portal.

## Publishing checklist

- [ ] `cargo-sources.json` generated from the release `Cargo.lock` and committed.
- [ ] Git `tag`/`commit` pinned to the release commit (currently
      `19e489e63b6aa656f6569f81c1c8b4fb8ed3c74a` for `v1.0.0`).
- [ ] `cargo build --release --locked --offline -p ob_client --bin openblock`
      succeeds inside the sandbox.
- [ ] `appstreamcli validate --no-net packaging/fedora/openblock.metainfo.xml`
      passes (the same file is installed as the app's metainfo).
- [ ] The Flathub PR is linked from the release notes and the release asset list
      is updated; do not attach a hand-built `.flatpak` without saying it is
      unsigned/unofficial.

## Legal

No Minecraft assets are bundled. The client generates procedural textures and
loads a user-selected resource pack from local files at runtime; the Flatpak
must not add a default pack. See [packaging/README.md](../README.md) §
Provenance and [SECURITY.md](../../SECURITY.md).

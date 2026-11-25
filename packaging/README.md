# Packaging

OpenBlock is **Linux-first and Fedora-first** ([DEC-054](../docs/decisions.md)): the
supported window is Fedora's current + previous release at any ship date
(Fedora 44/45 while this packaging was written), so the RPM and Flatpak builds
move forward with Fedora's cycle.
This directory holds the packaging sources for the primary platform; the
secondary platforms' packaging is described in
[spec/22](../docs/spec/22-platforms-and-distribution.md).

## Publication status for v1.0.0

**No binary package was published for the 1.0.0 release.** The GitHub release
carries four screenshots and nothing else: no source tarball, no `.rpm`, no
Flatpak bundle, no container image and no checksum/SBOM manifest. Everything in
this directory is a **staged recipe**, kept versioned with the source so the
first 1.0.x packaging wave can publish from a reviewed tag. The 1.0.1 audit
records the gap and the proposed release checklist in
[`docs/plan/1.0.1-audit.md`](../docs/plan/1.0.1-audit.md); use the source build
instructions in [`docs/admin/quickstart.md`](../docs/admin/quickstart.md) until
then.

## Artifact priority

| Priority | Artifact | Audience | 1.0.0 status | Source of truth |
| --- | --- | --- | --- | --- |
| 1 | **Flathub Flatpak** (`io.github.martintondel02.OpenBlock`) | Fedora and other Linux players | Staged; **not published** — the manifest still needs a generated `cargo-sources.json` (see [`flatpak/README.md`](flatpak/README.md)) | `packaging/flatpak/` |
| 2 | **COPR RPM** `openblock` (client) and `openblock-server` | Fedora users who prefer native packages and system integration | Staged; **not published** — needs a release tarball with vendored crates and COPR build validation | `packaging/fedora/openblock.spec` |
| 3 | AppImage and `.tar.zst` | Portable/manual installs, other distributions | **Not implemented** — `xtask dist` still prints `Planned: dist, …` | planned |
| 4 | Official Fedora repository package | Long-term goal; requires unbundling Rust crates via cargo2rpm | Post-1.0 (OPEN-021) | — |
| 5 | Server container image | Cloud and non-Fedora hosts | Recipe validated by inspection/CI lint; **no registry image published** | `packaging/container/Dockerfile` |

The Fedora RPM currently bundles the Rust crates and is therefore a **COPR
package, not Fedora-repository-compliant**. That is a deliberate, documented
trade-off until OPEN-021 is decided; the Flatpak has the same bundling model and
is the intended player-facing default once published.

## Build and verify locally (Fedora 44/45)

`rpmbuild` and `flatpak-builder` are **not** installed in this audit
environment, so the recipe was validated by inspection, XML/desktop parsers and
CI's Fedora-container job rather than by producing an RPM or a Flatpak here.

```sh
# RPM: the spec's Source0 is a release tarball that contains Cargo.lock and a
# `cargo vendor` tree (see the header of packaging/fedora/openblock.spec).
# The v1.0.0 release does not include that tarball yet, so a direct
# `rpmbuild -ba packaging/fedora/openblock.spec` cannot fetch Source0.
rpmbuild -ba packaging/fedora/openblock.spec

# Lint the built RPMs once they exist (rpmlint consumes packages, not the spec):
rpmlint ~/rpmbuild/RPMS/*/openblock*.rpm
# Verify the units/metadata without building an RPM:

desktop-file-validate packaging/fedora/io.github.martintondel02.OpenBlock.desktop
appstreamcli validate --no-net packaging/fedora/openblock.metainfo.xml
# The unit needs a stub binary for ExecStart; the Fedora CI job already does this.
sudo install -Dm755 /bin/true /usr/bin/openblock-server
sudo systemd-sysusers packaging/systemd/openblock-server.sysusers
sudo systemd-tmpfiles --create packaging/systemd/openblock-server.tmpfiles
sudo install -Dm644 packaging/systemd/server.toml /etc/openblock/server.toml
sudo systemd-analyze --man=no verify packaging/systemd/openblock-server.service

# Flatpak: blocked until cargo-sources.json is generated and committed.
# See packaging/flatpak/README.md for the exact steps.
flatpak-builder --user --install --force-clean build-dir packaging/flatpak/io.github.martintondel02.OpenBlock.yml
flatpak run io.github.martintondel02.OpenBlock
```

CI runs the Fedora-container checks in `fedora:44` and `fedora:45`; see
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Dedicated server on Fedora Server

Once the COPR project is published (not yet for 1.0.0), the packaged path is:

```sh
sudo dnf copr enable openblock/openblock
sudo dnf install openblock-server
sudo systemctl enable --now openblock-server
sudo firewall-cmd --permanent --add-service=openblock-server
sudo firewall-cmd --reload
journalctl -u openblock-server -f
```

Until then, install the binary, sysusers/tmpfiles, unit and default config by
hand as shown in [`systemd/README.md`](systemd/README.md).

The package installs `/etc/openblock/server.toml` and keeps worlds in
`/var/lib/openblock` via `StateDirectory=openblock`. The unit runs as the
unprivileged `openblock` user and is sandboxed (`ProtectSystem=strict`,
`ProtectHome=read-only`, `NoNewPrivileges=true`, `PrivateTmp=true`). The
firewalld service opens **TCP/25565** for the interim JSON/TCP protocol; there
is no TLS/authentication yet, so restrict it to a trusted LAN/VPN. SELinux stays
enforcing: no custom policy module is required or shipped. See the
[admin handbook](../docs/admin/README.md) and
[spec/19 §3.1.1](../docs/spec/19-server-administration.md#311-fedora-server-primary-deployment).

## Server container

```sh
# From the repository root
docker build -f packaging/container/Dockerfile -t openblock-server:local .
docker run --rm openblock-server:local --version
docker compose -f packaging/container/docker-compose.yml config
```

The image is multi-stage (`rust:1.100-alpine` builder, `alpine:3.22` runtime),
runs as UID 10001 with `/data` as its only volume, and contains no wgpu/winit/
X11/Wayland/Vulkan/PulseAudio/ALSA libraries. The Compose example publishes only
`127.0.0.1:25565`; see [`container/README.md`](container/README.md). A full
image build is a manual CI job because a cold Rust build is expensive; the push
CI job validates the Dockerfile and Compose file.

## Release checklist (proposed for 1.0.1 and later)

The v1.0.0 release shipped screenshots only, so the packaging/release trail is
incomplete. Before the next release, attach and link from the release notes:

1. **Source tarball** `openblock-<version>.tar.gz` containing `Cargo.lock`, the
   full workspace and a generated `vendor/` tree (the RPM's `Source0`); publish
   its SHA-256 and BLAKE3 sums.
2. **Checksums** `SHA256SUMS` (and a signed manifest or minisign/Ed25519
   signature) covering every attached artifact, per REQ-PLAT-018.
3. **SBOM** (CycloneDX or SPDX) generated from `Cargo.lock`, plus the license
   notice for bundled crates.
4. **RPM/COPR** build from the source tarball, with the per-crate
   `Provides: bundled(crate(...))` list generated from `Cargo.lock`; attach the
   RPMs or publish the COPR project.
5. **Flatpak** build after generating `cargo-sources.json` from `Cargo.lock`;
   attach the `.flatpak` bundle or publish on Flathub.
6. **Server container** digest (`docker image inspect --format '{{index .RepoDigests 0}}'`)
   and the Compose file used for the smoke test.
7. **Screenshots** (already attached for v1.0.0) and the release-notes link to
   the exact tag and this checklist.

Do not publish a partial asset set: either attach every artifact the release
notes promise, or state explicitly what is source-only.

## Files

| File | Purpose |
| --- | --- |
| `fedora/openblock.spec` | RPM spec: client, desktop integration, and server subpackage (COPR staging, not published) |
| `systemd/openblock-server.service` | Hardened systemd unit for the dedicated server |
| `systemd/openblock-server.sysusers` | Dedicated `openblock` system user |
| `systemd/openblock-server.tmpfiles` | Backup directory creation |
| `systemd/openblock-server.env.example` | Optional `OPENBLOCK_LOG` environment |
| `systemd/server.toml` | Default flat server configuration installed by the RPM |
| `systemd/README.md` | Manual install, firewall, console and validation notes |
| `fedora/openblock-server.firewalld.xml` | firewalld service definition for the TCP game port (25565/tcp) |
| `container/Dockerfile` | Multi-stage CPU-only server image |
| `container/docker-compose.yml` | Loopback-only reference Compose deployment |
| `container/README.md` | Build, run, healthcheck, backup and size notes |
| `fedora/io.github.martintondel02.OpenBlock.desktop` | Desktop entry (matches the Flatpak app ID) |
| `fedora/openblock.metainfo.xml` | AppStream metadata (required by Flathub and GNOME Software) |
| `fedora/openblock.mime.xml` | Shared MIME-info for `.obworld` archives; **not installed in 1.0** because archive import is not implemented |
| `fedora/io.github.martintondel02.OpenBlock.svg` | Project icon (CC0); a store-quality icon remains post-1.0 polish |
| `flatpak/io.github.martintondel02.OpenBlock.yml` | Flatpak manifest (staged; needs generated cargo sources) |
| `flatpak/README.md` | Flatpak blocker, generation steps and validation notes |

## Provenance and third-party assets

All packaging files are MIT OR Apache-2.0 (the icon is CC0-1.0) and carry REUSE
metadata in [`REUSE.toml`](../REUSE.toml). The package name and desktop/AppStream
IDs are working identifiers pending trademark clearance (DEC-037); renaming
must update this directory, the Flatpak ID and the repository URL in one change
and cannot silently change within a stable 1.x package.

No Minecraft code, textures, sounds or other third-party game assets are
bundled by any package. The client's procedural textures are generated at
build/run time; resource packs are read from user-owned local files selected at
runtime (`--texture-pack PATH`) and are never redistributed. Respect each
pack's licence; do not add a pack to this directory or to a release artifact.

## Audio runtime requirements

The client synthesises all sounds in memory; no sound files are bundled. Linux output uses the
PulseAudio simple API loaded at runtime (`libpulse-simple.so.0`), so no ALSA/PulseAudio development
headers are needed to build or package:

- **Flatpak:** `finish-args` includes `--socket=pulseaudio`, so the sandbox reaches the host's
  PipeWire/PulseAudio server. No `--device=all` or extra filesystem access is required.
- **Fedora RPM:** no audio `BuildRequires`; `pulseaudio-libs` is a weak `Recommends` because the
  library is dlopened and the client still runs silently without it.
- **Ubuntu CI:** no extra audio packages; the crate builds and tests without a sound server.
- **Windows/macOS:** compile the silent stub; no system audio library is linked. Real output on
  these platforms is post-1.0.
- **Dedicated server:** `ob_server` does not link `ob_audio` and needs no audio device.

A host with no output device still runs: the client logs one line, keeps all audio calls as no-ops,
and `--no-audio` skips device enumeration entirely. See
[spec/14 §0](../docs/spec/14-audio.md).

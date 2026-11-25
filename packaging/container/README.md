# OpenBlock server container

A small, non-root, read-only-root container for the interim `openblock-server`.
The image runs on a CPU-only host: `ob_server` has no `wgpu`, `winit`, X11,
Wayland, Vulkan, PulseAudio or ALSA dependency, and the runtime image contains
none of those libraries.

| Property | Value |
| --- | --- |
| Build context | repository root (`-f packaging/container/Dockerfile .`) |
| Builder | `rust:1.100-alpine` (musl, native arch) |
| Runtime | `alpine:3.22` + `netcat-openbsd` |
| User | UID/GID `10001` (`openblock`), no login shell |
| Port | `25565/tcp` |
| Volume | `/data` (worlds live in `/data/worlds/<name>/`) |
| Healthcheck | `nc -z 127.0.0.1 25565` (TCP liveness; no HTTP endpoint in this build) |
| Signals | `SIGTERM` and `SIGINT` save and exit; image `STOPSIGNAL SIGTERM`, compose `stop_signal: SIGINT` (both handled), 40 s grace |

There is no shell in the runtime image beyond Alpine's busybox `sh` (kept for
`nc`/troubleshooting); `cap_drop: ALL`, `no-new-privileges` and
`read_only: true` are the intended deployment settings.

## Build

```sh
# From the repository root. The builder needs network access to fetch crates
# unless a build cache/registry mirror is configured.
docker build -f packaging/container/Dockerfile -t openblock-server:local .

# Rootless alternative
podman build -f packaging/container/Dockerfile -t openblock-server:local .
```

The Rust build happens only in the builder stage. Nothing from the builder
(`cargo`, Rust sources, the client, GPU/audio crates) is copied to the runtime
image.

### Verify the image

```sh
docker run --rm openblock-server:local --version
docker image inspect openblock-server:local --format 'User={{.Config.User}} Size={{.Size}}'
# Expected: User=10001:10001, Size roughly 11-20 MB (base + binary + nc).
# The musl binary depends only on /lib/ld-musl-x86_64.so.1.
docker run --rm --entrypoint sh openblock-server:local \
  -c 'ldd /usr/local/bin/openblock-server || true; ls -l /data'

# No GPU/audio/window libraries may appear:
docker run --rm --entrypoint sh openblock-server:local \
  -c 'ldd /usr/local/bin/openblock-server 2>&1 | grep -Ei "vulkan|X11|wayland|pulse|asound|GL" && exit 1 || exit 0'
```

`cargo tree -p ob_server` is checked in CI by
[`scripts/check-server-deps.sh`](../../scripts/check-server-deps.sh); it rejects
`wgpu`, `winit`, X11/Wayland, Vulkan, PulseAudio and ALSA crates.

## Run

```sh
# Local-only, ephemeral scratch world (delete the container afterwards)
docker run --rm -p 127.0.0.1:25565:25565/tcp openblock-server:local

# Persistent named volume
docker volume create openblock-data
docker run -d --name openblock-server \
  -p 127.0.0.1:25565:25565/tcp \
  --read-only --cap-drop=ALL \
  --security-opt no-new-privileges:true \
  --stop-timeout 40 \
  -v openblock-data:/data \
  openblock-server:local

docker logs -f openblock-server
```

`--read-only` works because all mutable state is below `/data`; the image's
`/data` is pre-owned by 10001, so empty named volumes inherit the ownership.

The TCP healthcheck probes the listener without sending a `hello`, so the
server logs a `handshake failed`/timeout warning for each probe (every 30 s).
That line is expected liveness noise, not a client failure; use the `tps` line
and a real client join as readiness evidence.

Podman builds OCI images by default and drops Docker's `HEALTHCHECK` unless you
pass `--format docker`; the Compose example defines its own healthcheck, so
`docker compose ps` still reports health for both runtimes.

With a host bind mount, create it explicitly:

```sh
mkdir -p ./data && sudo chown 10001:10001 ./data
docker run --rm -p 127.0.0.1:25565:25565/tcp -v "$PWD/data:/data" openblock-server:local
```

Join with the client:

```sh
openblock --connect 127.0.0.1:25565 --name alice
```

## Compose

[`docker-compose.yml`](docker-compose.yml) is a loopback-only single-host
example with a named volume, resource hardening and the TCP healthcheck:

```sh
cd packaging/container
docker compose up -d --build
docker compose logs -f openblock
docker compose ps                 # healthy once the listener accepts TCP
docker compose down               # sends SIGINT and waits up to 40 s
```

The compose file deliberately publishes `127.0.0.1:25565:25565/tcp`. The server
binds `0.0.0.0` *inside* the container so the published port can reach it; the
host mapping restricts who can connect. Do not change it to `0.0.0.0:25565` or
port-forward it publicly without a VPN/reverse proxy and a firewall: the
interim protocol is plaintext with no authentication.

## Configuration

The entrypoint is:

```text
openblock-server --data-dir /data --listen 0.0.0.0:25565
```

To use a config file, mount it at `/data/server.toml` and append
`--config /data/server.toml`:

```sh
docker run ... -v "$PWD/server.toml:/data/server.toml:ro" \
  openblock-server:local --config /data/server.toml
```

Use the flat format from
[`packaging/systemd/server.toml`](../systemd/server.toml), but keep
`data_dir = "/data"` (or omit it: `--data-dir /data` wins on the command line).
`listen` in the file is also overridden by the entrypoint's `--listen`, so the
container remains reachable on the published port. Config changes require a
restart; there is no reload.

## Backups and upgrades

The container has no backup scheduler or volume snapshot integration. Stop the
container (or accept a live-copy risk), then run the host-side scripts against
the volume:

```sh
docker compose stop openblock
# Named volumes are prefixed with the Compose project name; find the real name.
vol_name=$(docker volume ls --format '{{.Name}}' | grep openblock-data | head -n1)
vol=$(docker volume inspect "$vol_name" --format '{{.Mountpoint}}')
sudo scripts/server-backup.sh --world "$vol/worlds/world" \
  --dest /var/backups/openblock --retention 7 --verify
docker compose start openblock
```

To upgrade, take a backup, replace the image/tag, and recreate the container:

```sh
docker compose pull        # when using a registry image
docker compose up -d --force-recreate
docker compose ps
docker compose logs --tail=50 openblock
```

Keep the previous image ID (`docker image inspect`) and the pre-upgrade backup
until the new server has started cleanly and a client has rejoined. See
[docs/admin/upgrade.md](../../docs/admin/upgrade.md).

## Size and build notes

* The builder stage is discarded; only the musl `openblock-server` binary is
  copied. The runtime package set is `alpine` + `netcat-openbsd`; the observed
  local image size was about 11 MB.
* Build time is dominated by compiling the Rust workspace for musl. Use a
  registry/build cache in CI; the full image build is a manual CI job because a
  cold build is several minutes.
* The image is single-arch by default. For multi-arch, use `docker buildx build
  --platform linux/amd64,linux/arm64` (or podman's `--platform`) on native
  runners; the Rust builder is multi-arch.
* `ob_server` has no Cargo features, so the build intentionally does not pass
  `--no-default-features`. The dependency check is the guard that keeps GPU,
  window and audio crates out of this image.

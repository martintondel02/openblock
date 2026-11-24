# 19 — Server Administration

> Status: normative - Last updated: 2026-11-24
> Depends on: [DEC-003](../decisions.md), [DEC-024](../decisions.md), [DEC-026](../decisions.md), [DEC-028](../decisions.md), [DEC-033](../decisions.md), [DEC-035](../decisions.md), [DEC-038](../decisions.md), [DEC-042](../decisions.md), [DEC-044](../decisions.md), [DEC-046](../decisions.md), [DEC-047](../decisions.md); [architecture](01-technical-architecture.md), [engine core](02-engine-core.md), [gameplay](08-gameplay-survival.md), [persistence](17-persistence.md), [networking](18-networking-multiplayer.md), [security](20-security-and-anti-cheat.md); [research 04](../research/04-networking-sota.md) §5, [research 09](../research/09-persistence-assets-packaging.md) §6.7
> Implements: MS-07

## 1. Purpose and scope

This chapter specifies the dedicated server as a product: how it is deployed, the complete
configuration surface, world and player administration, the console and RPC, observability, backups
and disaster recovery, upgrades, capacity planning, the security checklist, operational runbooks,
and the support policy. It is the contract between the engine and the person who runs a world for
other people ([DEC-028](../decisions.md)).

The server is the same `Host` as the integrated server ([01](01-technical-architecture.md) §3.1), a
single binary with no GPU, window or audio dependency, plus one world directory. Everything here is
about operating that binary, not about gameplay: game rules live in [08](08-gameplay-survival.md),
wire behaviour in [18](18-networking-multiplayer.md), the save format in [17](17-persistence.md), and
abuse policy in [20](20-security-and-anti-cheat.md).

## 2. Requirements

| ID | Requirement | Verified by | Milestone |
| --- | --- | --- | --- |
| REQ-SRV-001 | `openblock-server` MUST run headless on Linux, Windows and macOS with no GPU, window, audio or display dependency ([DEC-003](../decisions.md)). | `arch-guard`, container CI on a GPU-less host | MS-07 |
| REQ-SRV-002 | Every configuration key MUST have a documented type, default, range and reload class; an unknown key MUST fail start-up with its dotted path and a suggestion. | Generated schema test, typo fixtures | MS-07 |
| REQ-SRV-003 | The binary MUST emit a commented default `server.toml` on first run and `--print-config` MUST print the resolved configuration with secrets redacted. | Golden-file test | MS-07 |
| REQ-SRV-004 | Configuration MUST resolve defaults < user file < world file < environment < CLI ([02](02-engine-core.md) §3.8); secrets MUST never appear in logs or diagnostics. | Precedence matrix, redaction test | MS-07 |
| REQ-SRV-005 | A `live` key MUST apply within 1 s of the file changing; a `session` or `next_session` key MUST be reported as pending, and a `restart` key MUST be reported but not applied. A rejected reload MUST keep the last good document. | Reload tests per class | MS-07 |
| REQ-SRV-006 | `SIGTERM`, `SIGINT` and console `stop` MUST drain, flush dirty state and exit within the configured deadlines (default 30 s drain, 5 s flush); exit codes MUST follow [REQ-CORE-031](02-engine-core.md). | Signal tests, systemd and container runs | MS-07 |
| REQ-SRV-007 | World management MUST support list, create, load, delete, border, pregeneration, backup, restore, verify, import and export; exactly one world MUST be active per session. | Command tests | MS-07 |
| REQ-SRV-008 | `world delete` MUST refuse without `--confirm` and a successful backup; a deleted world's files MUST be moved to a trash directory outside `saves/`, not unlinked immediately. | Negative tests, restore test | MS-07 |
| REQ-SRV-009 | Pregeneration MUST be resumable, report progress, honour a thread budget and never starve the tick; generated columns MUST be identical to live generation ([DEC-046](../decisions.md)). | Pregen resume test, hash comparison | MS-08 |
| REQ-SRV-010 | Chunk regeneration MUST be an explicit command with a dry-run report and a backup; a changed generator fingerprint MUST never regenerate silently ([DEC-047](../decisions.md)). | Fingerprint-change test | MS-13 |
| REQ-SRV-011 | Backups MUST run on a schedule, before migration and on demand; retention MUST be bounded by count and age; every backup MUST be verified and recorded. | Backup/restore suite | MS-07 |
| REQ-SRV-012 | A backup MUST be consistent: flush and fsync first, then link or copy immutable region files ([17](17-persistence.md) §3.8); a partial backup MUST be marked failed, never used. | Kill-during-backup test | MS-07 |
| REQ-SRV-013 | Restore MUST target a new directory, verify every region header, index and hash before swapping under the world lock, and keep the replaced world for rollback. | Restore drill | MS-07 |
| REQ-SRV-014 | Ops, whitelist, bans, mute and kick MUST be available from console and RPC; bans and whitelists MUST be keyed on the player's public key ([DEC-026](../decisions.md)); every action MUST be audited. | Admin test matrix | MS-07 |
| REQ-SRV-015 | The audit log MUST be append-only JSON lines, mode 0600, hash-chained, rotated, and free of IP addresses unless explicitly enabled. | Inspection, tamper-evidence test | MS-15 |
| REQ-SRV-016 | A local console over stdin and a mode-0600 Unix socket MUST be available; `help` MUST be generated from the command table. | Console tests | MS-07 |
| REQ-SRV-017 | The remote console and RPC MUST be disabled by default, bind to loopback when enabled, require TLS 1.3 and a token file, be rate-limited and audited. | Exposure and auth tests | MS-07 |
| REQ-SRV-018 | An optional RCON-compatible listener MAY be enabled for existing tooling; it MUST be off by default, loopback-only and documented as plaintext. | Config and socket tests | MS-07 |
| REQ-SRV-019 | Logs MUST be structured JSON by default, UTC RFC 3339 timestamps, level per target, rotated by size and count, with IPs and secrets redacted. | Log fixtures | MS-07 |
| REQ-SRV-020 | The Prometheus endpoint MUST expose every metric of §3.6 with its documented type and unit, on loopback by default, with bounded label cardinality. | Metrics contract test | MS-07 |
| REQ-SRV-021 | `/health` (liveness) and `/ready` (readiness) MUST return 200 or 503 with a small JSON body and no secrets, and the container healthcheck MUST use them. | Endpoint tests | MS-07 |
| REQ-SRV-022 | Tick phase timings, TPS over 1/5/15 minutes, per-player bandwidth, chunk counts, save latency, backup age and allocator statistics MUST be observable. | Dashboard contract | MS-07 |
| REQ-SRV-023 | The server MUST enforce player, connection, per-IP, bandwidth, entity, world-size, decompression and command-rate limits from configuration. | Limit-bypass tests | MS-15 |
| REQ-SRV-024 | The container image MUST run as a non-root user with a read-only root filesystem, no shell, dropped capabilities and a single writable `/data` volume, and MUST be ≤ 60 MB. | Image inspection test | MS-07 |
| REQ-SRV-025 | Secrets (tokens, keys) MUST live outside the world directory, be mode 0600, be rotatable without a restart where the class allows, and never be logged. | Secret-scan test | MS-07 |
| REQ-SRV-026 | An upgrade MUST take a pre-upgrade backup, refuse a world written by a newer format, run migrations on a verified copy, and keep the previous binary and backup for rollback. | Upgrade/downgrade drills | MS-07 |
| REQ-SRV-027 | The host MUST enforce the protocol support window at join and kick older clients with the required range ([18](18-networking-multiplayer.md) §3.4). | Version matrix | MS-06 |
| REQ-SRV-028 | Reference configurations for 10, 50 and 100 players and a capacity table MUST be published and regenerated from the load harness. | Docs review, benchmark artifact | MS-15 |
| REQ-SRV-029 | Each runbook in §3.11 MUST name symptoms, checks, actions and rollback, and MUST be exercised at least once before 1.0. | Drill records | MS-07 |
| REQ-SRV-030 | `diagnose` MUST produce a redacted support bundle with logs, config, metrics, thread inventory and world header, and MUST exclude world data, chat and identities by default. | Bundle inspection | MS-07 |
| REQ-SRV-031 | The server MUST NOT send telemetry or make outbound connections unless the operator configured ACME, OIDC, backup copy or listing explicitly. | Network capture test | MS-07 |
| REQ-SRV-032 | A second process opening the same world MUST refuse with the holding PID and boot ID; a stale lock MUST be recoverable with an explicit flag. | Two-process test | MS-07 |
| REQ-SRV-033 | Command names and configuration keys MUST be snake_case ASCII and documented from one table ([DEC-044](../decisions.md)). | Schema test, `help` output | MS-07 |
| REQ-SRV-034 | Save failure, metrics failure and disk exhaustion MUST degrade only their feature and MUST be visible in health and logs; the tick MUST keep running. | Fault injection | MS-07 |
| REQ-SRV-035 | All operator-facing timestamps MUST be UTC, durations integer seconds, and tick counts explicit about 20 Hz versus the snapshot rate. | Review, UI strings | MS-07 |
| REQ-SRV-036 | Every runbook action and every destructive command MUST be auditable and reversible or explicitly confirmed. | Audit and confirmation tests | MS-07 |

## 3. Design

### 3.1 Deployment models

| Model | Artifact | Pros | Costs | Supported |
| --- | --- | --- | --- | --- |
| Bare binary | `.tar.zst` per target | no runtime, easy debugging | operator writes a supervisor | yes |
| systemd | unit file | restart, journald, hardening directives | Linux only | yes |
| Container | OCI image, multi-arch | reproducible, read-only root, limits | UDP and volumes must be configured | yes |
| Compose | `compose.yaml` + `.env` | one command on a VPS | single host | yes (reference) |
| Orchestration | manifest-free | readiness, drain, probes | no operator in 1.0 | prepared, not shipped; proxy in MS-21 |

The container is a multi-stage build (`rust:1.100` → distroless/static or `scratch`), UID 10001,
`/data` volume, read-only root, `cap_drop: [ALL]`, `no-new-privileges`, UDP 25565 by default, and an
exec-form healthcheck `["/openblock-server", "health", "--url", "http://127.0.0.1:9091/health"]`. The
Wasmtime JIT is validated under the chosen libc before release; if it needs glibc, the image uses
distroless/cc rather than `scratch`. Building, signing, publishing and the image contract are owned by
[22](22-platforms-and-distribution.md) (REQ-PLAT-027); this chapter owns running and operating the image.
It contains no `wgpu`, `winit`, `cpal`, X11, Wayland, ALSA or PulseAudio (MS-07 exit criterion).

```yaml
services:
  openblock:
    image: ghcr.io/openblock/openblock-server:0.2
    command: ["serve", "--config", "/data/server.toml"]
    ports: ["25565:25565/udp"]
    volumes: ["./data:/data"]
    read_only: true
    user: "10001:10001"
    cap_drop: ["ALL"]
    security_opt: ["no-new-privileges:true"]
    stop_grace_period: 30s
    healthcheck:
      test: ["CMD", "/openblock-server", "health", "--url", "http://127.0.0.1:9091/health"]
      interval: 30s
      timeout: 5s
      retries: 3
    restart: unless-stopped
```

The systemd unit uses `Restart=on-failure`, `RestartSec=5`, `LimitNOFILE=65536`, `MemoryMax=8G`,
`TasksMax=4096`, `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`,
`ReadWritePaths=/var/lib/openblock`, `NoNewPrivileges=true`, `SuccessExitStatus=143`, and
`AmbientCapabilities=CAP_NET_BIND_SERVICE` only when binding below 1024. `SIGTERM` starts the drain in
§3.8; systemd's `TimeoutStopSec=40` covers the 30 s drain plus flush.

Reference sizing for the three published configurations; the capacity evidence is in §3.9.

| Setting | 10 players | 50 players | 100 players |
| --- | --- | --- | --- |
| vCPU / RAM | 2 / 2.5 GiB | 4 / 3.5 GiB | 8 / 5 GiB |
| `server.simulation_distance_blocks` | 256 | 256 | 192 |
| `server.view_distance_blocks` | 512 | 512 | 384 |
| `server.max_players` | 10 | 50 | 100 |
| `performance.max_entities` | 1,500 | 5,000 | 8,000 |
| `replication.entities_per_snapshot` | 128 | 256 | 256 |
| `replication.bytes_per_second_per_player` | 65,536 | 65,536 | 49,152 |
| `backups.interval_minutes` | 120 | 60 | 60 |
| Tick headroom target | 70% | 30% | 20% |

```toml
# the 50-player reference, layered over the defaults of §3.2
[server]
max_players = 50
view_distance_blocks = 512
simulation_distance_blocks = 256
[performance]
worker_threads = 2                 # max(1, 4 vCPU - 2); the host tick takes the fourth
max_loaded_chunks = 20_000
[replication]
entities_per_snapshot = 256
[backups]
interval_minutes = 60
```

### 3.2 Configuration surface

The table is normative for defaults, types and reload classes. `live` applies within 1 s, `session`
applies to new sessions, `next_session` applies when the world is next loaded, and `restart` needs a
process restart. `--print-config` emits a commented `server.toml` with these keys; `config.schema.json` is
generated from the same source and checked in. Environment variables use `OPENBLOCK_` with `__` for
nesting; CLI flags are generated from the schema, so the two cannot drift.

| Key | Type | Default | Reload | Constraint |
| --- | --- | --- | --- | --- |
| `server.name` | string | `"OpenBlock Server"` | live | 1–64 chars, shown in the join screen |
| `server.motd` | string | `"A self-hosted OpenBlock world"` | live | ≤ 128 chars, plain text |
| `server.bind` | IP/host | `"0.0.0.0"` | restart | IPv4 or IPv6 literal or resolvable name |
| `server.port` | u16 | `25565` | restart | UDP; clients append it to the ticket |
| `server.max_players` | u16 | `20` | next_session | 1–500 |
| `server.snapshot_rate_hz` | u8 | `20` | next_session | `20` or `30` only; simulation stays 20 Hz |
| `server.view_distance_blocks` | u16 | `512` | next_session | 64–1024, multiple of 16 |
| `server.simulation_distance_blocks` | u16 | `256` | next_session | 64–`view_distance_blocks` |
| `server.online_mode` | enum | `"key"` | restart | `key`, `oidc`, `open` (LAN/dev only) |
| `server.require_content_match` | enum | `"strict"` | restart | `strict` (`pack_hash` equality) or `content_only` (view differences advisory) |
| `server.min_client_protocol_minor` | u16 | `0` | restart | ≤ the build's protocol minor |
| `server.allow_client_view_beyond_server` | bool | `true` | live | pop-in warning when true |
| `identity.mode` | enum | `"self_signed"` | restart | `self_signed`, `acme`, `provided` |
| `identity.key_path` | path | `"identity/server.key"` | restart | mode 0600, outside the world |
| `identity.cert_path` | path | `"identity/server.crt"` | restart | required for `provided` |
| `identity.acme_domain` | string | `""` | restart | required for `acme`; UDP 443 reachable |
| `identity.acme_contact` | string | `""` | restart | CA contact address |
| `identity.oidc_issuer` | url | `""` | restart | required for `online_mode = "oidc"` |
| `identity.oidc_client_id` | string | `""` | restart | device-code client |
| `identity.allow_local_keys` | bool | `true` | restart | local keys accepted beside OIDC |
| `network.mtu_initial` | u16 | `1200` | restart | 1200–1450 |
| `network.mtu_max` | u16 | `1400` | restart | ≥ `mtu_initial`, ≤ 1450 |
| `network.idle_timeout_seconds` | u16 | `30` | live | 10–300 |
| `network.keepalive_seconds` | u16 | `10` | live | < idle timeout |
| `network.enable_retry` | bool | `true` | live | stateless address validation |
| `network.max_connections` | u32 | `256` | live | endpoint-wide |
| `network.max_connections_per_ip` | u16 | `4` | live | 1–64 |
| `network.handshake_rate_per_second` | u16 | `20` | live | endpoint-wide token bucket; with Retry enabled, handshakes above it get a challenge ([18](18-networking-multiplayer.md) §3.3) |
| `network.max_queued_outbound_kib` | u32 | `4096` | live | per peer; background shed first |
| `network.allow_migration` | bool | `true` | live | client-initiated path migration |
| `network.enable_0rtt` | bool | `true` | restart | resume request only ([DEC-026](../decisions.md)) |
| `world.root` | path | `"saves"` | restart | relative to the data directory |
| `world.name` | string | `"main"` | restart | resolved against `level.json` in every directory under `world.root`; the directory itself is `saves/<uuid>/` ([17](17-persistence.md) §3.1) |
| `world.seed` | string | `""` | restart | decimal string; empty means random, logged |
| `world.generator` | resource ID | `"openblock:noise/1"` | restart | fingerprint recorded in `level.json` |
| `world.gamemode` | enum | `"survival"` | session | `survival`, `creative`, `adventure`, `spectator` |
| `world.difficulty` | enum | `"normal"` | session | `peaceful`, `easy`, `normal`, `hard`, `hardcore` (locks death behaviour) |
| `world.pvp` | bool | `true` | session | host-enforced |
| `world.allow_flight` | bool | `false` | session | anti-cheat exception, logged |
| `world.force_gamemode` | bool | `false` | session | re-apply on every join |
| `world.spawn_protection_blocks` | u16 | `16` | session | 0–256 |
| `world.world_border_blocks` | u32 | `29999984` | session | radius in blocks, matched to `level.json.world_border` |
| `world.autosave_interval_seconds` | u16 | `60` | live | 10–3600; same value as [17](17-persistence.md) REQ-SAVE-033 |
| `world.game_rules.*` | typed map | per [08](08-gameplay-survival.md) | session | unknown rule is an error |
| `performance.worker_threads` | u8 | `0` | restart | 0 = `max(1, cpus − 2)` |
| `performance.region_tick_workers` | u8 | `0` | restart | 0 = auto |
| `performance.max_loaded_chunks` | u32 | `20000` | live | generation and load backpressure |
| `performance.entity_activation_range_blocks` | u16 | `128` | next_session | AI and tick range |
| `performance.max_entities` | u32 | `5000` | next_session | hard cap; spawning stops with a metric |
| `performance.tick_soft_budget_ms` | u16 | `40` | live | logs an overrun above this |
| `performance.tick_hard_budget_ms` | u16 | `50` | live | watchdog threshold; never silently exceeded |
| `performance.save_flush_budget_ms` | u16 | `2` | live | per-tick save slice |
| `performance.chunk_send_budget_kib_per_tick` | u16 | `24` | live | per player ([18](18-networking-multiplayer.md) §3.7) |
| `replication.bytes_per_second_per_player` | u32 | `65536` | live | steady cap; 131,072 when streaming a 512-block interest set (PERF-NET-001) |
| `replication.join_burst_bytes_per_second` | u32 | `2097152` | live | join cap |
| `replication.entities_per_snapshot` | u16 | `256` | live | per tick packet |
| `replication.max_tracked_entities` | u16 | `512` | live | per client |
| `replication.precompress_chunks` | bool | `true` | restart | wire cache, on by default |
| `replication.full_state_refresh_seconds` | u8 | `2` | live | 1–10 |
| `content.pack_paths` | list | `["packs"]` | restart | ordered, base first |
| `content.required` | list | `["openblock:base"]` | restart | servers refuse to start without them |
| `content.allow_server_pack_delivery` | bool | `true` | restart | never delivers code |
| `content.max_pack_bytes` | u64 | `268435456` | restart | per pack |
| `content.http.enabled` | bool | `false` | restart | optional pack and `.obworld` range endpoint |
| `content.http.bind` | IP:port | `"127.0.0.1:8080"` | restart | loopback by default; put a TLS proxy in front for the internet |
| `mods.enabled` | list | `[]` | next_session | by ID and hash |
| `mods.allowlist` | list | `[]` | next_session | BLAKE3 hashes; empty means deny all |
| `mods.default_capabilities` | list | `[]` | restart | deny by default; the operator approves capabilities per mod ([16](16-modding.md) §3.3, [DEC-020](../decisions.md)) |
| `mods.wasm_memory_mib` | u16 | `64` | restart | guest memory per instance ([16](16-modding.md) §4) |
| `mods.wasm_fuel_per_callback` | u64 | `25000000` | live | deterministic mode only |
| `mods.wasm_wall_ms_per_callback` | u16 | `1` | live | epoch deadline in guarded mode |
| `permissions.op_permission_level` | u8 | `3` | live | 0–4, see [08](08-gameplay-survival.md) §3.12 |
| `permissions.function_permission_level` | u8 | `2` | live | command blocks and functions |
| `permissions.enable_command_block` | bool | `true` | session | per world |
| `logging.level` | enum | `"info"` | live | `error`…`trace` |
| `logging.format` | enum | `"json"` | live | `json`, `pretty` |
| `logging.rotation_size_mib` | u16 | `50` | live | 1–1024 |
| `logging.rotation_keep` | u8 | `5` | live | 1–100 |
| `logging.log_player_ips` | bool | `false` | live | off by default |
| `logging.log_chat` | bool | `true` | live | content policy belongs to the operator |
| `metrics.enabled` | bool | `true` | live | Prometheus text format |
| `metrics.bind` | IP:port | `"127.0.0.1:9090"` | restart | never public by default |
| `metrics.path` | string | `"/metrics"` | restart | must start with `/` |
| `metrics.per_player_series` | enum | `"top"` | live | `off`, `top`, `all` |
| `metrics.top_players` | u8 | `20` | live | 1–100 when `top` |
| `health.bind` | IP:port | `"127.0.0.1:9091"` | restart | liveness and readiness |
| `console.enabled` | bool | `true` | restart | stdin when attached |
| `console.socket` | path | `"console.sock"` | restart | mode 0600, data directory |
| `console.remote.enabled` | bool | `false` | restart | TLS 1.3 + token |
| `console.remote.bind` | IP:port | `"127.0.0.1:25576"` | restart | loopback by default |
| `console.remote.token_file` | path | `"secrets/console.token"` | live | 0600, ≥ 256-bit random, re-read on `reload` |
| `console.remote.rcon_compatible` | bool | `false` | restart | plaintext, loopback only, never remotely bound |
| `console.remote.rcon_port` | u16 | `25575` | restart | used only with the line above |
| `backups.enabled` | bool | `true` | live | scheduled backups |
| `backups.interval_minutes` | u16 | `60` | live | 15–1440 |
| `backups.destination` | path | `""` | live | empty = `<world>/backups/` ([17](17-persistence.md) §3.8) |
| `backups.keep_hourly` | u16 | `24` | live | count |
| `backups.keep_daily` | u16 | `7` | live | count |
| `backups.keep_weekly` | u16 | `4` | live | count |
| `backups.pre_migration` | bool | `true` | restart | cannot be disabled without `--i-know` |
| `backups.verify_after` | bool | `true` | live | re-read and hash every region |
| `backups.external_copy` | path | `""` | live | `rsync`-style target for disaster recovery |
| `limits.max_chunk_bytes` | u32 | `262144` | restart | decode cap before allocation |
| `limits.max_packet_bytes` | u32 | `65536` | restart | control message cap |
| `limits.max_world_bytes` | u64 | `1099511627776` | live | 1 TiB; writes refused above |
| `limits.max_chat_messages_per_10_seconds` | u8 | `5` | live | per player |
| `limits.max_commands_per_second` | u8 | `20` | live | per player |
| `limits.max_block_edits_per_command` | u32 | `32768` | live | per command |
| `limits.max_decompressed_bytes` | u64 | `536870912` | restart | zip-bomb guard |
| `shutdown.drain_seconds` | u16 | `30` | restart | new joins refused, play continues |
| `shutdown.save_deadline_seconds` | u16 | `5` | restart | flush deadline before exit |

Hyphenated names used in earlier chapters (`force-gamemode`, `op-permission-level`,
`spawn-protection`, `enable-command-block`, `rcon`) are the keys above in canonical snake_case
([DEC-044](../decisions.md)); the schema is the authority. `tick_rate` is deliberately not a key:
the simulation is 20 Hz and `snapshot_rate_hz` is the only rate an operator may raise.

Capability names in `[mods]` are the canonical dotted set of [16 §3.3](16-modding.md#33-capabilities):
`world.read`, `world.write`, `entity.spawn`, `ui`, `network`, `storage`, `server.admin`. Legacy
spellings such as `world_read` are accepted from older configs and canonicalised on load, with the
dotted form in logs and audit ([16 §3.10](16-modding.md#310-operator-controls)). `default_capabilities`
is empty by default because [DEC-020](../decisions.md) requires an operator decision per capability,
never an assumed grant.

Gameplay keys in `[world]` are defaults. Once a world exists, `level.json` is authoritative for its
gamemode, difficulty, rules and border ([17](17-persistence.md) §3.4); `world.force_gamemode` and the
`gamerule` command change the world, and `--force-world-settings` re-applies the config at load.

### 3.3 World management

| Command | Arguments | Effect | Notes |
| --- | --- | --- | --- |
| `world list` | — | UUID, name, size, last played, generator fingerprint | reads `level.json` in each world directory |
| `world create` | `--name`, `--seed`, `--generator`, `--gamemode`, `--difficulty` | creates `saves/<uuid>/` and `level.json` | refuses an existing name |
| `world load` | `--name` or `--uuid` | unloads the active world, loads the other | players are disconnected with `world_mismatch` and rejoin |
| `world delete` | `--name`, `--confirm` | backs up, then moves the directory to `<data>/trash/<uuid>/` | no immediate unlink |
| `world border` (`set`, `get`) | `--radius`, `--center` | updates `level.json` and all clients | clamped to ±29,999,984 |
| `pregen` | `--world`, `--radius`, `--threads`, `--resume`, `--verify`, `--dry-run` | generates columns ahead of play | progress every 5 s, resumable; the spelling in [06](06-worldgen.md) §3.11 |
| `world regen_chunks` | `--chunks`, `--dry-run`, `--force` | regenerates generated-only chunks | the `--regen-chunks` action of [DEC-047](../decisions.md); prints affected columns and builds first |
| `world backup` | `--label` | flush, link/copy, verify, record | never blocks the tick |
| `world restore` | `--backup`, `--verify-only` | restores into a new directory and swaps | requires a stopped world |
| `world verify` | `--repair` | checks headers, indexes, CRCs, content table | reports quarantine candidates |
| `world export` | `--output`, `--include-players` | writes an `.obworld` archive | excludes backups and cache |
| `world import` | `--input` | validates, extracts, verifies, renames | never writes partially into `saves/` |
| `world migrate` | `--dry-run`, `--verify` | runs the migration chain on a copy | refuses a newer format |

Pregeneration writes through the same world store as live generation, so a pregenerated column and a
live-generated column are byte-identical: same seed, generator fingerprint and content hash
([DEC-046](../decisions.md)). `--resume` reads the world's chunk index instead of restarting.
Regeneration is the only path that can replace generated terrain, it requires `--force`, and it
refuses to touch any column with player edits unless the report is acknowledged.

### 3.4 Player administration

| Command | Level | Effect |
| --- | --- | --- |
| `list` | 0 | names, keys (short form), ping, state |
| `op` / `deop` | 3 | grant or revoke level 0–3; only level 4 may mint level 4; stored by public key |
| `whitelist` (`add`, `remove`, `list`, `on`, `off`) | 3 | key-based allowlist; names are display only |
| `ban` / `pardon` | 3 | key-based ban with reason, optional expiry and IP/subnet evidence |
| `mute` / `unmute` | 3 | chat-only restriction with duration |
| `kick` | 3 | disconnect with a reason ([18](18-networking-multiplayer.md) §3.4) |
| `player info` | 3 | key, first seen, last seen, play time, addresses (only if IP logging is on) |
| `tp`, `gamemode`, `clear`, `give` | 2–3 | gameplay administration, audited |
| `say`, `tell` | 1–3 | broadcast or direct message |

Bans and whitelists are stored in `admin/keys.json` (mode 0600) keyed by the full public key, with
display name, reason, creation time, expiry and evidence. A rename or a new client never evades a
ban, and an IP is never the sole key. Permission levels are those of
[08](08-gameplay-survival.md) §3.12; console and RPC act at level 4. Every mutation appends an audit
record:

```json
{"ts":"2026-11-24T15:31:27Z","seq":1042,"actor":{"kind":"console","id":"local"},
 "action":"ban","target":{"kind":"player","key":"ed25519:9f2c…","name":"Ada"},
 "args":{"reason":"griefing","expires":"2026-12-24T00:00:00Z","evidence":"ip:198.51.100.0/24"},
 "result":"ok","prev_hash":"blake3:…","hash":"blake3:…"}
```

The hash chain covers canonical JSON of the record without `hash`, combined with `prev_hash`; a gap
or a mismatch is reported by `audit verify`. Rotation keeps 90 days by default and never truncates
the chain without recording a signed checkpoint.

### 3.5 Console and RPC

| Surface | Transport | Default | Auth | Audience |
| --- | --- | --- | --- | --- |
| local console | stdin/stdout, TTY | on | process user | interactive operator |
| local socket | Unix socket, NDJSON | on | mode 0600, same UID | scripts, `openblock ctl` |
| remote console | TLS 1.3 TCP, JSON-RPC 2.0 | off | bearer token file | remote operator |
| RCON-compatible | plaintext TCP | off, loopback only | same token file | existing tooling |

`openblock ctl` speaks the local socket or the TLS endpoint with the same JSON-RPC envelope, so a
script written against one works against the other. Every method maps to a console command or a
read-only query; there is no eval, no plugin loading and no arbitrary filesystem access. Mutating
methods are rate-limited to 20/s, audited, and return the command's structured result. The root token
is 256 bits from the OS CSPRNG, compared in constant time, stored in a `0600` file outside the world
and rotatable by replacing the file and running `reload`; the RPC layer may derive session tokens
scoped `read`, `moderate` or `admin` with an expiry of at most 12 h, and audit entries record the token
prefix, scope and a salted source-address hash, never the token
([20](20-security-and-anti-cheat.md) REQ-SEC-020).

The RCON-compatible listener is the single exception to "TLS 1.3 for remote admin": the RCON protocol
is plaintext, so it MUST stay off unless enabled, MUST bind a loopback address only, and is normally
reached through an SSH tunnel. Every other console or RPC surface requires TLS 1.3
([20 §3.9.5](20-security-and-anti-cheat.md#395-admin-tokens)).

| Command | Level | Notes |
| --- | --- | --- |
| `help [command]` | 0 | generated from the command table |
| `status` | 0 | version, uptime, world, players, tick, save age |
| `list` | 0 | players with state and ping |
| `tps [window]` | 0 | 1/5/15-minute TPS and p50/p95/p99 tick |
| `tick` | 3 | last 100 tick phases, worst systems |
| `metrics` | 3 | metric snapshot in text or JSON |
| `save` / `save_off` / `save_on` | 3–4 | force a flush, pause or resume autosave |
| `backup [label]` | 3 | flush, snapshot, verify, rotate |
| `reload` | 4 | re-read config, apply `live` keys |
| `mod` (`list`, `enable`, `disable`) | 4 | changes take effect after the next world load |
| `world …` | 3–4 | §3.3 |
| `op`, `deop`, `whitelist`, `ban`, `pardon`, `mute`, `unmute`, `kick` | 3 | §3.4 |
| `say`, `tell` | 1–3 | chat and direct message |
| `diagnose` | 4 | write a redacted support bundle |
| `audit` (`tail`, `verify`) | 4 | read or verify the audit chain |
| `stop [--drain N]` | 4 | graceful shutdown |

### 3.6 Observability

Logs are JSON lines on stdout and in `<data>/logs/server.jsonl`, rotated at
`logging.rotation_size_mib` × `logging.rotation_keep`, with fields `ts`, `level`, `target`, `span`,
`tick`, `player` (key hash, not name, where privacy matters), `msg` and typed fields. IP addresses
are omitted unless `log_player_ips` is true; tokens, keys and chat content are never logged by
default. `RUST_LOG`-style per-target filters are allowed but the resolved filter is logged at start.

Metrics are exposed in Prometheus text format. Naming follows
`openblock_<area>_<name>_<unit>` ([01](01-technical-architecture.md) §4.4); a rename breaks dashboards
and is a breaking change.

| Metric | Type | Unit / labels | Meaning |
| --- | --- | --- | --- |
| `openblock_build_info` | gauge | `version`, `protocol`, `commit` | always 1; build identity |
| `openblock_process_start_time_seconds` | gauge | seconds | Unix start time |
| `openblock_process_uptime_seconds` | gauge | seconds | since start |
| `openblock_players` | gauge | `state` | connecting, joining, playing, spectator |
| `openblock_player_ping_seconds` | histogram | `player` (top N) | RTT estimate |
| `openblock_player_bandwidth_bytes_per_second` | gauge | `direction`, `player` | per-player rate |
| `openblock_tick_duration_seconds` | histogram | seconds | full tick |
| `openblock_tick_phase_duration_seconds` | histogram | `phase` | input, pretick, tick, posttick, replication, save |
| `openblock_tick_overruns_total` | counter | — | ticks above the soft budget |
| `openblock_tps` | gauge | `window` | 1/5/15-minute TPS |
| `openblock_net_bytes_total` | counter | `direction` | bytes sent and received |
| `openblock_net_datagrams_lost_total` | counter | — | estimated loss |
| `openblock_net_decode_errors_total` | counter | `kind` | malformed datagram, stream or control message |
| `openblock_replication_queue_bytes` | gauge | `lane` | safety, near, prefetch, background |
| `openblock_chunks_loaded` | gauge | `status` | per chunk lifecycle state |
| `openblock_chunk_generation_seconds` | histogram | seconds | terrain + features |
| `openblock_entities` | gauge | — | simulated entities |
| `openblock_save_duration_seconds` | histogram | seconds | commit cycle |
| `openblock_save_queue_bytes` | gauge | bytes | pending dirty data |
| `openblock_save_failures_total` | counter | `reason` | ENOSPC, permission, checksum |
| `openblock_backup_last_success_timestamp_seconds` | gauge | seconds | Unix time of last verified backup |
| `openblock_backup_duration_seconds` | histogram | seconds | snapshot + verify |
| `openblock_memory_bytes` | gauge | `category` | world, mesh, content, net, mod, other |
| `openblock_allocator_bytes` | gauge | `kind` | live and reserved bytes |
| `openblock_allocator_fragmentation_ratio` | gauge | ratio | reserved ÷ live − 1 |
| `openblock_mod_fuel_exhausted_total` | counter | `mod` | quota hits |
| `openblock_mod_trap_total` | counter | `mod` | traps and panics |
| `openblock_auth_failures_total` | counter | `reason` | failed joins |
| `openblock_connections_total` | counter | `result` | accepted, rejected, dropped |
| `openblock_handshakes_rejected_total` | counter | `reason` | retry, rate, limit |
| `openblock_world_size_bytes` | gauge | bytes | excluding backups and cache |
| `openblock_console_requests_total` | counter | `transport`, `result` | console and RPC |
| `openblock_shutdown_duration_seconds` | histogram | seconds | drain + flush |

Label cardinality is bounded: never label by IP, never by world (one world per session), per-player
series only for the top N by bandwidth unless `per_player_series = "all"`. `/health` is liveness and
`/ready` is readiness:

```json
{"status":"ok","version":"0.2.0","protocol":1,"uptime_seconds":86400,
 "world":{"name":"main","uuid":"018f…","loaded_chunks":2048,"save_age_seconds":3},
 "tick":{"rate_hz":20,"snapshot_hz":20,"tps_1m":20.0,"p99_ms":12.4,"overruns_total":0},
 "players":{"online":12,"max":20},
 "backup":{"last_success_unix":1790000000,"age_seconds":1800}}
```

`/health` returns 503 only while draining or when no tick has completed for 5 s; `/ready` returns 503
until the world is loaded and the first save cycle has completed, and while new joins are refused.
The body never contains keys, paths or player names.

Recommended dashboards (JSON in `ops/grafana/`) and alerts:

| Panel / alert | Query sketch | Threshold |
| --- | --- | --- |
| TPS and tick p99 | `openblock_tps{window="1m"}`, histogram quantile | alert if TPS < 19 for 5 min or p99 > 40 ms for 10 min |
| Tick phase breakdown | `sum by (phase)` of phase duration | visual; identify the dominant phase |
| Players and join failures | `openblock_players`, `openblock_connections_total` | alert on sustained join failure ratio > 20% |
| Bandwidth per player | top-N player rate | alert on peers above cap for 5 min |
| Chunk pipeline | loaded chunks, generation latency, replication queue | alert on queue > 64 MiB for 10 min |
| Save health | duration, queue, failures, backup age | alert on any failure or backup age > 2× interval |
| Memory | category bytes, allocator live/reserved | alert above 85% of the container limit |
| Mod and auth health | fuel exhausted, traps, auth failures | alert on any trap or a rising auth-failure rate |

### 3.7 Backups, retention, RPO and disaster recovery

| Data | RPO | RTO | Mechanism |
| --- | --- | --- | --- |
| Player state | ≤ 60 s | restart | autosave every 60 s plus logout, death and transfer |
| World blocks | crash: last commit; disaster: `backups.interval_minutes` | 5 min in place, 30 min on a new host | region commits, then link/copy ([17](17-persistence.md) §3.5, §3.8) |
| Config, identity, keys | last change | 10 min | operator copy; never in a world archive |
| Audit log | ≤ 5 s | 1 min | append + rotation |

Retention defaults are 24 hourly, 7 daily and 4 weekly, pruned oldest first; the pre-migration backup
is never pruned while its version is running. A backup is: flush the world store and fsync, pause
compaction, hard-link or copy `level.json`, `ids.json`, `players/`, every region file and every structure sidecar,
verify a BLAKE3 manifest, then atomically rename the backup directory ([17](17-persistence.md) §3.8).
Hard links are only valid because region files are immutable once committed; the server detects a
filesystem without hard links and falls back to a copy, showing the space cost. `external_copy`
additionally writes to an off-host target; encryption and access control at that target are the
operator's responsibility and are documented as such.

Disaster recovery: provision a host, install the same or a newer build, restore the latest verified
backup into a new directory, run `world verify`, start with joins disabled, confirm the spawn area
and a sample of chunks, then enable joins. A quarterly drill runs the same procedure on a fresh
container and records the elapsed time in `ops/drills/`.

### 3.8 Upgrade and downgrade

1. Read the release notes, the world's `format_version`/`data_version` and the new build's supported maxima ([17](17-persistence.md) §3.7).
2. Run `world backup --label pre-upgrade`; confirm `openblock_backup_last_success_timestamp_seconds`
   advanced and the backup verified.
3. Run `stop` (exit 0) or `docker compose stop`/`systemctl stop` (SIGTERM, exit 143 accepted by
   `SuccessExitStatus`) and wait for the drain deadline.
4. Replace the binary or image tag, keeping the previous version installed or tagged.
5. Start the new build: migrations run on a verified copy with a pre-migration backup, and the world
   is swapped atomically afterwards ([17](17-persistence.md) §3.7). Watch logs for migration steps.
6. Join with a bot and a client, check `/ready`, TPS, tick p99, join time and one block edit.
7. Announce, and keep the previous binary and the pre-upgrade backup for at least 7 days.

A downgrade is refused when the world's `format_version` or `data_version` is newer than the binary: the operator restores
the pre-upgrade backup and the previous binary. There is no rolling upgrade in 1.0; the drain window
is the downtime (30 s default, plus client reconnect), and a proxy-based zero-downtime path belongs
to MS-21. Container upgrades follow the same order with `docker compose pull` and
`docker compose up -d`; the old container receives `SIGTERM` and has 30 s to drain.

### 3.9 Capacity planning

The table is the published sizing matrix. Values marked *target* are the MS-15 measurements the
design must meet, taken on the reference 4 vCPU / 8 GiB host; rows above 4 vCPU are the same workload
extrapolated and are labelled as such in `benchmarks/ms-15-capacity.md`, which replaces this table at
release and reopens the budget on regression.

| Players | Sim distance | vCPU | RAM | Tick p99 | Down per player | Join time | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 10 | 256 | 2 | 2.5 GiB | 8 ms (target) | 15 KB/s | 5 s | small VPS, comfortable |
| 25 | 256 | 4 | 3 GiB | 16 ms (target) | 20 KB/s | 8 s | recommended entry tier |
| 50 | 256 | 4 | 3.5 GiB | 30–35 ms (target) | 22 KB/s | 10 s | reference tier ([vision](../vision.md) §8) |
| 50 | 512 | 8 | 4 GiB | 45 ms (target) | 40 KB/s | 40 s | tight; only with entity caps |
| 100 | 192 | 8 | 4.5 GiB | 40 ms (target) | 18 KB/s | 12 s | entity-light worlds only |
| 100 | 256 | 8 | 5 GiB | 45 ms (target) | 25 KB/s | 15 s | not recommended at 1.0 |

The RAM column is the container limit; server RSS stays within `2 GiB + 20 MiB × players`
(3 GiB at 50, 4 GiB at 100), and the remainder covers the OS, page cache and allocator
fragmentation.

The tick budget at 20 Hz is 50 ms. For 50 players at 256 blocks the allocation is 15 ms simulation,
8 ms replication and interest, 6 ms chunk cache and send, 4 ms saves and 17 ms headroom; the p99
target is 35 ms ([vision](../vision.md) §8). Cost drivers in order: loaded ticking chunks, entities
with AI, per-player interest queries, chunk encoding during join and flight, then saves. Transport
crypto is not a driver at these player counts. A server is CPU-bound before it is bandwidth-bound,
and memory is `2 GiB + 20 MiB × players` ([01](01-technical-architecture.md) §5).

The harness is a swarm of headless bot clients that join, walk a fixed route, chat, edit blocks and
open a chest, with a join-storm mode. It records tick p50/p95/p99, per-phase time, RSS slope over
24 h, join bytes and per-player bandwidth. A configuration is publishable only with a 24-hour run and
an RSS slope near zero; a 500-bot join storm is the acceptance test for a public listing.

### 3.10 Security hardening

| Control | Default | Verification |
| --- | --- | --- |
| Firewall exposes UDP 25565 only | documented | port scan in the container test |
| Metrics, health and console bound to loopback | yes | socket test |
| Non-root UID, read-only root, no shell | yes | image inspection |
| `cap_drop: ALL`, `no-new-privileges` | yes | compose and systemd lint |
| CPU, memory, PID and file-descriptor limits | yes | cgroup test |
| TLS 1.3 with pinned SPKI or ACME | yes | handshake test |
| Token files mode 0600 outside the world | yes | secret-scan test |
| Handshake and connection rate limits | yes | load test |
| Whitelist and key-based bans available | yes | admin test |
| Backup destination off-host and encrypted | recommended | drill checklist |
| Logs redact IPs, keys and chat | yes | fixture test |
| Dependency audit and signed images | release gate | CI `cargo-deny`, cosign |
| DDoS scrubbing for public servers | provider guidance | runbook review |
| No outbound traffic unless configured | yes | capture test |
| World directory mode 0750, backups 0700 | yes | installer test |
| Update policy and security contact | `SECURITY.md` | review |

Public servers SHOULD sit behind a provider with UDP DDoS scrubbing and SHOULD enable QUIC Retry and
per-IP handshake limits. The console and RCON surfaces MUST NOT be exposed to the internet directly:
use the TLS console, and reach RCON only through an SSH tunnel, because the RCON protocol is
plaintext. Treat a leaked RCON token as full server compromise.

### 3.11 Operational runbooks

| Scenario | Symptoms | First checks | Action | Rollback |
| --- | --- | --- | --- | --- |
| Server will not start | exit 2/65/70, no join ticket | config validation error, `world.lock` holder, port in use, newer world format | fix the key or path; stop the other process; use the correct binary | restore the previous binary or backup |
| TPS drop | TPS < 19, p99 > 40 ms | tick phase metrics, loaded chunks, entity count, redstone, mod fuel, disk latency | lower sim distance or entity cap, disable the offending mod, pregen, move to SSD | lower settings; restore config |
| Save corruption | CRC failure, `world verify` errors, quarantine entries | last backup age, disk errors, `journal.obm` | stop writes, verify, restore the last good backup, preserve the corrupt world | restore previous backup; previous binary |
| Player stuck | player cannot move or joins into void | position, dimension, chunk status, world border | `tp` to a safe spawn, relog, check tickets | undo teleport; restore player data |
| Mod misbehaving | traps, fuel exhausted, tick attribution | `mod list`, trap logs, per-mod metrics | disable the mod, reload, reload in dev | re-enable the previous version |
| Disk full | save failures, writes refused, health degraded | `df`, world size, backup count, core dumps | free space, prune old backups, grow the volume, resume saves | restore from off-host backup |
| Certificate rotation | all clients warn, join fails | fingerprint change, ACME renewal, expiry | re-pin in the ticket and client, publish the new fingerprint | restore the old key (breaks pinning) |
| Rollback needed | bad release, corruption, regression | pre-upgrade backup, previous binary, protocol window | stop, restore backup, start the previous build | forward-fix and redeploy |

Each runbook names its first check so an on-call operator does not start with the most expensive
action. `diagnose` is safe to run at any time and is the standard attachment for support: logs,
redacted config, metrics, thread inventory and the world header, never world data, chat or keys.

### 3.12 Support and monitoring policy

| Severity | Meaning | First response | Target resolution |
| --- | --- | --- | --- |
| S1 | data loss, remote crash, security breach | 1 h | workaround 24 h, fix in the next patch |
| S2 | cannot start, cannot join, severe lag | 4 h | fix or workaround in 72 h |
| S3 | feature broken, workaround exists | 2 business days | next minor release |
| S4 | question, documentation, cosmetic | 5 business days | docs or backlog |

The support window follows the protocol window decided in
[18 §8](18-networking-multiplayer.md#8-open-questions); until that decision, the current and previous
minors receive security fixes, older servers get a migration path but no backports. Monitoring policy: alert on S1 and S2 conditions from §3.6
dashboards, review capacity monthly, and record every drill in `ops/drills/`. No telemetry is
collected; the operator owns the data and the support bundle is only produced on request. The
release cadence and the end-of-life policy live in
[plan/06](../plan/06-release-and-operations.md).

## 4. Data structures and formats

```rust
pub struct ServerConfig {
    pub server: ServerSection,
    pub identity: IdentitySection,
    pub network: NetworkSection,
    pub world: WorldSection,
    pub performance: PerformanceSection,
    pub replication: ReplicationSection,
    pub content: ContentSection,
    pub mods: ModsSection,
    pub permissions: PermissionsSection,
    pub logging: LoggingSection,
    pub metrics: MetricsSection,
    pub health: HealthSection,
    pub console: ConsoleSection,
    pub backups: BackupSection,
    pub limits: LimitsSection,
    pub shutdown: ShutdownSection,
}
```

Each section derives `Deserialize` with `deny_unknown_fields`, carries `#[schemars(range)]`
attributes, and declares its reload class in a table generated into `config.schema.json`. The resolved
config is immutable per generation; a reload builds a new generation, validates it, then swaps an
`Arc<ResolvedConfig>`, so a reader never sees a half-applied change.

```rust
pub struct HealthReport {
    pub status: HealthStatus,           // Ok, Degraded, Draining
    pub version: &'static str,
    pub protocol: u16,
    pub uptime_seconds: u64,
    pub world: WorldHealth,
    pub tick: TickHealth,
    pub players: PlayerHealth,
    pub backup: BackupHealth,
}

pub struct AuditRecord {
    pub ts: String,                     // RFC 3339 UTC
    pub seq: u64,
    pub actor: Actor,                   // console, socket, remote, system
    pub action: &'static str,
    pub target: Option<Target>,
    pub args: serde_json::Value,
    pub result: AuditResult,
    pub prev_hash: Blake3Hash,
    pub hash: Blake3Hash,
}
```

Health responses are built from the same counters as the metrics, so an alert and a health check
cannot disagree. Audit records are canonical JSON (sorted keys, no insignificant whitespace), hashed
with BLAKE3 over the record without `hash` plus `prev_hash`; `audit verify` walks the chain and stops
at the first mismatch or gap.

## 5. Performance budgets

The cross-cutting server budgets — start-up, drain, tick p99, RSS and load — are canonical in
[25 §5.3](25-performance-budgets.md) (`PERF-SRV-001…004`). This section adds the operational budgets
that table does not state.

| ID | Budget | Measurement |
| --- | --- | --- |
| PERF-SRV-010 | Config load, validate and schema check ≤ 100 ms | start-up benchmark |
| PERF-SRV-011 | `/metrics` scrape ≤ 20 ms and ≤ 1 MiB | endpoint benchmark |
| PERF-SRV-012 | `/health` and `/ready` ≤ 10 ms p99 | endpoint benchmark |
| PERF-SRV-013 | Console or RPC command latency ≤ 50 ms p99 on local surfaces | console benchmark |
| PERF-SRV-014 | Backup of a 2 GiB world ≤ 60 s with hard links; verification ≤ 2 min | backup drill |
| PERF-SRV-015 | Restore ≤ 5 min in place for 2 GiB; ≤ 30 min on a new host | quarterly drill |
| PERF-SRV-016 | Audit append ≤ 0.5 ms p99 and never on the tick thread | audit benchmark |
| PERF-SRV-017 | 50 players at 256 blocks: tick p99 ≤ 35 ms and RSS ≤ 3 GiB on 4 vCPU (PERF-SRV-002/003) | load harness |

## 6. Failure modes and edge cases

| Failure | Detection | Symptom | Containment | Recovery |
| --- | --- | --- | --- | --- |
| Bad configuration | start-up validation, exit 2 | process exits with a dotted path and suggestion | last good file untouched | fix the key |
| World locked by another process | `world.lock` with PID and boot ID | exit 65 naming the holder | second process refuses | stop the other process or clear a stale lock |
| Save failure (ENOSPC, permission) | write error, metric, log | "world not saved" banner, health degraded | previous save intact, retries with backoff | free space or fix permissions |
| Corrupt chunk | CRC or index failure, quarantine entry | `world verify` reports it; join may stall | payload quarantined, never silently regenerated | restore or repair |
| Disk full | `df`, save failures | writes refused, health degraded | read-only mode, tick continues | prune backups, grow volume |
| Backup failure | missing artifact, verification error | alert and metric stay stale | next attempt; never marks a partial backup good | fix destination, rerun |
| Mod trap or quota | wasmtime trap, fuel counter | "mod disabled" in console, tick unaffected | uncommitted batch discarded, mod disabled | fix or disable, reload |
| Metrics or console down | bind error | endpoint unavailable, game unaffected | feature degraded only | fix port or permissions |
| DDoS or handshake flood | connection and handshake metrics | new joins time out, existing play continues | Retry, limits, provider scrubbing | mitigate upstream, rotate endpoint |
| Migration failure | migration log, version check | world does not open | migration runs on a copy; original untouched | restore pre-migration backup |
| Certificate expiry | TLS handshake failures, alert | clients cannot connect | reload certificate where class allows | renew, restart, re-pin if changed |
| Memory limit reached | cgroup OOM, allocator metrics | process killed and restarted | limits, backpressure, mod quotas | lower settings, raise limit |
| Protocol version mismatch | join rejection | kick naming the supported range | client update | update client or server |

## 7. Testing strategy

The tiering and harness are owned by [24](24-testing-benchmarks.md); this table lists the
server-operations cases.

| Test | What it proves | Where |
| --- | --- | --- |
| Compose smoke | `docker compose up -d` on an empty directory creates a world, prints a ticket, a bot joins, `stop` exits 0 | CI, nightly |
| Config schema | every key has a type, default, range and reload class; unknown keys, bad ranges and cross-field violations fail with a path | `tests/config.rs` |
| Precedence and reload | defaults < file < world < env < CLI; each reload class behaves as documented | `tests/config_reload.rs` |
| Backup/restore drill | `kill -9` during a save and during a backup, then restore and verify every region; no data beyond the last commit is lost | `tests/backup_restore.rs` |
| 24-hour soak | 50 bots for 24 h: TPS, RSS slope, queue depth, save latency, no leak, no corruption | nightly, MS-15 |
| Upgrade/downgrade | N−1 world upgrades and serves; a newer world is refused; rollback works from the backup | CI matrix |
| Metrics contract | every metric in §3.6 exists with the documented type, unit and labels; no unbounded label | `tests/metrics_contract.rs` |
| Health semantics | liveness and readiness transition correctly while loading, draining and degraded | endpoint tests |
| Container inspection | image ≤ 60 MB, non-root, no shell, no GPU/audio/X11/Wayland libraries, read-only root works | image test |
| Runbook drills | each §3.11 runbook executed once with a recorded outcome | release checklist |

The soak and join-storm harnesses are the same bots used by [MS-15](../plan/01-milestones.md) and by
[24](24-testing-benchmarks.md) §3.13, so a capacity claim, a regression and a support diagnosis share
one measurement path.

## 8. Open questions

| # | Question | Proposal | Owner |
| --- | --- | --- | --- |
| 1 | Should the RCON-compatible listener ever be on by default for hosted servers? | No: it is plaintext and legacy. Off by default, loopback only, documented as such. Register item needed only if a hosting partner requires it. | MS-07 |
| 2 | Container base: musl `scratch` or distroless/cc for Wasmtime? | Choose with the MS-09 Wasmtime audit; the image must stay ≤ 60 MB and run read-only. Register item needed: container libc and Wasmtime support. | MS-07 / MS-09 |
| 3 | Is multi-world hosting (a lobby plus several worlds) a 1.0 feature? | No: one active world per session; the proxy and lobby arrive with MS-21. | MS-21 |
| 4 | Default backup destination inside the world directory or external? | Inside by default (single volume, simple permissions), with `external_copy` for disaster recovery; the world-size budget excludes backups. | MS-13 |
| 5 | May `online_mode = "open"` be used on a public server? | Permit it but warn at start-up and in `status`; it exists for LAN and development, and the server is unauthenticated. | MS-07 / MS-20 |
| 6 | Do we ship a web administration UI? | Not at 1.0: console, RPC and Grafana cover it; revisit after MS-21. | MS-16 |

## 9. References

- Decisions: [DEC-003](../decisions.md), [DEC-024](../decisions.md), [DEC-026](../decisions.md),
  [DEC-028](../decisions.md), [DEC-033](../decisions.md), [DEC-035](../decisions.md),
  [DEC-038](../decisions.md), [DEC-042](../decisions.md), [DEC-044](../decisions.md),
  [DEC-046](../decisions.md), [DEC-047](../decisions.md).
- Chapters: [00](00-conventions.md) §3.4; [01](01-technical-architecture.md) §3.1, §3.8, §5;
  [02](02-engine-core.md) §3.5, §3.8, §3.11; [08](08-gameplay-survival.md) §3.12;
  [17](17-persistence.md) §3.1, §3.5, §3.7, §3.8; [18](18-networking-multiplayer.md) §3.4, §3.6,
  §3.12; [20](20-security-and-anti-cheat.md); [25](25-performance-budgets.md).
- Research: [networking SOTA](../research/04-networking-sota.md) §5 (server operations, metrics,
  backups, capacity); [persistence](../research/09-persistence-assets-packaging.md) §6.7 (container
  image, reproducibility) and §3.7–§3.8 (compaction, backups); [Weft teardown](../research/01-weft-teardown.md)
  §4.4 (append-only regions and the format-gate lesson).
- Milestones: [MS-07](../plan/01-milestones.md#ms-07--dedicated-server-020alpha),
  [MS-15](../plan/01-milestones.md#ms-15--scale--anticheat-100rc),
  [MS-16](../plan/01-milestones.md#ms-16--10-launch-100).
- External: Prometheus exposition format <https://prometheus.io/docs/instrumenting/exposition_formats/>;
  systemd.exec hardening directives
  <https://www.freedesktop.org/software/systemd/man/systemd.exec.html>; Docker Compose file
  reference <https://docs.docker.com/compose/compose-file/>; RFC 3339 timestamps
  <https://www.rfc-editor.org/rfc/rfc3339>; BLAKE3 <https://github.com/BLAKE3-team/BLAKE3>;
  `kill(2)` and signal semantics <https://pubs.opengroup.org/onlinepubs/9699919799/>.

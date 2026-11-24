# 16 — Modding

> Status: normative - Last updated: 2026-11-24
> Depends on: [DEC-003](../decisions.md), [DEC-007](../decisions.md), [DEC-008](../decisions.md), [DEC-019](../decisions.md), [DEC-020](../decisions.md), [DEC-023](../decisions.md), [DEC-027](../decisions.md), [DEC-035](../decisions.md), [DEC-040](../decisions.md), [DEC-042](../decisions.md), [DEC-043](../decisions.md), [DEC-049](../decisions.md), [conventions](00-conventions.md), [architecture](01-technical-architecture.md), [content system](15-content-system.md)
> Implements: MS-09; native tier MS-21

## 1. Purpose and scope

This chapter specifies the executable tiers of the content platform: the sandboxed **WASM mod** host
that ships at [MS-09](../plan/01-milestones.md), its capability model, host ABI v1, event and action
model, client delivery of mod-authored content, developer tooling, and the **native plugin** tier
that is normative planning for post-1.0 ([MS-21](../plan/01-milestones.md), OPEN-007). It owns
`ob_modding` (runtime, ABI, quota accounting) and the mod-facing surface of `openblock-tools`.

Content that is *only* data is [15](15-content-system.md); this chapter adds code to that model and
nothing else. Three consequences shape every rule below:

- **Server-side authority.** Mods run in the authoritative host, never on a client (DEC-008). A
  client receives content and prediction descriptors, never code (DEC-020, DEC-049).
- **No ambient authority.** A mod can do exactly what its granted capabilities and quotas allow:
  no filesystem, no sockets, no clock, no process APIs, no way to reach another mod (DEC-020).
- **Budgeted, attributed, containable.** Every callback has a deadline, every host call has a
  quota, every trap is a value rather than a crash, and every microsecond is attributed to the mod
  that spent it.

Out of scope: the content schema and hashing (chapter 15), the protocol that carries mod messages
([18](18-networking-multiplayer.md)), server administration UX ([19](19-server-administration.md))
and the general threat model ([20](20-security-and-anti-cheat.md)).

## 2. Requirements

| ID | Requirement | Verified by |
| --- | --- | --- |
| REQ-MOD-001 | There MUST be exactly three tiers — content pack, WASM mod, native plugin — and a server MUST NOT execute code found in a content pack's `data/`, `assets/` or `ui/` trees (DEC-020). | Malicious-pack corpus |
| REQ-MOD-002 | A mod's capabilities MUST be declared in its manifest, granted by the operator, checked when the module is linked and again on every host call; no capability is granted by default. | Capability matrix tests |
| REQ-MOD-003 | Each mod MUST have its own store, instance and linear memory; no memory, handle or pointer is shared between mods or exposed to a guest. | Isolation tests, review |
| REQ-MOD-004 | The host MUST enforce per-callback, per-tick and lifetime quotas for CPU time, memory, host calls, actions, events, storage and logging, and MUST report each quota in diagnostics. | Quota fixtures |
| REQ-MOD-005 | A guest trap, fuel exhaustion, epoch deadline or memory-limit breach MUST discard that mod's uncommitted batch, disable the mod and leave the host alive, per REQ-ARCH-021; soft-limit breaches are counted and reported. | Malicious-mod corpus |
| REQ-MOD-006 | No host call MAY block on disk, network, GPU or another mod; mutations MUST be queued and applied at the tick commit point. | Stall injection, review |
| REQ-MOD-007 | The ABI MUST be versioned by a WIT package's semantic version; an unknown major is rejected at load with an actionable error, an unknown import fails instantiation, and minors are additive only. | ABI conformance suite |
| REQ-MOD-008 | The manifest MUST be validated for namespace ownership, semver, `api` range, capability set, dependencies and entry-point presence; unknown fields are errors. | Manifest fixtures |
| REQ-MOD-009 | Events MUST be batched once per tick per mod, ordered by `(phase, tick, sequence)`, with typed core events and schema-hashed custom payloads; no per-block callback may exist. | Event golden tests |
| REQ-MOD-010 | Event subscriptions MUST filter before serialisation; an unsubscribed event MUST cost no guest call and no serialisation. | Subscription benchmark |
| REQ-MOD-011 | Every host function MUST have a per-call size cap, a per-tick call cap and a documented error code; a call above a cap is rejected, never truncated silently. | ABI boundary tests |
| REQ-MOD-012 | Numeric IDs at the ABI boundary MUST resolve through the connection dictionary of DEC-040; strings resolve once at init, and a mod MUST NOT persist a NetId or a registry index. | Dictionary tests, storage scan |
| REQ-MOD-013 | Mod actions MUST be recorded in the replay log; deterministic mode MUST additionally meter fuel and use a seeded per-mod RNG so re-execution reproduces the same action log (DEC-007). | Replay comparison tests |
| REQ-MOD-014 | Per-mod storage MUST be a versioned key-value store, namespaced by mod ID, quota-bounded, surviving reloads, and MUST NOT contain world pointers or NetIds as durable references. | Storage tests, save round-trip |
| REQ-MOD-015 | Commands registered by a mod MUST live under its namespace, declare a permission level, be rate-limited, and be validated exactly like built-in commands (REQ-GAME-029). | Command abuse corpus |
| REQ-MOD-016 | UI data binding MUST be declarative display data computed by the host and delivered through the host UI binding, updated at most 10 times per second; the client runs no mod code, the binding MUST NOT write gameplay state, and UI actions are intents the host validates. | UI integration test |
| REQ-MOD-017 | Mod network messages MUST be namespaced and negotiated by schema hash; an unknown channel is a protocol error, payloads are rate-limited, and no message may carry code. | Protocol tests |
| REQ-MOD-018 | A client MUST receive only content packs the server authorises, verified against the content root; it MUST NOT receive, install or execute mod code ([15](15-content-system.md) §3.10). | Two-client join tests |
| REQ-MOD-019 | Prediction descriptors MUST be content-hash-covered data; a client predicts only from them, and undocumented behaviour stays server-authoritative with correction. | Prediction/correction tests |
| REQ-MOD-020 | Registry contributions MUST occur only at load time through chapter 15's pipeline; a frozen registry is immutable and a late contribution is an error that disables the mod. | Late-registration test |
| REQ-MOD-021 | Hot reload MUST swap the instance at a tick boundary, keep host-owned storage, retain the old instance until the new one initialises, and keep the old one on failure, within 5 s. | Reload integration test |
| REQ-MOD-022 | Cost MUST be attributed per mod: CPU per callback, host calls, accepted and rejected actions, traps, memory peak, storage bytes. | Diagnostics inspection |
| REQ-MOD-023 | An operator MUST be able to allowlist by `(id, version, hash, signer)`, grant or revoke capabilities, disable and quarantine a mod, and audit every decision. | Admin integration tests |
| REQ-MOD-024 | The native tier MUST use a small versioned C ABI, an operator allowlist, a signature check, crash containment and no auto-download; it ships post-1.0 (DEC-020, OPEN-007). | Native harness |
| REQ-MOD-025 | Mods MUST NOT add scheduler systems, change stage order or timing, open windows, or link GPU or audio code; `ob_modding` MUST NOT depend on `wgpu`, `winit` or `cpal` (REQ-ARCH-014, DEC-003). | `arch-guard`, inspection |
| REQ-MOD-026 | Mod code MUST NOT run on a client in the 1.0 tier; [OPEN-006](../decisions.md) is closed by [DEC-049](../decisions.md), a client tier is post-1.0, and any such tier would need a new decision and this ABI's capability model. | Binary inspection, review |
| REQ-MOD-027 | Every host function MUST have a threat-model entry, a capability, a quota, error codes, a fuzz corpus seed and two reviewing maintainers before it ships. | Review checklist |
| REQ-MOD-028 | Deprecations MUST be announced for at least two minor releases or six months, the previous major MUST be hostable in parallel for twelve months, and the previous release's example mods MUST run in CI. | Compatibility suite |
| REQ-MOD-029 | Guest logging MUST be structured, attributed, rate-limited and size-capped; guest-controlled bytes MUST never be interpolated into a log line unescaped. | Log-forging corpus |
| REQ-MOD-030 | A mod failure MUST NOT corrupt a registry, a save or another mod's state; invalid contributions and actions are rejected atomically with the generation unchanged. | Fault-injection suite |
| REQ-MOD-031 | The reference mod set MUST meet the budgets of §5, gated in CI with the [DEC-035](../decisions.md) tolerance. | `bench_mods` gate |
| REQ-MOD-032 | Disable, unload and 100 reload cycles MUST NOT grow host memory beyond 1 MiB of RSS slope. | Soak test |

## 3. Design

### 3.1 Three tiers and the trust matrix

| Tier | Artifact | Runs | Sandbox | Trust | Ships |
| --- | --- | --- | --- | --- | --- |
| **T0 content pack** | `.obpack` with `data/`, `assets/`, `ui/`, `lang/` | client and server, as validated data | none needed: validated data | untrusted data | MS-05 |
| **T1 WASM mod** | `.obpack` with `scripts/mod.wasm` | server host only | wasmtime instance, capabilities, quotas | untrusted code | MS-09 |
| **T2 native plugin** | `.so`/`.dll`/`.dylib` | supervised worker process by default | none; operator-allowlisted | trusted by operator | post-1.0, MS-21 |

The tiers are cumulative: a mod is a content pack plus an executable artifact, so it uses the same
manifest, hash, layer, dependency and reload machinery. A pack with `scripts/mod.wasm` but no `[mod]`
table is a pack error; a `[mod]` table without the artifact is an error. A mod may contribute
content only through [15](15-content-system.md)'s loader, at load time, under its own namespace.

**No code on the client at 1.0.** T0 is data and is validated and rendered on a client; T1 and T2 run
only in the host. The 1.0 client contains no wasm engine, no mod runtime and no per-screen code, so a
mod reaches a client only as content: data, view assets and prediction descriptors ([DEC-049](../decisions.md)).

```mermaid
sequenceDiagram
  participant S as Simulation (host)
  participant E as Event collector
  participant R as ob_modding runtime
  participant G as Guest instance (one per mod)
  participant Q as Action queue
  participant W as World edit API
  S->>E: collect events in phase order
  E->>R: capability-filtered batch per mod
  R->>G: on_event / on_tick (epoch deadline, quotas)
  G-->>R: host calls (queries, queued mutations)
  R->>Q: validate capability, cap and quota
  Q->>W: commit in deterministic order, after all mods
  W-->>S: edits applied; resulting events reach the next tick
```

Only the host thread enters a guest. Host functions are leaf calls: they never call back into the
guest, so re-entrancy cannot recurse. A guest call that would need to wait returns a token instead
(§3.4.3). Mods cannot observe another mod's callback order; they observe world state only at commit.

### 3.2 The WASM host

The runtime is wasmtime **51.0.0** with the component model enabled; the WIT package is
`openblock:mod@1.0.0` ([research/06](../research/06-modding-and-content-system.md) §3.2). Rust 1.97 is
the minimum for wasmtime 51 and the workspace pins 1.100. No WASI interface is provided: the guest
gets no filesystem, no sockets, no clocks and no environment. A module that imports WASI links
against an unresolved import and fails instantiation with `0x0401` naming the import — a deliberate
tripwire, because a mod that accidentally uses `std::fs` must fail loudly rather than silently get a
sandboxed stub.

| Setting | Value | Rationale |
| --- | --- | --- |
| `wasm_component_model` | `true` | typed WIT boundary; semver versioning (DEC-020) |
| `wasm_threads`, `wasm_simd` | `false` | no shared memory between mods; deterministic baseline |
| `wasm_reference_types`, `bulk_memory` | `true` | needed by the canonical ABI |
| `StoreLimits::memory_size` | 64 MiB | guest memory ceiling (PERF-MOD-004, [25](25-performance-budgets.md)) |
| `max_instances` per mod | 1 | one live instance, one linear memory |
| `epoch_interruption` | `true`, deadline 1 ms per callback | hard DoS guard |
| `consume_fuel` | off in guarded mode, on in deterministic mode | fuel is deterministic but slower ([research/06](../research/06-modding-and-content-system.md) §3.2) |
| `wasm_nan_canonicalization` | `true` in deterministic mode | NaN bit patterns cannot leak host state |
| `async_support` | off | all calls are bounded and synchronous |
| `parallel_compilation` | `true` | compile off-thread, never in a tick |

**Quotas.** Every quota is per mod, enforced by the host, and reported in the mod account (§4.5):

| Resource | Quota | On breach |
| --- | --- | --- |
| CPU per callback | 1 ms wall clock (epoch), 25 M fuel in deterministic mode | trap `0x0703`/`0x0702`; mod disabled |
| CPU per tick, all mods | 1.5 ms p99 aggregate for the reference set (PERF-MOD-002) | guarded mode stops dispatching for the rest of the tick and records the skipped mods; deterministic mode alerts only |
| Guest memory | 64 MiB, growth denied by `ResourceLimiter` | trap `0x0704`; mod disabled |
| Host calls | 10,000 per tick per mod (≤ 1,000 world queries), 64 deferred queries outstanding | call fails `0x0205`; counted |
| Actions | 4,096 per tick, 512 block cells per call | excess rejected `0x0205`; counted |
| Events | 16,384 per tick, coalesced per the table in §3.5 | low-priority events dropped; counted |
| Storage | 8 MiB, 1 MiB per value, 4,096 keys | write fails `0x0501`; mod keeps running |
| Log and metrics | 100 lines/s, 4 KiB per line, 64 metrics | excess dropped; counted |
| Network | 32 KiB/s per player, 1 MiB/s per mod | excess rejected `0x0602` |
| Module | 16 MiB, one artifact per mod | manifest error |

**Determinism.** Two modes exist, and the difference is recorded in the session manifest:

- **Guarded (default).** Epoch deadlines bound wall-clock time. Callback ordering is deterministic,
  but a near-deadline callback may trap on one host and complete on another, so replays consume the
  recorded action log rather than re-executing guests.
- **Deterministic (`deterministic = true`).** Fuel metering, canonical NaNs, a per-mod RNG stream
  derived from `(world seed, mod id, tick, call sequence)` and no wall-clock input. Re-executing the
  guest reproduces the action log; `--verify-mods` does exactly that and marks a divergent replay
  non-reproducible instead of pretending otherwise.

Competitive and replay servers SHOULD enable deterministic mode for mods that change world state;
the cost is the measured fuel overhead, not a guess. Wall-clock dispatch cut-off is a guarded-mode
convenience precisely because it cannot be reproduced: it is recorded in the action log, and
deterministic mode never takes it.

**Teardown.** Disable, quarantine and reload share one path: stop entering the guest, discard the
pending action batch, cancel scheduled timers and subscriptions, invalidate query tokens, drop the
store so linear memory is released, keep host-owned storage and the account record, then emit
`unload` with a reason. A guest cannot refuse teardown — there is no destructor call that can block,
and the host never waits on guest code to exit.

**Traps and containment.** A guest trap is a `Result`, never a panic. A panic inside a host closure is
caught (`catch_unwind`, available because server builds unwind per DEC-042) and converted to
`0x0705`; the batch is discarded, the mod disabled, and the trap recorded with its code, callback,
last host call and a 32-frame guest backtrace when DWARF is present. In client and distribution
builds `panic = "abort"` applies, and mods do not run there in 1.0 (REQ-MOD-026). A single tick
exceeding 10 s is a host bug, not a mod fault: the watchdog writes a crash report and exits 70
([02 §3.11](02-engine-core.md#311-errors-panics-and-exit-codes)).

### 3.3 Capabilities

Seven capabilities exist. A mod declares the ones it needs; the operator grants a subset; the loader
provides imports only for granted capabilities, and every call re-checks the grant so a mid-session
revocation takes effect at the next tick boundary.

| Capability | Grants | Default | Enforced at | Quota |
| --- | --- | --- | --- | --- |
| `world.read` | block, biome, light, height, entity and component queries; raycasts; region snapshots | off | link, call | 10,000 host calls/tick, of which ≤ 1,000 world queries ([10](10-physics-and-collision.md) §3.10) |
| `world.write` | queued block edits, block-entity mutation, damage, effects, component writes | off | link, action validation | 4,096 actions/tick, 512 cells/call |
| `entity.spawn` | spawn and despawn entities and item stacks | off | action validation | 64 spawns/tick, 256 live per mod |
| `ui` | open screens, bind values, notifications for a player | off | host; client only renders | 1,024 values/tick, 10 Hz |
| `network` | namespaced messages to clients | off | channel negotiation | 32 KiB/s/player |
| `storage` | per-mod key-value store | off | namespace isolation | 8 MiB |
| `server.admin` | register commands, write allowlisted game rules, broadcast | off | permission level + operator allowlist | 32 commands, 16 rules |

`entity.spawn` is a grant *in addition to* `world.write`: spawning is the most expensive and most
abusable action, so the host does not let a build-only mod fill the world with entities.
[20](20-security-and-anti-cheat.md) §3.10 groups `spawn_entity` under `world.write` for brevity; the
host enforces the more specific grant, which is never weaker. `actions.world` and `entities.write`
are deprecated aliases of `world.write`, accepted for one major version from older manifests and
configs; the loader logs a deprecation warning and rewrites them on export, and canonical logs and
audit records always use the dotted name.

### 3.4 Host ABI v1

#### 3.4.1 Surface and batching

The ABI exposes coarse-grained calls only: a block query of up to 16³ cells, an entity page of 256
IDs, an edit batch of up to 512 cells. There is no per-block or per-entity call, because a
per-block host call at 20 Hz does not fit in 1.5 ms/tick
([research/06](../research/06-modding-and-content-system.md) §3.7). Queries return copied values;
no borrowed host memory outlives a call. Strings are UTF-8; invalid UTF-8 returns `0x0404` rather
than being lossily converted.

| Group | Functions | Capability |
| --- | --- | --- |
| session | `abi_version`, `tick`, `rng_u64`, `content_id`, `tag_contains` | none (content metadata only) |
| scheduling | `schedule(delay_ticks, tag)`, `cancel(token)`, `subscribe(kinds)`, `unsubscribe` | none |
| diagnostics | `log(level, message)`, `metric(name, value)` | none |
| world queries | `query_blocks`, `query_entities`, `get_component`, `entity_uuid`, `raycast`, `light_at`, `height_at`, `biome_at` | `world.read` |
| world writes | `edit_blocks(batch)`, `set_block_entity`, `damage`, `add_effect`, `set_component`, `use_item_on` | `world.write` |
| entities | `spawn_entity(kind, pos, components)`, `despawn_entity` | `entity.spawn` |
| UI | `ui_open(player, screen, args)`, `ui_bind(player, screen, key, value)`, `ui_close`, `notify` | `ui` |
| network | `send(player, channel, payload)`, `broadcast(channel, payload)` | `network` |
| storage | `kv_get`, `kv_set`, `kv_delete`, `kv_list` | `storage` |
| admin | `register_command(name, permission, usage)`, `set_game_rule(id, value)`, `broadcast_message(text)` | `server.admin` |

`content_id("block", "openblock:stone")` resolves through the connection dictionary once; a mod
caches the numeric result and never persists it (DEC-040). `entity_uuid` exists because a mod that
must remember an entity across restarts needs the persistent identifier of DEC-043, not the NetId.

#### 3.4.2 Errors

Every fallible import returns `result<T, u16>` with a stable code; ranges group by cause (§4.4). A
capability denial is `0x0101`, a quota breach `0x0205`, a bad handle `0x0206`, a channel that was
never negotiated `0x0601`. Errors never poison the instance: a mod may handle `0x0205` by backing off
and retrying next tick. The host never returns `Ok` for a partially applied mutation: each call's
batch is atomic, so either all of it is queued or none of it is.

#### 3.4.3 Asynchrony

There is no blocking wait and no `await`. Cheap calls complete inline. Expensive calls
(`query_entities` beyond one page, `raycast` beyond 512 blocks, a region snapshot) return a token and
complete later through `on_query(token, payload)`, delivered before the event batch of the next
tick. At most 64 tokens may be outstanding; a token expires after 20 ticks and is reported as a
timeout, so a mod cannot leak tokens. Tokens die with the instance on reload, and the guest is told
`0x0303` if it calls with a token from a previous instance.

### 3.5 The event model

Events are collected during the tick and delivered as one batch per mod at the **mod event point**:
after the simulation systems, before the action commit. The order inside a batch is
`(phase, tick, emission sequence)`; a mod cannot see another mod's batch or influence its order.

| Event | Phase | Payload | Coalescing and cap |
| --- | --- | --- | --- |
| `world_load` | 0 | world UUID, tick | once per world load |
| `player_join` | 1 | player key, name | never dropped |
| `tick` | 2 | tick number | once per tick |
| `block_break`, `block_place` | 3 | position, old/new state, actor | one per position, last wins, 4,096 |
| `entity_death` | 4 | NetId, killer, damage type | 1,024 |
| `player_leave`, `player_chat` | 5 | player key, message | chat coalesced to 20/s per player |
| `world_save` | 6 | tick, duration | once per save cycle |
| `custom` | 7 | channel, schema hash, bytes | 4,096, dropped first under pressure |
| `unload` | 8 | reason | at most once, no actions accepted |

`on_command` and `on_ui_action` are separate callbacks, not events: a command runs in its own
budgeted call with its arguments validated, and a UI action is an intent that the host checks
exactly like the equivalent client intent (DEC-023). Without `world.read`, world events are delivered
as aggregate counts rather than positions, so a mod cannot use the event stream to bypass the query
capability. Subscription is a filter applied before serialisation (REQ-MOD-010); `tick` is the only
event always delivered, and a mod that returns from `on_tick` without work costs one boundary
crossing, not a tick of latency.

### 3.6 Actions and the commit point

A guest never mutates the world. It queues actions, and the host applies them after every mod's
callbacks for that tick have returned:

1. Per mod in registration order, per call in call order; block edits inside one call are sorted
   canonically before application ([05](05-world-model.md) REQ-WORLD-014).
2. Capability, ownership, bounds, protection, reach, rate and budget are validated per action; a
   rejected action is counted and logged once per tick, never retried automatically.
3. A call whose batch fails validation is rejected as a whole (`0x0203` or `0x0209`), so a mod cannot
   half-place a structure.
4. Applied edits go through the world edit API and emit the same events as a player edit, which are
   visible to mods on the next tick — never in the same batch that produced them.
5. Actions submitted during `on_shutdown` were already discarded at unload; the host returns
   `0x0302`.

Because the commit point is after all callbacks, two mods editing one block in one tick resolve by
the deterministic order above, and the world never observes a partially applied callback.

### 3.7 What a client receives

A client never receives code. At join it receives the server's content manifest — `content_hash`,
`view_hash`, pack IDs, versions, sizes, licences, signers and an opaque `behaviour_hash` — and either
matches the content root or fetches the missing leaves from the server's content-addressed endpoint,
verifies each leaf and the root, installs them in its per-server cache and reloads
([15](15-content-system.md) §3.10). `scripts/` is excluded from the client-facing Merkle tree; a
client that finds a script in an installed pack refuses the pack. The consent prompt names size,
licence, signer and source; an operator may disable pushing entirely.

Behaviour reaches the client only as **prediction descriptors**: an optional `prediction` component
of a block or entity entry, part of `content_hash` rather than `view_hash`:

```ron
// data/blocks/conveyor.ron (fragment)
BlockDef(
  id: "kiln:conveyor",
  // … shape, material, rendering, loot …
  prediction: PredictionDef(
    shape: [(0, 0, 0, 16, 9, 16)],
    velocity: Some((x_milli: 2000, y_milli: 0, z_milli: 0)),
    friction_permille: 900,
    use_time_ct: 0, cooldown_ct: 0, changes_world: false,
  ),
)
```

The client predicts movement on a belt, a moving platform or a custom collision shape from these
fields and reconciles against the host like any other predicted motion. Anything without a
descriptor is server-authoritative and may correct; that is the explicit price of not shipping mod
code to clients (DEC-008, DEC-020).

**UI values are computed on the host.** `ui_bind` and `ui_open` run only in the host process. The host
resolves the screen document's declared `bind` paths against its own state, applies any `ui_bind`
values the mod sets, at most 10 times per second, and sends the resulting values as data over the same
UI binding that core screens use
([13 §3.10](13-ui-and-ux.md#310-data-driven-ui-for-content-packs-and-mods)); the client renders them in the declared
widgets and returns actions as intents. Nothing is evaluated where the player is: the client has no
module runtime and no per-screen code path, and a mod cannot ship a UI callback that a client
executes ([DEC-049](../decisions.md)). The value model and its 10 Hz rule are shared with
[13](13-ui-and-ux.md), and the update is coalesced with ordinary state replication rather than sent
per widget.

### 3.8 Developer experience

The path from `git clone` to a new block in-game is a [vision §8](../vision.md) metric of ≤ 10
minutes, so it is a tested artefact:

| Tool | Purpose |
| --- | --- |
| `xtask mod new kiln --template {machine, minigame, ai}` | template crate, `pack.toml`, one block, one item, one recipe, one command, a WASM entry point, tests |
| `xtask mod dev --world ./dev-world --watch` | integrated host on a dev world, rebuild, hot reload of packs and the module, error overlay |
| `xtask mod build --check` | manifest, WIT import list, capability and quota validation without running |
| `xtask mod profile --top 20` | per-mod p50/p95/max callback cost, host calls, action queue depth, flamegraph export |
| `openblock-tools log view --follow --mod kiln` | attributed, filtered log with guest spans |
| in-game mods panel | state, version, hash, capabilities, live cost, reload and disable buttons (operators only) |

The template builds a component with `cargo component build --target wasm32-wasip2`; the host
provides no WASI 0.2 imports, so an accidental `std::fs` use fails instantiation with an actionable
error rather than a stub. Dev builds carry DWARF (`debug = true`) so traps show guest frames. Three
example mods ship and are part of CI: `examples/mods/kiln` (machine with storage), `examples/mods/duel_arena`
(minigame lifecycle and commands) and `examples/mods/herd` (AI task and UI values). They are also the
compatibility corpus of REQ-MOD-028: each release's tagged artifacts run against the next host.

### 3.9 The native plugin tier (post-1.0)

Native plugins exist for operator-trusted work that cannot pay the WASM boundary: platform
integration and performance-critical extensions. They are **not** a modder escape hatch. The tier is
normative planning gated on OPEN-007, ships at [MS-21](../plan/01-milestones.md), and never
auto-downloads.

```c
#define OB_NATIVE_ABI_MAJOR 1
typedef struct { uint32_t abi_major, abi_minor, struct_size; } ObAbiHeader;

typedef struct ObHostV1 {
  ObAbiHeader hdr; void *ctx;
  uint32_t (*log)(void *ctx, uint32_t level, const uint8_t *msg, size_t len);
  uint32_t (*query_blocks)(void *ctx, const int32_t origin[3], const uint32_t size[3],
                           uint32_t mask, uint32_t *out, size_t out_cap);
  uint32_t (*submit_actions)(void *ctx, const uint8_t *batch, size_t len);
} ObHostV1;

typedef struct ObPluginV1 {
  ObAbiHeader hdr; const char *id;
  int32_t (*init)(const ObHostV1 *host, const uint8_t *manifest, size_t len);
  int32_t (*on_events)(const uint8_t *events, size_t len);
  void    (*shutdown)(void);
} ObPluginV1;

OB_EXPORT const ObPluginV1 *ob_plugin_v1(void);
```

Rules: the header is the first field; a caller ignores trailing fields it does not know via
`struct_size`; no Rust type, unwinding, `Vec` or allocator crosses the boundary; strings are
`(ptr, len)` UTF-8; buffers are caller-owned or freed by an exported `free` function; every call
happens on one host thread, and a plugin may not call host functions from threads it creates. The
semantics of the host functions are the WASM ABI's, so behaviour ports without redesign.

The default deployment is a **supervised worker process** (`openblock-server --native-worker`)
speaking a framed IPC channel, which gives real crash containment. In-process loading is permitted
only with `native.in_process = true` and a signature from an allowlisted key, and is documented as
deprecated-risk. A worker crash restarts the worker and replays the tick's queued actions; three
crashes in ten minutes quarantine the plugin. Native plugins are exempt from nothing in §3.3, and
they are held to the same action, budget and audit model.

### 3.10 Operator controls

```toml
[mods]
enabled = ["openblock:kiln", "openblock:herd"]
allowlist = ["blake3:9f2c…", "blake3:41aa…"]    # hashes, not names ([19](19-server-administration.md) §4)
require_signature = true
push_to_clients = false
trust_updates = false           # a new hash needs re-approval
default_capabilities = []      # DEC-020: approval, never assumption
wasm_wall_ms_per_tick = 2      # epoch backstop; per-callback deadline stays 1 ms

[mods.grants."openblock:kiln"]
capabilities = ["world.read", "world.write", "storage"]
cpu_millis_per_tick = 0.25
actions_per_tick = 512

[mods.native]
enabled = false
in_process = false
allow = []
```

Legacy spellings (`world_read`) are accepted from older configs and canonicalised to the dotted
capability names on load; the canonical form in logs and audit is always `world.read`. The
deployment default of `default_capabilities` is empty here even though
[19](19-server-administration.md) §3.2 allows an operator to pre-approve `world.read` per mod, because DEC-020 requires
an operator decision per capability.

Console commands (`mod list|info|grant|revoke|disable|enable|quarantine|reload|timings`) require
permission level 4, except `list` and `info`, which need 3
([19](19-server-administration.md) §5). Grants are keyed by `(id, version range, hash)`; with
`trust_updates = false` an update lands **disabled** until the operator approves the new hash. `quarantine` moves the artifact
and its storage out of the load path into `<world>/quarantine/mods/<id>/<hash>/` and records why.
Every decision appends one JSONL audit record (§4.5), so "who enabled this mod and when" is a query,
not a guess. Capability revocation applies at the next tick boundary; the mod stays loaded and its
denied calls return `0x0102`, which lets well-written mods degrade instead of trapping.

### 3.11 Security review requirements

| Threat | Vector | Control | Test |
| --- | --- | --- | --- |
| Sandbox escape | wasmtime or host-function bug | pinned runtime, no WASI, no `unsafe` in `ob_modding` outside the audited store wrapper, 24 h fuzz | malicious corpus, Miri, advisory watch |
| Capability bypass | calling an ungranted import, forging a handle, reusing a token | link-time import gating, call-time re-check, NetId validation, per-instance tokens | boundary tests, forged-handle corpus |
| DoS | infinite loop, memory bomb, action flood, log flood, event storm | epoch, limiter, per-tick caps, rate limits, coalescing | quota fixtures, 20-mod stress |
| Supply chain | tampered artifact, malicious update, dependency compromise | hash-pinned artifacts, signatures, lockfile, `cargo-deny`, no auto-download, no network at load | signature and lockfile tests |
| Registry/save corruption | invalid registration, dangling components, partial commit | load-time registration only, atomic action batches, immutable generations | fault injection, save round-trip |
| Information leak | world queries outside interest, logs, metrics, network | capability gates, redaction without `world.read`, rate-limited egress | leak corpus, redaction tests |
| Cheat surface | mod-authored client behaviour or prediction descriptors | no client code, descriptors are content-hashed and host-validated | two-client prediction tests |

Any new host function needs a row in this table, a capability, a quota, error codes, a fuzz seed and
two reviewers; a host function without a test in the malicious corpus does not merge. The security
review at [MS-15](../plan/01-milestones.md) re-walks the table and must close with no open
high-severity finding.

### 3.12 API stability and deprecation

The ABI follows semver through the WIT package (`openblock:mod@1.0.0`) and the manifest's `api = 1`.
Adding a function or a variant case is a minor; changing a signature, removing a case or tightening a
quota is a major. The host advertises `abi_version() -> (major, minor)` and refuses an unknown major
at load with the required version, never by guessing. Major versions can be hosted in parallel
(`@1` and `@2` in one binary) for a twelve-month window, which is how the ecosystem survives a
breaking change; the exact-source-hash ABI of Weft, where every host edit invalidates every mod, is
the counter-example we refuse ([research/06](../research/06-modding-and-content-system.md) §2.5).

Deprecation is a process, not an announcement: a deprecated function or field keeps working for at
least two minor releases or six months (whichever is longer), logs one warning per mod per session,
has a documented replacement, and is listed in the ABI changelog. The compatibility suite compiles
and runs the previous release's example mods against the new host; a minor upgrade must not require a
rebuild, and the suite fails if it does.

## 4. Data structures and formats

### 4.1 Manifest

A mod is a pack ([15](15-content-system.md) §3.6) with a `[mod]` table. Top-level keys come before any
table header, and the top-level `api` is the host ABI major this chapter versions:

```toml
id = "openblock:kiln"
version = "1.4.2"
content_format = 1
api = 1
license = "MIT OR Apache-2.0"
layer = "mod"
edits = ["openblock:furnace"]

[requires]
"openblock:base" = "1"

[mod]
entry = "scripts/mod.wasm"      # the top-level `api = 1` is the host ABI major
game = ">=0.4, <2"
deterministic = false
commands = ["stoke", "vent"]

[mod.capabilities]
world.read = {}
world.write = { actions_per_tick = 512 }
storage = { quota_mib = 8 }

[mod.events]
subscribe = ["tick", "block_break", "block_place", "custom"]
```

### 4.2 ABI v1 (excerpt)

```wit
package openblock:mod@1.0.0;

interface types {
  record block-pos { x: s32, y: s32, z: s32 }
  record vec3u { x: u32, y: u32, z: u32 }
  record vec3i { x: s32, y: s32, z: s32 }
  type state-id = u32;          // connection dictionary index (DEC-040), session-local
  type entity-id = u64;         // NetId (DEC-043), never persisted
  enum log-level { trace, debug, info, warn, error }
  record action-status { accepted: u32, rejected: u32, code: u16 }
  variant event {
    world-load(u64), player-join(string), player-leave(string), tick(u64),
    block-break(tuple<block-pos, state-id>), block-place(tuple<block-pos, state-id>),
    entity-death(tuple<entity-id, string>), player-chat(tuple<string, string>),
    world-save(u64), custom(tuple<string, u32, list<u8>>),
  }
}

interface host {
  use types.{block-pos, vec3u, vec3i, state-id, entity-id, log-level, action-status, event};
  abi-version: func() -> tuple<u16, u16>;
  tick: func() -> u64;
  rng-u64: func() -> u64;
  content-id: func(kind: string, id: string) -> result<u32, u16>;
  tag-contains: func(tag: string, id: string) -> bool;
  log: func(level: log-level, message: string);
  metric: func(name: string, value: u64);
  schedule: func(delay-ticks: u32, tag: u32) -> result<u64, u16>;
  cancel: func(token: u64) -> bool;
  subscribe: func(kinds: list<string>) -> result<u64, u16>;
  query-blocks: func(origin: block-pos, size: vec3u, mask: u32) -> result<list<state-id>, u16>;
  query-entities: func(min: block-pos, max: block-pos, filter: string, page: u32) -> result<list<entity-id>, u16>;
  get-component: func(e: entity-id, key: string) -> result<list<u8>, u16>;
  entity-uuid: func(e: entity-id) -> result<string, u16>;
  raycast: func(origin: block-pos, dir: vec3i, max-cells: u32) -> result<option<tuple<block-pos, state-id>>, u16>;
  light-at: func(p: block-pos) -> result<u8, u16>;
  edit-blocks: func(edits: list<tuple<block-pos, state-id>>) -> result<action-status, u16>;
  spawn-entity: func(kind: string, p: block-pos) -> result<entity-id, u16>;
  damage: func(e: entity-id, amount: u32, kind: string) -> result<action-status, u16>;
  ui-bind: func(player: string, screen: string, key: string, value: u64) -> result<_, u16>;
  send: func(player: string, channel: string, payload: list<u8>) -> result<_, u16>;
  kv-get: func(key: string) -> option<list<u8>>;
  kv-set: func(key: string, value: list<u8>) -> result<_, u16>;
  register-command: func(name: string, permission: u8, usage: string) -> result<_, u16>;
  set-game-rule: func(id: string, value: i64) -> result<_, u16>;
}

world behaviour-mod {
  import host;
  export init: func(manifest-json: string) -> result<_, u16>;
  export on-tick: func(tick: u64) -> result<_, u16>;
  export on-event: func(batch: list<event>) -> result<_, u16>;
  export on-query: func(token: u64, payload: list<u8>) -> result<_, u16>;
  export on-command: func(player: string, name: string, args: list<string>) -> result<_, u16>;
  export on-ui-action: func(player: string, screen: string, action: string, args: list<u8>) -> result<_, u16>;
  export on-shutdown: func(reason: u8);
}
```

The excerpt omits `set-block-entity`,
`add-effect`, `set-component`, `use-item-on`, `despawn-entity`, `ui-open`, `ui-close`, `notify`,
`broadcast`, `kv-delete`, `kv-list` and `broadcast-message`; the generated WIT file is the normative
artifact and the manifest's import list is checked against it.

### 4.3 Event and action encoding

Batches cross the boundary in the ABI's own fixed little-endian encoding, not the protocol's
`postcard`: the shapes are small and fixed, the bytes are noise beside the canonical-ABI copy, and a
fixed layout is fuzzable byte-for-byte and readable in a packet dump.

| Structure | Fields | Fixed bytes (excluding variable payload) |
| --- | --- | --- |
| event header | `u8 kind`, `u32 tick`, `u16 payload_len` | 7 |
| block event | header + `i32 x,y,z` + `u32 old`, `u32 new` + `u64 actor` | 35 |
| death event | header + `u64 net_id` + `u8 killer_kind` + `u32 damage_type` | 20 |
| custom event | header + `u8 ns_len` + `u32 schema_hash` | 12 + namespace + payload |
| block edit | `i32 x,y,z` + `u32 state` + `u16 flags` | 18 |
| spawn action | `u16 kind_len` + `i32 x,y,z` | 14 + kind + components |

The guest sees decoded values; PERF-MOD-006 measures the encode/decode cost, and a golden-vector
suite pins the bytes so neither side can drift silently.

### 4.4 Error codes

| Range | Meaning | Examples |
| --- | --- | --- |
| `0x0001` | success (used where a status is returned) | — |
| `0x01xx` | capability | `0x0101` not granted, `0x0102` revoked mid-session |
| `0x02xx` | argument or limit | `0x0201` out of bounds, `0x0202` too large, `0x0203` unknown ID, `0x0204` invalid argument, `0x0205` quota exceeded, `0x0206` unknown entity, `0x0207` stale handle, `0x0208` read-only, `0x0209` schema mismatch |
| `0x03xx` | lifecycle | `0x0301` not in a callback, `0x0302` shutting down, `0x0303` not initialised, `0x0304` rate-limited |
| `0x04xx` | ABI | `0x0401` unknown import, `0x0402` major mismatch, `0x0403` missing export, `0x0404` invalid UTF-8, `0x0405` invalid component |
| `0x05xx` | storage | `0x0501` quota, `0x0502` key too long, `0x0503` value too large |
| `0x06xx` | network | `0x0601` channel not negotiated, `0x0602` payload too large, `0x0603` peer not permitted |
| `0x07xx` | runtime | `0x0701` guest trap, `0x0702` out of fuel, `0x0703` epoch deadline, `0x0704` memory limit, `0x0705` host panic contained |

The numeric ranges are stable across majors; a code is never reused for a different cause.

### 4.5 Accounting and audit records

```rust
pub struct ModAccount {
    pub id: ResourceId, pub version: Version, pub module_hash: Hash32,
    pub state: ModState,                    // Loading | Running | Degraded | Disabled | Quarantined
    pub cpu: Histogram,                     // ns per callback: p50, p95, max, total
    pub host_calls: u64, pub actions_accepted: u64, pub actions_rejected: u64,
    pub events_delivered: u64, pub events_dropped: u64,
    pub traps: u32, pub last_trap: Option<TrapRecord>,
    pub memory_peak: u64, pub kv_bytes: u64, pub pending_timers: u32,
}
```

One audit line per operator or lifecycle decision, JSONL, append-only:

```json
{"ts":"2026-11-24T15:40:12Z","kind":"capability_grant","mod":"openblock:kiln","version":"1.4.2",
 "hash":"blake3:9f2c…","capability":"world.write","actor":"console:martin","session":"…","tick":0}
```

## 5. Performance budgets

Aggregate mod budgets are owned by [25](25-performance-budgets.md) §5.6 and are not restated here:
`PERF-MOD-001` caps any single WASM mod at 0.30 ms/frame p95 on the client, measured against that
frame, and 1.5 ms/tick p99 on the server; `PERF-MOD-002` caps the reference eight-mod set at
0.15 ms/frame p95 client aggregate and 1.5 ms/tick p99 server aggregate; `PERF-MOD-003` caps load,
validation, compile and hot reload at 5 s each; `PERF-MOD-004` caps per-mod memory at 64 MB. At 1.0
the client half of `PERF-MOD-001`/`002` is a **reserved envelope, not a gate**: no mod code runs on a
client (REQ-MOD-026), so nothing draws on it in 1.0. The host-computed UI data of §3.7 arrives as
replicated display values and is charged to the ordinary client network and UI budgets, not to the
mod envelope. [OPEN-006](../decisions.md) is closed by [DEC-049](../decisions.md): a client code tier
is post-1.0, would need a new decision, and would inherit these ceilings unchanged rather than raise
them.
This chapter owns the ABI-level measurements that make those aggregates achievable:

| ID | Budget | Measurement |
| --- | --- | --- |
| PERF-MOD-005 | Trivial host call ≤ 120 ns p50 and ≤ 400 ns p99; 1 KiB payload ≤ 1.5 µs; 4 KiB ≤ 5 µs | 1 M-call microbenchmark |
| PERF-MOD-006 | Event and action encode/decode ≤ 5 µs per mod per batch host-side, excluding guest execution, ≤ 32 B per event on average | batch bench |
| PERF-MOD-007 | Module compile ≤ 50 ms warm-cached and instantiate ≤ 5 ms, tighter than the `PERF-MOD-003` gate and the start-up path's real requirement | start-up bench |
| PERF-MOD-008 | Apply 4,096 queued block edits ≤ 0.5 ms, excluding light propagation | action commit bench |
| PERF-MOD-009 | Accounting, quota and audit overhead ≤ 50 µs/tick for 20 mods; logging stays within 100 lines/s per mod | profiler assertion |
| PERF-MOD-010 | The 20-mod stress set adds ≤ 64 MiB host-side RSS, excluding guest linear memories (PERF-MEM-015) | 24 h soak |

## 6. Failure modes and edge cases

| Failure | Symptom without the rule | Required behaviour |
| --- | --- | --- |
| Infinite loop | tick never returns; server freezes | epoch deadline traps `0x0703`, batch discarded, mod disabled, host continues |
| Memory bomb | host OOM or swap death | `ResourceLimiter` denies growth, trap `0x0704`, mod disabled |
| Filesystem or network escape | mod reads `/etc/shadow` or opens a socket | no WASI and no ambient authority; the import fails at link with `0x0401`; canary files untouched |
| Registry corruption | a mod rewrites block definitions at runtime | registration only at load, frozen registry immutable, late contribution disables the mod |
| Panic in a handler | process abort on the server | guest panic is a trap; host panic is caught (`0x0705`); mod disabled, world intact |
| Hot reload state loss | machines forget their contents | state lives in host-owned storage; reload keeps the KV namespace; guest statics are declared non-durable |
| Action during shutdown | writes after the last save | `0x0302`, actions discarded; unload is one-way |
| Forged or stale entity handle | host dereferences a pointer-sized ID | NetIds are validated per dimension and tick; unknown returns `0x0206` |
| Token leak | deferred queries pile up | 64 outstanding tokens, 20-tick expiry, `on_query` timeout event |
| Log or event flood | disk and network pressure | rate limits and coalescing; dropped counts reported, never silent |
| Deterministic divergence | replay produces a different world | replays consume the action log; `--verify-mods` marks divergence instead of claiming reproducibility |
| Mod update changes behaviour | players see different results per server | `content_hash`/`behaviour_hash` pinned; updates land disabled without operator approval |

## 7. Testing strategy

1. **Malicious mod corpus.** Hand-written modules for infinite loop, memory bomb, filesystem
   traversal, socket connect, registry corruption, panic, oversized return, invalid UTF-8, forged
   handle, re-entrant host call and event storm. Each must leave the host alive, disable the mod, write
   one audit line, leave canary files and listeners untouched, and keep the world hash unchanged.
2. **ABI boundary fuzzing.** Structured fuzzers for every import's argument tuple, the event decoder and
   the action encoder: 1 h per PR, 24 h in the [MS-15](../plan/01-milestones.md) soak, no panic, no
   OOM, no hang, ≥ 80% line coverage of `ob_modding`.
3. **Determinism with mods.** A replay of a 10,000-tick modded session with the action log produces the
   same state hash; deterministic mode re-executes guests and compares action logs; a deliberately
   nondeterministic test mod must be caught and reported.
4. **Compatibility.** The previous release's example mod artifacts, pinned by hash, instantiate and run
   against the new host; a minor upgrade must not require a rebuild; a major bump must be hostable in
   parallel and must fail with `0x0402` otherwise.
5. **Budget gate.** `bench_mods` runs the reference 8-mod set of PERF-MOD-002 (machines, AI tasks, a
   minigame, UI and storage) plus a 20-mod stress set, and fails CI on a `PERF-MOD-*` regression
   beyond tolerance.
6. **Isolation.** Cross-mod KV reads, handle forgery, log forging and capability confusion are tested
   as negative cases; two mods with the same key must never see each other's values.
7. **Reload and soak.** 100 reload cycles, a 24 h soak with the reference set, a `kill -9` during a
   commit, and a quarantine/restore cycle, asserting flat RSS and byte-identical saves.
8. **Native tier.** Supervised crash containment (worker killed mid-callback), signature mismatch,
   `struct_size` compatibility, and ABI minor mismatch are exercised on the harness before [MS-21](../plan/01-milestones.md).
9. **Operator controls.** Allowlist, deny, grant, revoke, disable, quarantine and `trust_updates = false`
   are integration-tested through the console and the admin RPC, with audit lines asserted.

## 8. Open questions

1. **Client presentation code (post-1.0; OPEN-006 is closed by DEC-049).** 1.0 ships none and the client
   contains no module runtime. If a post-1.0 tier is later proposed, does it reuse `openblock:mod@1.x`
   with the same capabilities and the reserved envelope of §5, or does it need a separate
   rendering-only ABI and a new decision? Owner: **MS-09**, post-1.0.
2. **Register item needed: fuel default.** Guarded mode is cheap but makes re-execution
   non-reproducible; deterministic mode costs the measured fuel overhead. Is fuel mandatory for
   competitive servers, or opt-in? Owner: **MS-09** with MS-15.
3. **wasmtime cadence.** Pin 51.0.0 and upgrade quarterly, or start on the 48.x LTS line for a
   twenty-four-month support window? The answer decides how often mods must be revalidated. Owner:
   **MS-09**.
4. **Native policy details (OPEN-007).** Signing scheme, in-process opt-in wording, worker restart and
   quarantine thresholds, and whether a first-party native plugin may ship before 1.0. Owner: **MS-21**.
5. **Storage lifetime.** Per-mod KV must live somewhere durable, and [17](17-persistence.md) §3.1 does
   not yet place it: inside the save for portability, or beside it under `mods/<namespace>/kv/`
   (recommended) with the world's lifecycle? Owner: **MS-13**.
6. **UI binding protocol ownership.** The `ui_bind` value model is shared with [13](13-ui-and-ux.md);
   one owner for the schema and its 10 Hz update rule is needed. Execution is settled (host-only,
   DEC-049), so what is left is schema ownership, not a client ABI. Owner: **MS-05** with MS-09.
7. **Network egress limits.** 32 KiB/s per player is a first guess; measure against a minigame and a
   map-sync mod before fixing it in the protocol. Owner: **MS-06** with MS-09.
8. **Deprecation window.** Two minors or six months is the proposal; confirm whether a twelve-month
   parallel-major window is affordable for the host maintenance budget. Owner: **MS-09**.
9. **Catalog and signing keys.** Shared with [15](15-content-system.md) Q9: who holds the keys, and is
   mod signing mandatory for the public catalog? Owner: **MS-16**.

## 9. References

- Decisions: [DEC-003](../decisions.md), [DEC-007](../decisions.md), [DEC-008](../decisions.md),
  [DEC-019](../decisions.md), [DEC-020](../decisions.md), [DEC-023](../decisions.md),
  [DEC-027](../decisions.md), [DEC-035](../decisions.md), [DEC-040](../decisions.md),
  [DEC-042](../decisions.md), [DEC-043](../decisions.md), [DEC-049](../decisions.md).
- Chapters: [00](00-conventions.md), [01](01-technical-architecture.md), [02](02-engine-core.md),
  [05](05-world-model.md), [06](06-worldgen.md), [08](08-gameplay-survival.md),
  [09](09-entities-ai.md), [11](11-inventory-crafting-items.md), [13](13-ui-and-ux.md),
  [15](15-content-system.md), [18](18-networking-multiplayer.md), [19](19-server-administration.md),
  [20](20-security-and-anti-cheat.md), [25](25-performance-budgets.md).
- Research: [modding and content system](../research/06-modding-and-content-system.md) §3 (execution
  models, Wasmtime configuration, quotas, WIT sketch), §7 (distribution, permissions, guards),
  §8 (recommended architecture); [custom content and differentiation](../research/10-custom-content-and-differentiation.md)
  §7 (platform services and the "no client code" rule).
- External: wasmtime — <https://docs.wasmtime.dev/> (epoch interruption, fuel metering,
  deterministic execution, `StoreLimits`); WebAssembly component model and WIT —
  <https://component-model.bytecodealliance.org/>; `wasm-tools` and `wit-bindgen` —
  <https://github.com/bytecodealliance/wasm-tools>, <https://github.com/bytecodealliance/wit-bindgen>;
  WASI 0.2 — <https://wasi.dev/>.
- Milestones: [MS-09, MS-21](../plan/01-milestones.md); [roadmap](../plan/00-roadmap.md).

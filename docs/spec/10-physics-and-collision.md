# 10 — Physics and Collision

> Status: normative - Last updated: 2026-11-24
> Depends on: [DEC-004](../decisions.md), [DEC-007](../decisions.md), [DEC-008](../decisions.md), [DEC-021](../decisions.md), [DEC-029](../decisions.md), [DEC-035](../decisions.md), [DEC-038](../decisions.md), [DEC-039](../decisions.md), [DEC-045](../decisions.md); [conventions](00-conventions.md), [world model](05-world-model.md), [survival](08-gameplay-survival.md), [entities and AI](09-entities-ai.md), [redstone](12-redstone-and-logic.md), [content system](15-content-system.md), [networking](18-networking-multiplayer.md), [anti-cheat](20-security-and-anti-cheat.md)
> Implements: MS-03 (player movement and collision), MS-10 (fluid flow, falling blocks, TNT), MS-11 (boats, minecarts, projectiles, piston interplay), MS-15 (movement validation input)

## 1. Purpose and scope

This chapter specifies the deterministic voxel physics of OpenBlock: the collision model, block collision shapes, broadphase and raycasts, motion integration and resolution, step-up and sneaking, ladders, fluids, elytra, entity pushing, projectiles, boats and minecarts, falling blocks, TNT, the piston interface, fluid flow, the collision query API exposed to mods, and the determinism and anti-cheat contracts around all of it. It is the contract between `ob_world`, `ob_sim`, `ob_client` prediction and [20](20-security-and-anti-cheat.md).

There is **no rigid-body engine**. A general rigid-body solver brings contact manifolds, sleeping islands, constraint iteration counts and solver-order nondeterminism that a voxel game does not need and cannot reconcile across client and server. Every body is an axis-aligned box swept against the voxel grid, with a small number of constrained movers (boats, minecarts, passengers) layered on top.

Out of scope: world storage, sections and block states ([05](05-world-model.md)); damage arithmetic, health, hunger and knockback-to-damage conversion ([08](08-gameplay-survival.md)); entity brains, spawning and projectiles' damage ownership ([09](09-entities-ai.md)); fluid and water rendering ([07](07-light-and-weather.md)); redstone power, update ordering, piston block movement and TNT ignition ([12](12-redstone-and-logic.md)); wire encoding and prediction/reconciliation ([18](18-networking-multiplayer.md)); enforcement policy and evidence ([20](20-security-and-anti-cheat.md)).

Five rules frame the design. **Same solver everywhere.** Client prediction, integrated host and dedicated server run this code; only authority differs ([DEC-008](../decisions.md)). **Shapes are integer.** Collision geometry is authored in 1/16-block units and converted exactly to `f64`, so a shape never depends on a compiler's float rounding. **Continuous where it matters.** Entity motion uses bounded sub-steps; projectiles use continuous voxel traversal, so speed cannot tunnel. **One fixed order.** Axis order, neighbour order, pair order and tie-breaks are specified, not incidental. **Space is `f64`, presentation is `f32`.** Gameplay positions and velocities are `f64` with pinned transcendentals ([DEC-021](../decisions.md), [DEC-039](../decisions.md)); `f32` exists only in the renderer.

## 2. Requirements

| ID | Requirement | Verification |
| --- | --- | --- |
| REQ-PHYS-001 | One solver implements all body motion; no external rigid-body engine, no per-body constraint iteration count, and the same code runs in prediction, integrated host and dedicated server. | Dependency scan + session-parity test ([01](01-technical-architecture.md)) |
| REQ-PHYS-002 | Entity motion is an AABB sweep resolved on separate axes in the fixed order **Y, then X, then Z**; the order is documented here, covered by golden traces and never varied by body or platform. | Code inspection + golden trace |
| REQ-PHYS-003 | A body advances by at most `SUBSTEP = 0.25 m` per sweep step and at most 16 steps per axis per tick; speed is clamped to `TERMINAL = 78.4 m/s`. | Boundary tests + fuzz |
| REQ-PHYS-004 | Collision shapes derive from block states as unions of axis-aligned boxes in 1/16-block integer units, canonically merged to at most 8 boxes per shape. | Shape validator + table inspection |
| REQ-PHYS-005 | Full-cube and empty shapes are fast paths; a collision query allocates nothing and returns borrowed or fixed-capacity data. | Allocation counter + code review |
| REQ-PHYS-006 | Broadphase skips sections with `non_air == 0`, visits cells in key order, and caps a single sweep at 4,096 cell tests before splitting or returning `Budget`. | Bench + saturation fixture |
| REQ-PHYS-007 | Raycasts use voxel DDA with per-shape box tests, a 512 m cap, and a documented tie-break (smaller `tMax`, then X < Y < Z). | `raycast` conformance table |
| REQ-PHYS-008 | Step-up is at most 0.6 m, attempted at most three times, and succeeds only if the body can stand after the lift. | Stair/slab scenario suite |
| REQ-PHYS-009 | Sneaking prevents walking off a ledge: a horizontal move that would leave no support within 0.05 m is cancelled. | Edge-walk conformance |
| REQ-PHYS-010 | Ladders, vines and scaffolding provide a 2.35 m/s climb, suspend gravity while climbing and expose a slide when the climb input is released. | Climb scenario |
| REQ-PHYS-011 | Water and lava use a submersion fraction sampled at nine points; buoyancy, drag, swim and lava sink follow §3.4; waterlogged blocks count as fluid. | Medium fixtures + replay |
| REQ-PHYS-012 | Elytra flight uses the kinematic glide of §3.4 with base 30 m/s, dive cap 67.2 m/s and firework thrust; collision ends gliding. | Elytra scenario + golden trace |
| REQ-PHYS-013 | Boats and minecarts are constrained movers: boats use buoyancy, paddle thrust and land friction; minecarts follow the rail graph, powered rails and curve limits. | Vehicle conformance maps |
| REQ-PHYS-014 | Entity-entity collisions resolve in `(spawn_seq, spawn_seq)` order, at most two passes and 0.4 m of relative correction per tick, and every pushed body is re-checked against blocks so a push cannot enter a wall. | Push stress + no-penetration fuzz |
| REQ-PHYS-015 | Projectiles integrate by continuous voxel DDA for blocks and swept AABB for entities, choose the earliest hit with a documented tie-break, ignore the owner for 10 ct, and are host-resolved. | Projectile corpus + two-client test |
| REQ-PHYS-016 | A falling block is removed in the same transaction that creates its entity, and landing either places the block or drops the item exactly once. | Kill-mid-fall + duplication test |
| REQ-PHYS-017 | TNT fuses, explosion ray geometry and entity exposure follow §3.8; block destruction is applied by [12](12-redstone-and-logic.md) and damage by [08](08-gameplay-survival.md). | Explosion golden fixture |
| REQ-PHYS-018 | Piston moves never leave an entity intersecting a moved block: entities are displaced, ride or take crush damage in one transaction, ordered by `spawn_seq`. | Piston/entity matrix |
| REQ-PHYS-019 | Fluid flow follows §3.9: level semantics, downward priority, source formation, fluid-fluid reactions, delays and per-tick budgets; rendering is [07](07-light-and-weather.md). | Fluid conformance + 24 h soak |
| REQ-PHYS-020 | The mod collision API is read-only, bounded per call and per tick, returns borrowed data, and requires the `world.read` capability on the host. | ABI tests + malicious-mod corpus |
| REQ-PHYS-021 | Simulation math is `f64` in a fixed operation order with pinned `sin`/`cos`/`tan`/`atan2`; no `mul_add`, SIMD reduction, fast-math or platform transcendental (`sin`, `cos`, `tan`, `atan2`, `powf`, `exp`) enters a gameplay result; `sqrt`, `floor`, `ceil` and `abs` are exact IEEE-754 operations. | Cross-platform replay hash |
| REQ-PHYS-022 | Non-finite positions or velocities are rejected at the boundary and after every axis; the body restores its last valid position, zeroes velocity and the offending axis, and increments a counter instead of panicking. | NaN/Inf fuzz |
| REQ-PHYS-023 | Contact and landing events are emitted exactly once per tick per body, and fall distance is accumulated by physics and consumed by [08](08-gameplay-survival.md). | Event-count test |
| REQ-PHYS-024 | The host produces a `MoveTrace` for every simulated input; [20](20-security-and-anti-cheat.md) validates speed, acceleration, collision, noclip and reach from that trace. | Cheater corpus |
| REQ-PHYS-025 | The reach and line-of-sight queries used by combat and interaction are the same shape-aware raycasts, with the same caps and tie-breaks. | Cross-chapter query test |
| REQ-PHYS-026 | Client prediction runs this solver on the prediction overlay and never writes authoritative positions; a correction converges within two ticks. | Prediction/reconciliation tests |
| REQ-PHYS-027 | Content collision shapes are validated at freeze (coordinates, box count, merge result); a hot reload rebuilds the shape table between ticks and unsticks any body left inside a new solid shape. | Shape validator + reload test |
| REQ-PHYS-028 | Fluid and block updates are scheduled in `(due_tick, packed block key, kind)` order with no persisted sequence; physics never runs inside a fluid or redstone update, and entity movement never interleaves with scheduled ticks. | Cross-system ordering test |
| REQ-PHYS-029 | No tunnelling is observable: after every tick a body's AABB penetrates no solid shape by more than 1e-6 m, at every supported speed and shape thickness. | Fuzz invariant + reference solver |
| REQ-PHYS-030 | `/debug physics` and metrics expose body count, sweep and cell counts, ray count, fluid queue depth, recovery and `Budget` counters, and per-system timing. | Overlay + metrics test |

## 3. Design

### 3.1 Model and coordinate conventions

All simulation positions are `WorldPos` (`f64`, metres), velocities are `Vec3` (`f64`), and the tick advances by exactly `dt = 0.05 s` at 20 Hz ([DEC-038](../decisions.md)). A body is an axis-aligned box with a position, a velocity and a yaw; rotation is used for movement direction and projectile spread, never for the collision box, except for vehicle bodies, whose sweep box is rotated as a constrained approximation. Collision boxes are axis-aligned in world space; a diagonal fence or a sloped rail is represented by the boxes of its block state, not by an oriented box.

| Constant | Value | Notes |
| --- | ---: | --- |
| `TICK_DT` | 0.05 s | fixed; never wall-clock |
| `GRAVITY` | 32.0 m/s² | parity-shaped fall feel; jump 8.4 m/s gives ≈ 1.10 m |
| `TERMINAL_VELOCITY` | 78.4 m/s | clamp on every body |
| `GROUND_ACCEL` / `GROUND_FRICTION` | 40 / 30 m/s² | toward target speed / no input |
| `AIR_DRAG` | 0.98 per tick | multiplicative |
| `STEP_HEIGHT` | 0.6 m | [08 §3.7](08-gameplay-survival.md#37-movement-abilities) |
| `SNEAK_EDGE_MARGIN` | 0.05 m | support test inset |
| `SUBSTEP` / `MAX_SUBSTEPS` | 0.25 m / 16 | per axis |
| `WATER_BUOYANCY`, `WATER_DRAG` | 30.0 m/s², ×0.80/tick | net sink 2.0 m/s² at full submersion |
| `LAVA_BUOYANCY`, `LAVA_DRAG` | 16.0 m/s², ×0.50/tick | net sink 16.0 m/s² |
| `SWIM_UP` | 2.2 m/s | [08 §3.7](08-gameplay-survival.md#37-movement-abilities) |
| `CLIMB_SPEED` | 2.35 m/s | ladder, vine, scaffolding |
| `PROJECTILE_MAX_SPEED` | 100 m/s | content may lower it |
| `COLLISION_EPS` | 1e-7 m | separation after a contact |

The solver is a *discrete* simulation: it does not integrate a continuous trajectory and then search for the first impact. It moves a body in bounded increments and stops at the first shape overlap, which is cheaper, easier to make deterministic and sufficient at 20 Hz. Continuous traversal is used where the discrete model would tunnel: raycasts and projectiles. The two models share the same shape table and the same block queries.

### 3.2 Collision shapes from block states

A block state maps to a `ShapeId` at registry freeze; the shape table stores collision, outline and support geometry. Fluid blocks have an empty collision shape, waterlogged blocks keep their block shape, and non-solid blocks such as plants, torches and rails have empty collision (their outline shape is separate). A block type declares one of three shape sources:

| Source | Used by | Representation |
| --- | --- | --- |
| `full_cube` | stone, dirt, planks, most ores | `ShapeId(0)`, no boxes stored |
| `boxes` | slabs, stairs, fences, walls, panes, carpets, snow, custom content | 1–8 boxes in 1/16 units, canonicalised at freeze |
| `empty` | air, fluids, plants, torches | `ShapeId(1)`, no collision |

```rust
pub struct ShapeBox { pub min: [u8; 3], pub max: [u8; 3] } // 0..=16, min < max
pub struct ShapeBounds { pub min: [f32; 3], pub max: [f32; 3] } // broadphase only
pub struct VoxelShape {
    pub boxes: [ShapeBox; 8], pub len: u8,
    pub bounds: ShapeBounds,           // union in block-local metres
    pub solid: bool,                   // full cube fast path
    pub climbable: bool,               // ladder, vine, scaffolding
    pub fluid: bool,                   // empty collision, fluid source
}
pub struct ShapeTable { shapes: Box<[VoxelShape]>, state_to_shape: Box<[ShapeId]> }
```

**Authoring.** A content shape is a list of boxes with integer corners `0..=16`; rotation, mirroring and connection variants are generated per block state at freeze, so runtime never transforms a box. Stairs and slabs therefore have one shape per state; fences, walls and panes have a `[ShapeId; 16]` variant table indexed by a 4-bit connection mask computed from the four horizontal neighbours. The connection mask is derived at query time (four block reads plus a table index), not stored as a block-state property, which respects the derived-property rule of [05 §3.4](05-world-model.md#34-block-states) without allocating.

**Canonicalisation.** Boxes are sorted by `(min.y, min.x, min.z, max.y, max.x, max.z)`, then merged greedily when two boxes share a full face and their union is itself a box. Merging repeats until stable. A full cube collapses to `ShapeId(0)`; an empty list is `ShapeId(1)`; a result above 8 boxes is a content error naming the block and state. The table holds at most 2,048 unique shapes (≤ 256 KiB excluding the dense state map); overflow is a pack error, not a runtime fallback. Shapes are immutable for a session; a hot reload builds a new table and swaps it between ticks ([REQ-PHYS-027](#2-requirements)).

**Shape queries.** `shape_of(BlockStateId, connection_mask) -> ShapeId`; `collision_boxes(ShapeId) -> &[ShapeBox]`; `outline` and `support` are separate tables so a slab can support an entity on its top face while its outline is the full selection box. The common case is one state → one shape and one box; a full cube test is a boolean, not a loop.

### 3.3 Broadphase and raycasts

**Swept AABB.** For a body box `b` and a per-axis delta, expand to `swept = b.union(b.translate(delta))`, shrink by `COLLISION_EPS`, compute the overlapped section range, and iterate sections in ascending `(sx, sy, sz)` key. Skip sections whose `non_air == 0` ([05 §3.11](05-world-model.md#311-world-queries)). Within a section, iterate the block cells intersecting `swept` in `(y, z, x)` order and test only shapes whose `bounds` intersect the swept box. The candidate list is built once per body per tick and refreshed when the body enters a new section; sub-steps reuse it. A sweep is capped at 4,096 cell tests; if the cap is hit, the delta is split in half and the sweep retried (at most four splits), otherwise `Budget` is returned and the body keeps its last valid position for that axis.

**Raycast.** `raycast(origin, dir, max, shape_kind, filter)` traverses voxel cells with the Amanatides–Woo DDA ([references](#9-references)). For each cell it reads the state, resolves its shape (including the connection mask for fences), and intersects the ray with each box, expanding by `COLLISION_EPS`. The nearest hit wins; ties resolve by smaller `tMax`, then X < Y < Z, matching [05 §3.11](05-world-model.md#311-world-queries). The traversal stops at `max` (512 m for gameplay queries, 3–5 m for interaction reach), at the first chunk below `Ready` when `stop_at_unloaded` is set, and returns `RayHit { block, face, point, distance, shape, box_index, hit_fluid }`. A ray over 512 m is a programming error in a gameplay path; tools may raise the cap.

**AABB query.** `overlap(aabb, shape_kind, filter)` expands by the largest shape bound, iterates sections and cells in key order, and yields `(BlockPos, ShapeId, box_index)` lazily in `(section key, local index, box index)` order. It is allocation-free; the caller supplies a fixed buffer or a closure. It is the primitive behind entity pushing, block-placement checks, fluid source scans and mod queries.

### 3.4 Motion integration, step-up and media

```rust
fn step_body(b: &mut Body, input: &MoveIntent, world: &dyn PhysicsQuery, dt: f64) {
    if !b.is_finite() { b.recover_last_valid(); return; }
    let medium = sample_medium(b.aabb(), world);       // Air | Water | Lava + submersion 0..9
    let sub = medium.submersion as f64 / 9.0;
    apply_input(b, input, medium);                     // target horizontal velocity
    b.vel.y -= GRAVITY * dt;
    match medium.kind {
        Water => b.vel.y += WATER_BUOYANCY * sub * dt,
        Lava  => b.vel.y += LAVA_BUOYANCY  * sub * dt,
        Air   => {}
    }
    apply_drag_and_friction(b, medium, dt);
    clamp_speed(&mut b.vel, TERMINAL_VELOCITY);
    let delta = b.vel * dt;
    for axis in [Axis::Y, Axis::X, Axis::Z] {
        sweep_axis(b, axis, delta[axis], input, world);
    }
    try_step_up(b, input, world);                      // after the three axes
    update_flags_and_fall_distance(b, medium);
}
```

**Medium sampling.** The body's AABB is sampled at its eight corners and centre; a sample counts as water when the cell is water or waterlogged, and as lava when it is lava. `submersion = fluid_samples / 9` in `{0, 1/9, …, 1}`. Buoyancy and drag scale by submersion, so a body leaving water loses lift continuously rather than snapping between states. The fluid at the eye determines the underwater overlay; that is presentation and lives in [07](07-light-and-weather.md).

**Input.** A player's wire `MoveInput` and a mob's [09 §3.6](09-entities-ai.md#36-movement-interface) intent both become this `MoveIntent`. Horizontal target speed is the mode's walk, sprint, sneak or swim speed ([08 §3.7](08-gameplay-survival.md#37-movement-abilities)), scaled by 0.5 in water and 0.25 in lava. Ground acceleration approaches the target at 40 m/s²; with no input, friction decelerates at 30 m/s² down to zero. Air control uses a quarter of ground acceleration and `AIR_DRAG` per tick. Jump sets `vel.y = 8.4 m/s` only when `on_ground`; there is no coyote time and no buffered jump, which matches the parity feel and keeps the rule one line long. The jump-boost effect adds 0.4 m/s per level, capped so the vertical speed never exceeds terminal.

**Axis sweep.** For each axis, the body moves in `SUBSTEP` increments. On the first overlap it stops at `hit.t * step - sign(step) * COLLISION_EPS`, zeroes that axis velocity, records a `Contact { block, face: axis, shape }` and stops. A downward Y contact sets `on_ground` and resets fall distance; an upward Y contact zeroes upward velocity (head bump); horizontal contacts set `collided_x` or `collided_z`. The order Y → X → Z is fixed: resolving X before Z means a body sliding into a corner stops on X and continues on Z, which is observable and tested, and is why the solver is not X/Z symmetric (§7).

**Step-up.** After the three axes, if the body is grounded and a horizontal axis was blocked, the solver tries up to three lifts. It measures the free height above the current box (≤ `STEP_HEIGHT`), sweeps up, re-runs the blocked horizontal axis, then sweeps down by the lift plus `COLLISION_EPS`. It keeps the result only if the body ends supported; otherwise it restores the pre-lift position. The three attempts cover a slab (0.5), a stair (0.5) and a snow layer (0.125) without a special case per block. Step-up never applies while sneaking, gliding, in a fluid or riding.

**Sneaking edge guard.** For a horizontal axis, before moving, the solver offset-tests the support under the destination box inset by `SNEAK_EDGE_MARGIN` on the leading edge. If no collision shape intersects the support probe within 0.05 m below, the axis move is cancelled and the body stays at the edge. The guard applies only when grounded and not in a fluid; it is a gameplay rule, not a collision shape, and is replicated as the `sneaking` flag so the client predicts the same edge.

**Ladders and climbing.** If the body overlaps a `climbable` shape and `input.climb` is set, vertical velocity is driven toward `+2.35 m/s`; sneak while climbing drives it toward `-2.35 m/s` and overrides climb when both are set. Gravity is suspended and horizontal velocity is damped by 0.5 per tick. Without input, the body slides down at `-0.5 m/s`. Leaving the climbable shape restores gravity on the next tick. Scaffolding uses the same path with a content flag for its top surface.

**Fall distance.** Physics accumulates `fall_distance += max(0, -delta.y)` while the body is airborne and not in a fluid, climbing or gliding. Landing, teleport, dimension transfer, mounting and entering water reset it to zero. [08](08-gameplay-survival.md) converts the distance to damage; physics never applies fall damage itself.

**Elytra glide.** Gliding is a kinematic model, not aerodynamic simulation:

```text
dive      = max(0, -sin(pitch))
target_h  = 30 + 37.2 * dive                       // 30 m/s level, 67.2 m/s at -90°
v_h      += (target_h - v_h) * 0.05               // per tick
v_y      += (v_h * sin(pitch) - 2.0 * (1 - dive)) * 0.1
v_y      += 0.8 * firework_ticks_remaining / 20   // per tick while boosted
clamp v_y to [-67.2, 67.2]
```

A collision with a block or entity ends gliding and zeroes the vertical velocity; the body then falls normally. Firework boost is 20 ct and is consumed by content. Elytra durability is [11](11-inventory-crafting-items.md), not physics. The constants are base-profile data; the shape of the model is normative.

### 3.5 Entity-entity collisions and pushing

Entity bodies collide softly. After all bodies have moved, pairs from the 16 m spatial hash ([09 §3.5](09-entities-ai.md#35-storage-iteration-and-queries)) whose AABBs overlap are resolved in ascending `(spawn_seq_a, spawn_seq_b)` order, two passes maximum. Each body's inverse mass comes from the definition's mass ([15 §3.4](15-content-system.md#34-registry-catalogue)), clamped to 1/1,000..1,000 so a content error cannot create an infinite push: a player is 70 kg, a small mob 8 kg, a large mob 160 kg, a boat 400 kg and a minecart 300 kg in the base pack. The correction is split by weight, capped at 0.4 m of relative displacement per tick, and applied along the axis of least penetration, with ties broken by Y, X, Z. Projectiles, item entities and display entities do not push.

After a push, each moved body is swept against blocks; if the destination overlaps a solid shape, the push is reverted for that body, the pair is left overlapping and `push_blocked` increments. An overlapping pair is retried next tick; it does not jitter or teleport. This is the rule that stops a mob from pushing a player through a wall and stops a boat from being shoved into terrain. Two players pushing each other are symmetric: both receive half the correction, so neither has authority over the other's position. A player standing on a boat is a passenger, not a colliding body; the seat transform owns the position ([09 §3.14](09-entities-ai.md#314-projectiles-vehicles-and-falling-blocks)).

### 3.6 Projectiles and hit detection

```rust
pub struct ProjectileBody {
    pub pos: WorldPos, pub vel: Vec3, pub radius: f64,
    pub gravity: f64, pub drag: f64,
    pub owner: Option<NetId>, pub ignore_owner_ct: u16,
    pub pierce: u8, pub stuck: Option<StuckAt>, pub age_ct: u32,
}
```

Per tick the host advances the projectile along the segment `p → p + v·dt`. **Blocks:** a continuous voxel DDA over the expanded segment (radius as an AABB inflation) returns the earliest shape hit; a shape thinner than the segment cannot be missed, so a 100 m/s arrow cannot tunnel through a 1/16 m carpet. **Entities:** candidates within the segment's expanded bounds are swept as AABBs inflated by `radius`; the earliest `t` wins, ties break by `(t, spawn_seq)`. Block and entity hits are compared; the smaller `t` resolves. A hit on the owner is ignored for `ignore_owner_ct`; a friendly-fire flag on the projectile filters players on the same team (content). On a hit, physics computes `point`, `normal`, `hit_entity` and calls [08](08-gameplay-survival.md) once; pierce decrements and continues from `point + normal * 1e-4`, otherwise the projectile becomes `stuck` at `point - normal * 0.01` with zero velocity. Stuck projectiles despawn after 600 ct, in-flight ones at 1,200 ct.

Projectiles do not load chunks. A segment that would enter a chunk below `Ready` stops at the last `Ready` boundary and the projectile freezes; it despawns after 100 ct if the chunk never loads. The client never sends a hit; it renders a tracer from the replicated spawn and the impact from the replicated despawn, so a cheating client cannot create damage ([REQ-ENTITY-019](09-entities-ai.md#2-requirements)).

### 3.7 Boats, minecarts and passengers

**Boats.** A boat is a constrained body with yaw, buoyancy, paddle thrust and high land friction. In water it floats at the surface: buoyancy cancels gravity while the box is at least half submerged, horizontal thrust is 2.0 m/s² per input direction and drag is ×0.90 per tick, with a maximum speed of 8 m/s. On land it moves with ×0.60 friction and no thrust, so it slides and stops. A boat takes fall damage above 3 m and breaks above 8 m/s impact into a solid block, dropping its item and ejecting passengers to a validated free spot. It cannot climb a full block; step-up is disabled for vehicle bodies.

**Minecarts.** A minecart is a point constrained to the centreline of its rail block state ([12](12-redstone-and-logic.md) owns rail shape and powered-rail power). Its state is a scalar speed along the rail tangent, a direction and a rail position. Powered rail accelerates at 0.5 m/s², friction is ×0.996 per tick, gravity contributes `GRAVITY * slope` on sloped rails, and the cart is clamped to 8 m/s on straight rail and 12 m/s on a curve before it derails (derailing means becoming a free body with the same velocity, then normal collision). At a rail junction the cart takes the branch selected by its incoming direction and the redstone state read from [12](12-redstone-and-logic.md); ties take the lowest `(x, z)` branch, a documented divergence from update-order-dependent behaviour. Curves snap the cart to the centreline each tick, so a cart cannot cut a corner through a wall.

**Passengers.** A passenger's collision is disabled; its position is the seat transform, and its look is its own. Dismount checks a free 2-block space at the dismount offset and is refused otherwise. A vehicle that enters an unloaded chunk freezes at the boundary and is saved with its passengers' UUIDs; it never transfers passengers to a different vehicle.

### 3.8 Falling blocks, TNT and pistons

**Falling blocks.** A gravity block that loses support is converted by [12](12-redstone-and-logic.md) into a `falling_block` request. Physics removes the block and creates the entity in one `WorldEdit` transaction: the block's state is removed first, the entity is inserted in the same batch, and a failure rolls the batch back, so a crash cannot duplicate the block. The entity falls with gravity 32 m/s² and `AIR_DRAG`; on landing, if the destination is loaded, `Ready` and replaceable, the host places the state and removes the entity in one transaction. If the destination is occupied, it becomes an item entity of the block's item form through [11](11-inventory-crafting-items.md). Falling blocks cannot be pushed by pistons; a piston move applies to the block before it becomes an entity.

**TNT.** Primed TNT is an entity with an 80 ct fuse (content), gravity and a hiss event. On fuse end the host evaluates the explosion before any block is destroyed: 1,352 fixed Fibonacci-sphere directions, a step of 0.3 m, and a radius of `2 × power` metres. Each ray records its first block with non-empty collision and its resistance, and the set of blocks whose `power × (1 − distance/radius)` exceeds resistance becomes the break list, sorted by `(distance, block key)` and capped at 4,096 entries. Entity exposure is the fraction of rays whose segment intersects the entity's AABB before the first block hit, in per-mille. The result `ExplosionResult { blocks, exposures, chained_tnt }` is handed to [12](12-redstone-and-logic.md) for block edits and ignition and to [08](08-gameplay-survival.md) for damage; physics owns geometry only. Chain reactions are queued with a delay of `10 + mix64(spawn_seq) mod 21` ct, so the fuse is random-looking but replayable. No block is destroyed twice and no entity takes damage twice in one explosion.

**Pistons.** [12](12-redstone-and-logic.md) owns the block-move transaction (push limit, sticky behaviour, slime/honey, immovable blocks). Physics exposes two operations that the piston transaction calls around its commit:

```rust
pub enum DisplacePolicy { Push, Crush, Ignore }
pub struct Displaced { pub entity: NetId, pub from: WorldPos, pub to: WorldPos, pub crushed: bool }
pub fn plan_displace_entities(world: &World, moved: Aabb, delta: Vec3,
    max_blocks: u8, policy: DisplacePolicy) -> Result<SmallVec<[Displaced; 16]>, PistonBlocked>;
pub fn settle_entities(world: &mut World, moved: Aabb, policy: DisplacePolicy);
```

`plan_displace_entities` is a pure plan: entities intersecting `moved` are translated by `delta` if their destination is free; if blocked, the plan tries the free faces in the canonical order owned by [12 §3.5.2](12-redstone-and-logic.md#352-pistons) — **−Y, −X, +X, −Z, +Z, +Y** — and takes the first one that is free. That is the same list redstone uses for crush displacement, not a nearest-facing search; "nearest" is not a tie-break, which is why the two chapters quote one list rather than two similar ones. A fixture with two free faces asserts which destination is chosen, so a reordering cannot hide behind the word "nearest". If no face is free, `Push` and `Ignore` return `PistonBlocked`, while `Crush` (opt-in; the base pack aborts) plans damage through [08](08-gameplay-survival.md). The piston transaction applies the block batch and the entity plan in one commit, so an abort changes neither blocks nor entities, preserving the all-or-nothing rule of [12 §3.5.2](12-redstone-and-logic.md). After the commit, `settle_entities` re-checks the moved volume for bodies that entered it during the same tick (for example a player stepping into the head) and pushes them out or applies crush damage; that is a post-commit correction, not a rollback. A piston can move an entity at most 12 blocks, matching the push limit. Passengers move with their vehicle as one unit; a piston cannot separate a rider from its seat.

### 3.9 Fluid flow

Fluid state is the block-state encoding of [05 §3.4](05-world-model.md#34-block-states): `level ∈ 1..=7` is a flowing fluid, `level = 8` is a source, and `falling` marks a falling column. A level-0 fluid block is invalid and is removed on load or tick. A cell with no fluid has no block; a waterlogged block reports a source to fluid queries but is never replaced by flow. Rendering of water surfaces, fog and underwater effects is [07](07-light-and-weather.md); this section owns flow rules and scheduling.

**Flow rule.** When a fluid block is ticked:

1. If it is a **source** and not falling: water spreads downward into a replaceable cell below as a falling column (`level = 8`, `falling = true`, not a renewable source), and it also spreads horizontally to replaceable cells with `level = 7`; when the cell below is not replaceable it spreads only horizontally. Lava uses the same rule but stops at level 5, giving three horizontal steps instead of seven.
2. If it is **flowing** or **falling**: if the cell below is replaceable, it continues falling with `level = 8, falling = true`; when it lands it becomes a flowing block at `level = 7` in its own cell and spreads horizontally from there. A non-falling flowing block raises its own level to `max(own, max_horizontal_neighbour_level − 1)` and removes itself below the fluid's minimum level (1 for water, 5 for lava).
3. A flowing water block at `level = 7` with at least two horizontal **non-falling source** neighbours becomes a source (renewable water); falling columns do not count. Lava never creates a source this way.
4. Flow replaces only replaceable blocks (air, plants, snow layers, fire); it does not replace a waterlogged block, a solid block or another fluid source.

**Reactions.** When water and lava meet, the block result is content data: a water source adjacent to a lava source produces obsidian; flowing water against a lava source produces cobblestone; flowing lava into water produces stone. The reaction is a single block edit with cause `Fluid`, scheduled before the next flow tick and applied in `(distance, block key)` order.

**Scheduling and budgets.** Fluid updates enter the chunk's `TickQueue` ordered by `(due_tick, packed block key)` with a delay of 5 ct for water, 30 ct for lava in the overworld and 10 ct in the underworld, from content; the fluid `kind` is part of the entry, and because a cell holds only one fluid kind this order is equivalent to the canonical `(due_tick, key, kind)` of [05 §3.7](05-world-model.md#37-scheduled-ticks). There is no sequence number, so equal-due updates order by position and load in the same order they were saved. The host drains at most 512 fluid updates per tick and keeps at most 4,096 pending; when the cap is reached, entries for the same position coalesce to the latest state, so a flood degrades to slower spreading rather than an unbounded queue. An update schedules at most six successors; no fluid update applies immediately, which is what prevents recursive floods and tick spikes. Fluid updates never interleave with entity movement ([REQ-PHYS-028](#2-requirements)); the cross-system order is owned by [12](12-redstone-and-logic.md).

**Source and duplication safety.** A source is created only by the two-neighbour rule; a flowing block and a falling column never create a source by themselves. A fluid break that would create a new source is checked against the block state and the chunk status before the edit. The conformance corpus includes an infinite-water fixture, a lava/water cobblestone generator, a waterlogged-slab break and a 1,000-block flood; each asserts a bounded block count and a stable hash after quiescence.

### 3.10 Collision query API for mods

```rust
pub trait PhysicsQuery {
    fn sweep(&self, aabb: Aabb, delta: Vec3, filter: QueryFilter) -> Option<SweepHit>;
    fn overlap(&self, aabb: Aabb, out: &mut [BlockPos], filter: QueryFilter) -> Result<usize, QueryError>;
    fn raycast(&self, origin: WorldPos, dir: Vec3, max: f64, filter: QueryFilter) -> Option<RayHit>;
    fn fluid(&self, pos: BlockPos) -> FluidKind;
}
```

The API is read-only and host-side: a WASM mod needs the `world.read` capability ([16](16-modding.md)) and a native plugin is operator-trusted. 1.0 clients run no mod or WASM code at all ([DEC-049](../decisions.md)), so there is no client-side query path to leak occupancy; a post-1.0 client code tier would remain presentation-only, consistent with the visual-only rule for shader packs ([DEC-045](../decisions.md)). `overlap` writes into a caller buffer and returns the number written or a `QueryLimit` when the buffer is too small; `sweep` and `raycast` return fixed-size values. Each mod gets 1,000 queries per tick, 256 results per `overlap`, and 512 m maximum ray length; exceeding a quota disables the query, logs the mod and does not affect the simulation. No query returns a mutable world handle, a block entity, or another entity's state.

### 3.11 Determinism and numeric policy

Gameplay physics is `f64` and follows [DEC-039](../decisions.md) and [DEC-021](../decisions.md):

1. **Fixed operation order.** Velocity update, drag, clamp, Y sweep, X sweep, Z sweep, step-up, flags. No parallel reduction inside a body; bodies may be processed in parallel only when results are gathered in `spawn_seq` order.
2. **Pinned functions.** `sin`, `cos`, `tan`, `atan2`, `powf` and `exp` come from `ob_math`'s pinned implementation; `sqrt`, `floor`, `ceil` and `abs` are exact IEEE-754 operations and may use the platform. No `mul_add`, no fast-math, no vectorised min/max whose NaN semantics differ.
3. **No unordered iteration.** Sections, cells, shapes, contacts, pairs and fluid entries are visited in documented key order. Hash-map iteration is forbidden in the tick.
4. **Quantised hashing.** The state hash covers positions at 1/4,096 m, velocities at 1/4,096 m/s and rotations at 1/65,536 turn. A sub-quantum difference is still a bug if it changes a later decision, but it is not what the hash checks.
5. **Recovery.** A non-finite input or result restores the body's last valid position, zeroes velocity and the offending axis, records `physics_recovered` and logs once per body per second; it never panics. A body outside the border is clamped and counted.
6. **Coordinate precision.** At the 30 Mm border, `f64` has a ULP of 3.7 × 10⁻⁹ m ([05 §3.1](05-world-model.md#31-coordinate-spaces-and-conversions)); collision arithmetic never subtracts two far-apart `f64`s in `f32`. The renderer converts camera-relative deltas to `f32`; simulation never stores that value.

### 3.12 Prediction and anti-cheat interface

The client predicts its own body with this solver against its prediction overlay ([05 §3.6](05-world-model.md#36-the-mutable-world-api)). It sends inputs, never positions. The host replays the input on the authoritative body and records:

```rust
pub struct MoveTrace {
    pub input_seq: u32, pub tick: Tick,
    pub start: WorldPos, pub end: WorldPos,
    pub vel_before: Vec3, pub vel_after: Vec3,
    pub contacts: SmallVec<[Contact; 4]>,
    pub flags: MotionFlags,
    pub fall_distance: f64,
}
```

[20](20-security-and-anti-cheat.md) checks speed, acceleration, collision consistency, noclip (an end position inside a solid shape), fly (airborne without a jump/vehicle/glide source) and reach (a shape-aware raycast from eye to target). Because the trace comes from the same solver the client predicts with, a legitimate player's trace passes and a modified client's does not. A rejected trace corrects the client with the authoritative body state and does not replay the rejected move. Teleports and dimension transfers produce a `TeleportTrace` with a cause and are exempt from speed checks but not from destination validation.

### 3.13 Ordering with other systems

The physical tick order is: scheduled block and fluid ticks → entity effects and AI → entity motion and pushing → projectile integration → falling-block landing → block-change flush. Fluid and redstone scheduling is owned by [12](12-redstone-and-logic.md); this chapter only guarantees that a fluid update does not run inside a body's sweep and that entity movement does not interleave with scheduled ticks ([REQ-PHYS-028](#2-requirements)). A piston transaction asks physics to displace entities before and after its block move; a TNT explosion returns geometry to [12](12-redstone-and-logic.md) and [08](08-gameplay-survival.md) and applies nothing itself. A block edit that closes the space around a body is resolved by the next tick's `unstuck` pass: a 3×3×3 upward search for a free AABB, then a lateral ring search, then a freeze with a logged `stuck` counter. It never teleports a player to the world spawn.

## 4. Data structures and formats

### 4.1 Runtime structures

```rust
pub struct Body {
    pub pos: WorldPos, pub vel: Vec3, pub yaw: Yaw, pub hitbox: [f64; 3],
    pub last_valid: WorldPos, pub flags: MotionFlags,
    pub fall_distance: f64, pub medium: Medium, pub step_attempts: u8,
} // aabb() is pos + hitbox; recovery restores last_valid and zeroes velocity
pub struct SweepHit { pub t: f64, pub axis: Axis, pub block: BlockPos,
    pub shape: ShapeId, pub box_index: u8, pub point: WorldPos }
pub struct RayHit { pub block: BlockPos, pub face: Axis, pub point: WorldPos,
    pub distance: f64, pub shape: ShapeId, pub box_index: u8, pub hit_fluid: bool }
pub struct Contact { pub block: BlockPos, pub face: Axis, pub shape: ShapeId }
pub struct Medium { pub kind: MediumKind, pub submersion: u8 } // 0..=9 ninths
```

`Body` fits in 128 B (position, velocity, hitbox, last valid position, yaw, flags, fall distance, medium) and is stored in the `BodySet` SoA of [02 §3.7](02-engine-core.md#37-entity-store) and [09 §3.5](09-entities-ai.md#35-storage-iteration-and-queries). Vehicles add a `VehicleBody { kind, rail: Option<RailPose>, speed, seats }`; projectiles add the `ProjectileBody` of §3.6. Motion state is persisted inside the entity payload ([09 §4.2](09-entities-ai.md#42-save-record)); physics has no separate save file.

### 4.2 Shape table encoding

| Field | Size | Notes |
| --- | ---: | --- |
| `shape_count` | u16 | ≤ 2,048 |
| `box_count[i]` | u8 | 0 = empty, 1..=8 stored boxes; the full cube is `ShapeId(0)` and stores none |
| `boxes[i][j]` | 6 B | `min.xyz`, `max.xyz`, each `u8` in 0..=16 |
| `flags[i]` | u8 | bit0 full cube, bit1 climbable, bit2 fluid, bit3 support-only |
| `bounds[i]` | 24 B | two `f32` triples in block-local metres, broadphase only |
| `state_to_shape` | 2 B per state | dense `ShapeId` map built at freeze |

The bounds are `f32` for broadphase only; every exact test uses the integer boxes converted to `f64` by `n / 16`, which is exact because 16 is a power of two. The table is immutable for a session and swapped atomically on reload.

### 4.3 Fluid record and schedule

| Field | Size | Notes |
| --- | ---: | --- |
| `kind` | u8 | water or lava |
| `level` | u8 | 1–7 flowing, 8 source |
| `falling` | bool | falling column |
| `cause` | u8 | flow, reaction, neighbour, worldgen |
| `due_tick` | i64 | absolute tick; saved as a remaining delay against the payload's `saved_tick` |
| `block_key` | u64 | packed position; canonical tie-break within a tick |

A fluid entry is 24 B including padding (the four small fields precede the two 8-byte
fields, so there is one 4-byte pad), plus the queue node. Pending entries are `BTreeMap<(i64, u64), FluidTick>` per chunk, persisted with the chunk as delays ([17](17-persistence.md)); the same-position coalescing map is a transient `FxHashMap<BlockPos, u64>` holding the canonical key per position, rebuilt from the queue on load, so it is not persisted.

## 5. Performance budgets

Measured on the reference machine at 20 Hz (50 ms tick), p99 unless stated; these feed [25](25-performance-budgets.md).

| ID | Budget | Measurement |
| --- | --- | --- |
| PERF-PHYS-001 | One player movement + collision ≤ 25 µs p99; 50 players ≤ 1.25 ms/tick | `bench_player_physics` |
| PERF-PHYS-002 | 200 active entity bodies, excluding AI ≤ 1.2 ms/tick p99 | `bench_entity_physics_200` |
| PERF-PHYS-003 | 10,000 projectiles integrated and resolved ≤ 2.0 ms/tick p99 | `bench_projectiles_10k` |
| PERF-PHYS-004 | 512 m shape-aware raycast ≤ 20 µs p99; a rare 1,024-ray bulk query ≤ 21 ms | `bench_raycast` |
| PERF-PHYS-005 | 512 fluid updates ≤ 0.5 ms/tick p99; 4,096 pending drain ≤ 1.5 ms | `bench_fluids` |
| PERF-PHYS-006 | TNT power 4 explosion geometry (1,352 rays) ≤ 100 µs p99 | `bench_explosion` |
| PERF-PHYS-007 | 100 falling blocks ≤ 0.2 ms/tick p99 | `bench_falling_blocks` |
| PERF-PHYS-008 | 1,000 entity-pair push resolutions ≤ 0.3 ms/tick p99 | `bench_entity_push` |
| PERF-PHYS-009 | Shape lookup ≤ 2 ns; shape table ≤ 256 KiB for 2,048 shapes excluding the state map | microbench + `size_of` assertion |
| PERF-PHYS-010 | Physics memory ≤ 4 MiB at 2,000 bodies; `Body` ≤ 128 B | allocator counter |
| PERF-PHYS-011 | 50 players + 200 entities + 2,000 projectiles ≤ 3.5 ms/tick p99 (no AI) | `bench_physics_mixed` |

## 6. Failure modes and risks

| Failure | Detection | Behaviour |
| --- | --- | --- |
| Non-finite position or velocity from a mod, explosion or division | `is_finite` after input and each axis | restore last valid position, zero the axis, count, log rate-limited; never panic |
| Tunnelling through a thin shape at high speed | post-tick penetration check + fuzz | sub-step split, snap to contact, count; a repeated case fails CI |
| Body stuck inside a block after an edit or shape reload | `overlap` on spawn/landing/edit | 3×3×3 upward then lateral unstuck search; freeze and log if none |
| Corner clip on a diagonal move | conformance trace | fixed axis order; stop on the first blocked axis, no corner cutting |
| Body pushed through a wall by an entity or piston | post-push block sweep | revert the push, leave the pair overlapping, count `push_blocked` |
| Fluid flood or update storm | queue depth, drain time | 512/tick budget, 4,096 cap, same-position coalescing, no immediate updates |
| Water/lava duplication or double reaction | block-count assertion after quiescence | one transaction per reaction, source rules of §3.9, conformance fixture |
| Piston leaves an entity inside a block | second displacement pass | abort the piston move or crush per policy; never leave an overlap |
| Minecart leaves the rail or over-speeds | rail pose check, speed cap | derail into a free body at the current velocity, clamp, log |
| Boat stuck on land or in a wall | speed/friction probe, overlap | allow pushing, break after impact threshold, eject to a free spot |
| Raycast into an unloaded chunk | `Ready` check | stop at the boundary, return the last hit or `None` |
| Explosion block list over cap | count before apply | cap at 4,096, log, apply the nearest; content power is validated |
| Mod query flood | per-mod quota | reject with `QueryLimit`, disable the query, keep the tick |
| Shape hot reload puts bodies inside new geometry | overlap scan after swap | unstuck pass next tick, no world rollback |

| Risk | Impact | Mitigation |
| --- | --- | --- |
| RISK-058 Physics cost grows with entities and projectiles | tick overrun in combat | budgets, SoA, projectile caps, mixed-scene bench |
| RISK-059 Pathological mod shapes | collision cost and visual mismatch | 8-box cap, canonical merge, shape-table cap, fuzz |
| RISK-060 Tunnelling under lag or high speed | players fall through worlds, arrows miss | sub-step cap, continuous projectile DDA, penetration fuzz |
| RISK-061 Fluid simulation spikes or duplicates | tick spikes, economy damage | scheduled budget, no recursion, source rules, 24 h soak |
| RISK-062 Entity pushing used to grief or clip | player trust and anti-cheat | capped push, re-sweep, blocked-push revert |
| RISK-063 Vehicle physics feels wrong or derails | boats and minecarts unusable | conformance maps, base-profile constants, recorded feel tests |
| RISK-064 Determinism drift from math or order | desync and failed replays | pinned functions, fixed order, cross-platform hashes, no FMA |
| RISK-065 Piston/entity transaction duplication | item and block duplication | one transaction, before/after displacement, duplication fuzz |

## 7. Testing strategy

| Level | What it proves | Tooling |
| --- | --- | --- |
| Unit | shape merge, connection masks, axis order, cost and clamp constants, medium sampling | nextest with golden tables |
| Conformance scenarios | walk, sprint, jump 1–3 blocks, slab/stair step, sneak edge, ladder, water exit, lava sink, elytra dive, boat, minecart, piston crush, sand fall, TNT, arrow, 512 m ray | golden trace per scenario: position, velocity, contacts, flags each tick |
| Piston face order | an entity blocked on two faces is displaced to the first free face of the [12 §3.5.2](12-redstone-and-logic.md#352-pistons) list (−Y, −X, +X, −Z, +Z, +Y), not to the geometrically nearest one | two-free-face fixture asserting the destination |
| Differential solver | no penetration and final position within 1e-4 of a 1e-4 m brute-force reference on 1,000 random scenes | `proptest` + reference implementation |
| Fuzz | random inputs, shapes, velocities, explosions, fluid edits: finite, inside border, no penetration > 1e-6, no duplicate block/item, no panic | `cargo-fuzz`, 24 h nightly, 10M-tick soak |
| Determinism | same trace and state hash on Linux/Windows/macOS and 1/2/8 workers; 180° rotation symmetry for a scene and its rotated copy | cross-platform replay harness ([02 §3.10](02-engine-core.md#310-determinism-harness)) |
| Ordering | fluid/block/entity/piston interleavings do not change the hash for the same input log | scripted interleaving test |
| Prediction | client correction converges within two ticks; rejected prediction does not tunnel | loopback simulator ([MS-06](../plan/01-milestones.md)) |
| Anti-cheat input | speed, fly, noclip, reach traces from physics are detected by [20](20-security-and-anti-cheat.md) | cheater corpus ([MS-15](../plan/01-milestones.md)) |
| Content | shape boxes in range, ≤ 8 after merge, no empty-but-solid shape, reload unsticks | pack validator + reload fixture |
| Performance | every PERF-PHYS budget on the reference machine and the 4 vCPU server | bench box, CI gate ([DEC-035](../decisions.md)) |

The symmetry test deliberately uses a 180° yaw rotation plus an integer block translation, not a 90° rotation: the fixed Y → X → Z order is not X/Z symmetric, and pretending otherwise would hide a real class of corner-case divergence. A 90° test would need a solver whose axis order is derived from the movement direction, which is a different (and rejected) design.

## 8. Open questions

1. **Spatial fixed-point fallback ([OPEN-002](../decisions.md)).** OPEN-002 still asks whether spatial state becomes fixed-point if the determinism harness shows drift; this chapter assumes `f64` per [DEC-039](../decisions.md). If OPEN-002 flips, shapes, integration and the save encoding need a coordinated migration. Owner: **MS-02**.
2. **Register item needed: piston crush policy.** Whether a blocked piston displaces, damages or aborts, and which damage type it uses, is gameplay-visible and shared with [12](12-redstone-and-logic.md). Owner: **MS-11** with MS-12.
3. **Register item needed: canonical fluid model.** Level scale, source formation and waterlogging flow are specified here as one clean model; whether servers may select a "classic parity" fluid mode is a decision, not a config default. Owner: **MS-10** with MS-14.
4. **Elytra and vehicle constants.** The model shape is fixed, but 30 / 67.2 m/s, 8 m/s boats and 8/12 m/s minecarts are base-profile values that need play tuning. Owner: **MS-11**.
5. **Projectile cap at scale.** 10,000 projectiles is the budget; whether a server enforces a lower active-projectile cap, and what happens to the excess, needs an owner. Owner: **MS-15**.
6. **Entity-entity collision for vehicles.** Boats and minecarts currently collide softly with mobs and players; a solid-body mode may be needed for parity and for minecart collisions. Owner: **MS-11**.
7. **Register item needed: mod shape budget.** 8 boxes and 2,048 shapes are conservative; whether trusted packs may raise them with an operator opt-in is open. Owner: **MS-09** with MS-15.
8. **Fluid scheduling ownership.** This chapter specifies the rules and delays, [12](12-redstone-and-logic.md) the cross-system order; a single owner for the merged queue may be needed before MS-11. Owner: **MS-11** with MS-12.
9. **Collision epsilon and guaranteed penetration bound.** 1e-6 m after a tick is the test threshold; whether it is also a public API guarantee for mods is open. Owner: **MS-02**.

## 9. References

- Decisions: [DEC-004](../decisions.md) (sections), [DEC-007](../decisions.md) (fixed tick), [DEC-008](../decisions.md) (authority), [DEC-021](../decisions.md) (f64 and pinned math), [DEC-029](../decisions.md) (validation), [DEC-035](../decisions.md) (budgets), [DEC-038](../decisions.md) (20 Hz), [DEC-039](../decisions.md) (numeric policy), [DEC-045](../decisions.md) (visual-only packs), [DEC-049](../decisions.md) (no client mod code in 1.0).
- Chapters: [00](00-conventions.md), [01](01-technical-architecture.md), [02](02-engine-core.md), [05](05-world-model.md), [07](07-light-and-weather.md), [08](08-gameplay-survival.md), [09](09-entities-ai.md), [11](11-inventory-crafting-items.md), [12](12-redstone-and-logic.md), [15](15-content-system.md), [16](16-modding.md), [17](17-persistence.md), [18](18-networking-multiplayer.md), [20](20-security-and-anti-cheat.md), [25](25-performance-budgets.md).
- Research: [Weft teardown](../research/01-weft-teardown.md) §4.5 (region ticking) and §10 lesson 20 (validated movement); [Minecraft feature inventory](../research/05-minecraft-feature-inventory.md) §3.9 (fluids, gravity, lighting) and §3.12 (combat and movement feel).
- External: Amanatides & Woo, "A Fast Voxel Traversal Algorithm for Ray Tracing", *Eurographics '87* — <https://doi.org/10.1111/j.1467-8659.1987.tb00527.x>; Christer Ericson, *Real-Time Collision Detection*, Morgan Kaufmann, 2005, ch. 5–6 (AABB sweeps and plane tests); IEEE 754-2019, *IEEE Standard for Floating-Point Arithmetic* — <https://doi.org/10.1109/IEEESTD.2019.8766229>; Minecraft Wiki, "Fluid", "Explosion" and "Piston" — external parity facts for flow delays, blast geometry and push limits: <https://minecraft.wiki/w/Fluid>, <https://minecraft.wiki/w/Explosion>, <https://minecraft.wiki/w/Piston>.

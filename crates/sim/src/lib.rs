//! Authoritative player simulation: deterministic, headless movement and block interaction.
//!
//! The simulation is independent of the renderer and of wall-clock time. Player
//! positions and velocities are [`WorldVec`] (`f64`) metres; the body is an
//! axis-aligned box swept against the world's solid blocks on one axis at a time.
//! All decisions are made from the supplied [`PlayerInput`] and [`World`], so the
//! same input sequence always produces the same result.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles

use ob_content::{BlockId, MobKind, MobSpec, Registry};
use ob_math::{Aabb, BlockPos, SEA_LEVEL, WorldVec};
use ob_world::World;

/// Half-width of the player's square collision box, in metres.
pub const PLAYER_HALF_WIDTH: f64 = 0.3;
/// Total height of the player's collision box, in metres.
pub const PLAYER_HEIGHT: f64 = 1.8;
/// Height of the eye above the player's feet, in metres.
pub const EYE_HEIGHT: f64 = 1.62;
/// Downward acceleration while walking, in metres per second squared.
pub const GRAVITY: f64 = 28.0;
/// Upward velocity applied by a jump from the ground, in metres per second.
pub const JUMP_SPEED: f64 = 8.4;
/// Walking speed on land, in metres per second.
pub const WALK_SPEED: f64 = 4.317;
/// Sprinting speed on land, in metres per second.
pub const SPRINT_SPEED: f64 = 5.612;
/// Walk speed while crouching/sneaking (Minecraft's ~1.31 m/s).
pub const SNEAK_SPEED: f64 = 1.31;
/// Eye height above the feet while crouching (blocks).
pub const CROUCH_EYE_HEIGHT: f64 = 1.27;
/// Flying speed, in metres per second.
pub const FLY_SPEED: f64 = 12.0;
/// Maximum distance at which the player can break or place a block, in metres.
pub const REACH: f64 = 5.0;
/// Fixed simulation tick length, in seconds (20 Hz).
pub const TICK_DT: f64 = 1.0 / 20.0;

/// Maximum health, in half-hearts (20 HP = 10 hearts).
pub const MAX_HEALTH: f32 = 20.0;
/// Maximum hunger, in hunger points (20 points = 10 drumsticks).
pub const MAX_HUNGER: f32 = 20.0;
/// Downward impact speed that still causes no fall damage, in metres per second.
///
/// With [`GRAVITY`] at 28 m/s² this makes a fall of about three metres safe.
pub const FALL_SAFE_IMPACT: f64 = 13.0;
/// Health lost per metre per second of impact above [`FALL_SAFE_IMPACT`].
pub const FALL_DAMAGE_PER_IMPACT: f32 = 0.8;
/// Seconds between starvation damage while hunger is zero.
pub const STARVATION_INTERVAL: f64 = 4.0;
/// Seconds between health regeneration ticks while hunger is high.
pub const REGEN_INTERVAL: f64 = 4.0;
/// Minimum hunger at which health regenerates.
pub const REGEN_HUNGER_THRESHOLD: f32 = 18.0;
/// Hunger drained per second by simply existing, in points per second.
pub const HUNGER_IDLE_DRAIN: f32 = 0.005;
/// Extra hunger drained per metre moved horizontally.
pub const HUNGER_DRAIN_PER_METRE: f32 = 0.01;
/// Extra hunger drained when a jump is initiated.
pub const HUNGER_JUMP_COST: f32 = 0.03;

/// Largest distance the feet may move along one axis per collision sub-step.
const MAX_SUBSTEP: f64 = 0.45;
/// Largest height the player can step up, in metres.
const STEP_HEIGHT: f64 = 0.6;
/// Small separation used when descending after a step-up attempt.
const STEP_EPSILON: f64 = 1e-4;
/// Speed clamp applied to the player each tick, in metres per second.
const TERMINAL_VELOCITY: f64 = 78.4;
/// Water buoyancy acceleration, in metres per second squared.
const WATER_BUOYANCY: f64 = 26.0;
/// Fraction of velocity retained per tick in water.
const WATER_DRAG: f64 = 0.80;
/// Fraction of the walking speed available in water.
const WATER_SPEED_FACTOR: f64 = 0.5;
/// Horizontal acceleration toward the target speed, in metres per second squared.
const GROUND_ACCEL: f64 = 40.0;
/// Reduced air control acceleration, in metres per second squared.
const AIR_ACCEL: f64 = 10.0;
/// Horizontal acceleration while in water, in metres per second squared.
const WATER_ACCEL: f64 = 20.0;
/// Upward swim velocity while jump is held in water, in metres per second.
const SWIM_UP: f64 = 2.2;
/// Number of bisection iterations used to settle a blocked axis exactly.
const CONTACT_ITERATIONS: u32 = 24;
/// Tolerance for accumulated seconds compared against whole-second timers.
const TIMER_EPSILON: f64 = 1e-9;

/// One axis of the collision sweep.
const AXIS_X: usize = 0;
/// One axis of the collision sweep.
const AXIS_Y: usize = 1;
/// One axis of the collision sweep.
const AXIS_Z: usize = 2;

/// Player movement intent for one simulation tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlayerInput {
    /// Movement along the camera's horizontal forward axis, in `-1..=1`.
    pub forward: f32,
    /// Movement along the camera's right axis, in `-1..=1`.
    pub strafe: f32,
    /// Whether the jump (or swim-up) control is held.
    pub jump: bool,
    /// Whether the sprint modifier is held.
    pub sprint: bool,
    /// Whether the crouch/sneak control is held.
    pub sneak: bool,
    /// Edge-triggered request to flip [`Player::flying`].
    pub fly_toggle: bool,
}

/// Mutable state of one player.
#[derive(Debug, Clone)]
pub struct Player {
    /// Feet centre in world space.
    pub pos: WorldVec,
    /// Velocity in metres per second.
    pub velocity: WorldVec,
    /// Horizontal look angle in radians (zero faces `-Z`).
    pub yaw: f32,
    /// Vertical look angle in radians (positive looks up).
    pub pitch: f32,
    /// Whether a downward sweep contacted a solid block on the last tick.
    pub on_ground: bool,
    /// Whether flight mode is active.
    pub flying: bool,
    /// Whether the player is crouching/sneaking this tick.
    pub sneaking: bool,
    /// Whether the player is sprinting this tick.
    pub sprinting: bool,
    /// Health in half-hearts, `0..=`[`MAX_HEALTH`].
    pub health: f32,
    /// Hunger in points, `0..=`[`MAX_HUNGER`].
    pub hunger: f32,
    /// Whether health reached zero. Dead players take no further survival damage.
    pub dead: bool,
    /// Seconds accumulated toward the next starvation damage tick.
    pub starvation_timer: f64,
    /// Seconds accumulated toward the next regeneration tick.
    pub regen_timer: f64,
}

impl Player {
    /// Creates a motionless, alive player with full health and hunger.
    pub fn new(spawn: WorldVec) -> Self {
        Self {
            pos: spawn,
            velocity: WorldVec::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            flying: false,
            sneaking: false,
            sprinting: false,
            health: MAX_HEALTH,
            hunger: MAX_HUNGER,
            dead: false,
            starvation_timer: 0.0,
            regen_timer: 0.0,
        }
    }

    /// World-space position of the camera eye.
    ///
    /// Crouching lowers the camera to [`CROUCH_EYE_HEIGHT`].
    pub fn eye(&self) -> WorldVec {
        let eye_height = if self.sneaking { CROUCH_EYE_HEIGHT } else { EYE_HEIGHT };
        WorldVec::new(self.pos.x, self.pos.y + eye_height, self.pos.z)
    }

    /// The player's axis-aligned collision box.
    pub fn aabb(&self) -> Aabb {
        player_aabb_at(self.pos)
    }

    /// Unit camera-forward vector derived from [`Player::yaw`] and [`Player::pitch`].
    pub fn forward(&self) -> WorldVec {
        camera_forward(self.yaw, self.pitch)
    }
}

/// Facts about one simulation step that gameplay rules consume.
///
/// The simulation itself stays mode- and vitals-agnostic; [`advance_vitals`]
/// and the Host decide what hunger, fall damage and regeneration do with these
/// events.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StepEvents {
    /// Downward speed at the moment the player first touched the ground this
    /// tick, in metres per second; zero when the player did not land.
    pub landing_impact: f64,
    /// Horizontal distance travelled this tick, in metres.
    pub moved_distance: f64,
    /// Whether the player moved with the sprint modifier this tick.
    pub sprinted: bool,
    /// Whether a jump was initiated from the ground this tick.
    pub jumped: bool,
}

/// Block interaction requested from the player's camera.
#[derive(Debug, Clone, Copy)]
pub struct BlockAction {
    /// Remove the block hit by the camera ray.
    pub break_block: bool,
    /// Place [`BlockAction::place`] in the air cell adjacent to the hit block.
    pub place_block: bool,
    /// Block type to place.
    pub place: BlockId,
}

/// Advances one player by `dt` seconds.
///
/// The solver resolves the collision axes in the fixed order Y, X, Z, clamps
/// each axis step to 0.45 metres, and attempts a step-up after a grounded
/// horizontal collision. Flying ignores gravity and follows the camera's
/// forward/right vectors. Water applies buoyancy, drag and a reduced target
/// speed; holding jump swims upward.
///
/// Returns the [`StepEvents`] observed this tick. The simulation does not
/// change health or hunger itself; [`advance_vitals`] and the Host apply those
/// rules so creative mode can ignore them.
pub fn step_player(world: &World, player: &mut Player, input: &PlayerInput, dt: f64) -> StepEvents {
    let start_pos = player.pos;
    if input.fly_toggle {
        player.flying = !player.flying;
    }

    let forward_input = f64::from(input.forward.clamp(-1.0, 1.0));
    let strafe_input = f64::from(input.strafe.clamp(-1.0, 1.0));
    let moved_intent = forward_input != 0.0 || strafe_input != 0.0;
    let yaw = f64::from(player.yaw);
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let flat_forward = WorldVec::new(-sin_yaw, 0.0, -cos_yaw);
    let flat_right = WorldVec::new(cos_yaw, 0.0, -sin_yaw);
    let was_on_ground = player.on_ground;
    let in_water = !player.flying && player_in_water(world, player.pos);
    let mut jumped = false;
    player.sneaking = input.sneak && !player.flying;

    if player.flying {
        let mut wish = player.forward() * forward_input + flat_right * strafe_input;
        if wish.length_squared() > 1.0 {
            wish = wish.normalize();
        }
        player.velocity = wish * FLY_SPEED;
    } else {
        player.velocity.y -= GRAVITY * dt;
        if in_water {
            player.velocity.y += WATER_BUOYANCY * dt;
            apply_water_drag(player, dt);
        }

        let mut wish = flat_forward * forward_input + flat_right * strafe_input;
        if wish.length_squared() > 1.0 {
            wish = wish.normalize();
        }
        // Crouching caps movement at sneak speed and suppresses sprinting.
        let mode_speed = if player.sneaking {
            SNEAK_SPEED
        } else if input.sprint && moved_intent {
            SPRINT_SPEED
        } else {
            WALK_SPEED
        };
        let target_speed = if in_water {
            mode_speed * WATER_SPEED_FACTOR
        } else {
            mode_speed
        };
        let target = wish * target_speed;
        let acceleration = if in_water {
            WATER_ACCEL
        } else if was_on_ground {
            GROUND_ACCEL
        } else {
            AIR_ACCEL
        };
        let max_delta = acceleration * dt.abs();
        let mut horizontal_delta = target - WorldVec::new(player.velocity.x, 0.0, player.velocity.z);
        let delta_length = horizontal_delta.length();
        if delta_length > max_delta && delta_length > 0.0 {
            horizontal_delta *= max_delta / delta_length;
        }
        player.velocity.x += horizontal_delta.x;
        player.velocity.z += horizontal_delta.z;

        if in_water {
            if input.jump {
                player.velocity.y = SWIM_UP;
            }
        } else if was_on_ground && input.jump {
            player.velocity.y = JUMP_SPEED;
            jumped = true;
        }
    }
    player.sprinting = input.sprint && !input.sneak && moved_intent && !player.flying && !in_water;

    clamp_velocity(&mut player.velocity);
    // Captured before the collision response zeroes it: this is the speed the
    // player had when the downward sweep first contacted the ground.
    let pre_collision_vertical_velocity = player.velocity.y;

    player.on_ground = false;

    let blocked_y = sweep_axis(world, &mut player.pos, AXIS_Y, player.velocity.y * dt);
    if blocked_y {
        if player.velocity.y < 0.0 {
            player.on_ground = true;
        }
        player.velocity.y = 0.0;
    }

    let mut desired_x = player.velocity.x * dt;
    let mut desired_z = player.velocity.z * dt;
    if player.sneaking && player.on_ground && !player.flying && !in_water {
        // Crouch edge protection: refuse a horizontal step whose destination
        // footprint has no block directly underneath.
        if !has_support_below(world, player.pos + WorldVec::new(desired_x, 0.0, 0.0)) {
            desired_x = 0.0;
            player.velocity.x = 0.0;
        }
        if !has_support_below(world, player.pos + WorldVec::new(desired_x, 0.0, desired_z)) {
            desired_z = 0.0;
            player.velocity.z = 0.0;
        }
    }

    let blocked_x = sweep_axis(world, &mut player.pos, AXIS_X, desired_x);
    if blocked_x {
        player.velocity.x = 0.0;
    }
    let blocked_z = sweep_axis(world, &mut player.pos, AXIS_Z, desired_z);
    if blocked_z {
        player.velocity.z = 0.0;
    }

    if !player.flying && player.on_ground && (blocked_x || blocked_z) {
        let step_x = if blocked_x { desired_x } else { 0.0 };
        let step_z = if blocked_z { desired_z } else { 0.0 };
        if step_x != 0.0 || step_z != 0.0 {
            try_step_up(world, player, step_x, step_z);
        }
    }

    let landing_impact = if !was_on_ground && player.on_ground && pre_collision_vertical_velocity < 0.0 {
        -pre_collision_vertical_velocity
    } else {
        0.0
    };
    let moved_x = player.pos.x - start_pos.x;
    let moved_z = player.pos.z - start_pos.z;
    StepEvents {
        landing_impact,
        moved_distance: (moved_x * moved_x + moved_z * moved_z).sqrt(),
        sprinted: player.sprinting,
        jumped,
    }
}

/// Whether any solid block overlaps `aabb`.
///
/// Iterates the integer block cells whose cubes overlap the box. Faces that
/// merely touch the boundary do not count as an overlap.
pub fn aabb_collides(world: &World, aabb: &Aabb) -> bool {
    let min_x = aabb.min.x.floor() as i32;
    let max_x = (aabb.max.x.ceil() as i32).saturating_sub(1);
    let min_y = aabb.min.y.floor() as i32;
    let max_y = (aabb.max.y.ceil() as i32).saturating_sub(1);
    let min_z = aabb.min.z.floor() as i32;
    let max_z = (aabb.max.z.ceil() as i32).saturating_sub(1);
    if max_x < min_x || max_y < min_y || max_z < min_z {
        return false;
    }
    for y in min_y..=max_y {
        for z in min_z..=max_z {
            for x in min_x..=max_x {
                if world.is_solid(BlockPos::new(x, y, z)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether a block sits directly under the given feet position.
///
/// Used by crouch edge protection: a thin probe AABB just below the feet detects
/// support without depending on the full collision box's corner geometry.
fn has_support_below(world: &World, feet: WorldVec) -> bool {
    let probe = Aabb::new(
        WorldVec::new(feet.x - PLAYER_HALF_WIDTH, feet.y - 0.08, feet.z - PLAYER_HALF_WIDTH),
        WorldVec::new(feet.x + PLAYER_HALF_WIDTH, feet.y, feet.z + PLAYER_HALF_WIDTH),
    );
    aabb_collides(world, &probe)
}

/// Applies a break/place interaction from the player's camera.
///
/// The ray starts at [`Player::eye`] and follows [`Player::forward`] for at
/// most [`REACH`] metres. A break clears the hit block. A place writes
/// `action.place` into the air cell on the hit face only when the hit block is
/// solid and the new block would not overlap the player. Returns whether the
/// world changed.
pub fn apply_block_actions(world: &mut World, player: &Player, action: &BlockAction) -> bool {
    if !action.break_block && !action.place_block {
        return false;
    }
    let Some(hit) = world.raycast(player.eye(), player.forward(), REACH) else {
        return false;
    };

    let hit_was_solid = world.is_solid(hit.block);
    let mut changed = false;
    if action.break_block {
        changed |= world.set_block(hit.block, BlockId::AIR);
    }
    if action.place_block && hit_was_solid && action.place != BlockId::AIR {
        let target = hit.block.offset(hit.normal.x, hit.normal.y, hit.normal.z);
        if world.get_block(target) == BlockId::AIR && !Aabb::block(target).intersects(&player.aabb()) {
            changed |= world.set_block(target, action.place);
        }
    }
    changed
}

/// Fall damage for one landing impact speed, in health points.
///
/// Impacts at or below [`FALL_SAFE_IMPACT`] are harmless. Above it, damage is
/// `(impact - FALL_SAFE_IMPACT) * 0.8`, capped at [`MAX_HEALTH`] so a single
/// fall can kill but never overkill.
pub fn fall_damage(impact: f64) -> f32 {
    if !impact.is_finite() || impact <= FALL_SAFE_IMPACT {
        return 0.0;
    }
    let excess = (impact - FALL_SAFE_IMPACT) as f32;
    (excess * FALL_DAMAGE_PER_IMPACT).min(MAX_HEALTH)
}

/// Applies raw damage, marking the player dead and stopping them at zero health.
///
/// Returns the damage actually dealt (clamped to the remaining health). Dead
/// players take no further damage.
pub fn apply_damage(player: &mut Player, amount: f32) -> f32 {
    if player.dead || !amount.is_finite() || amount <= 0.0 {
        return 0.0;
    }
    let dealt = amount.min(player.health.max(0.0));
    player.health -= dealt;
    if player.health <= 0.0 {
        player.health = 0.0;
        player.dead = true;
        player.velocity = WorldVec::ZERO;
    }
    dealt
}

/// Applies fall damage from a landing impact speed and returns the damage dealt.
///
/// Creative-mode immunity is the Host's responsibility: this helper always
/// applies the survival rule.
pub fn apply_fall_damage(player: &mut Player, impact: f64) -> f32 {
    apply_damage(player, fall_damage(impact))
}

/// Advances the survival hunger, starvation and regeneration rules by `dt`.
///
/// Hunger drains by [`HUNGER_IDLE_DRAIN`] per second, by
/// [`HUNGER_DRAIN_PER_METRE`] per metre moved and by [`HUNGER_JUMP_COST`] per
/// jump initiated this tick, clamped at zero. At zero hunger the player takes
/// one point of damage every [`STARVATION_INTERVAL`] seconds; at or above
/// [`REGEN_HUNGER_THRESHOLD`] hunger with missing health they heal one point
/// every [`REGEN_INTERVAL`] seconds. Death at zero health stops the player.
///
/// This is a pure function of the player and the supplied events: no wall
/// clock and no randomness. Dead players are left alone.
pub fn advance_vitals(player: &mut Player, events: &StepEvents, dt: f64) {
    if player.dead {
        player.velocity = WorldVec::ZERO;
        return;
    }
    if !dt.is_finite() || dt < 0.0 {
        return;
    }

    player.health = player.health.clamp(0.0, MAX_HEALTH);
    player.hunger = player.hunger.clamp(0.0, MAX_HUNGER);

    let mut drain = HUNGER_IDLE_DRAIN * dt as f32;
    drain += events.moved_distance as f32 * HUNGER_DRAIN_PER_METRE;
    if events.jumped {
        drain += HUNGER_JUMP_COST;
    }
    player.hunger = (player.hunger - drain).max(0.0);

    if player.hunger <= 0.0 {
        // Starving: no regeneration, one damage per STARVATION_INTERVAL.
        player.regen_timer = 0.0;
        player.starvation_timer += dt;
        while player.starvation_timer + TIMER_EPSILON >= STARVATION_INTERVAL && !player.dead {
            player.starvation_timer -= STARVATION_INTERVAL;
            apply_damage(player, 1.0);
        }
    } else {
        player.starvation_timer = 0.0;
        if player.hunger >= REGEN_HUNGER_THRESHOLD && player.health < MAX_HEALTH {
            player.regen_timer += dt;
            while player.regen_timer + TIMER_EPSILON >= REGEN_INTERVAL && player.health < MAX_HEALTH {
                player.regen_timer -= REGEN_INTERVAL;
                player.health = (player.health + 1.0).min(MAX_HEALTH);
            }
        } else {
            player.regen_timer = 0.0;
        }
    }

    if player.health <= 0.0 {
        player.health = 0.0;
        player.dead = true;
        player.velocity = WorldVec::ZERO;
        player.regen_timer = 0.0;
    }
}

/// Eats food, restoring hunger points.
///
/// Returns whether the food was eaten. Dead players, inedible amounts and
/// players who are already full cannot eat.
pub fn eat(player: &mut Player, food: u8) -> bool {
    if player.dead || food == 0 || player.hunger >= MAX_HUNGER {
        return false;
    }
    player.hunger = (player.hunger + f32::from(food)).min(MAX_HUNGER);
    true
}

/// Restores a player to full health and hunger and clears death and vitals
/// timers. Position, velocity and inventory are left untouched.
pub fn reset_vitals(player: &mut Player) {
    player.health = MAX_HEALTH;
    player.hunger = MAX_HUNGER;
    player.dead = false;
    player.starvation_timer = 0.0;
    player.regen_timer = 0.0;
}

/// Finds a safe spawn point above the loaded surface at block column `(x, z)`.
///
/// The feet are centred in the column and placed one metre above the highest
/// non-air block reported by [`World::height_at`], which clears both solid
/// ground and the top of a water column. An unloaded column falls back to one
/// block above sea level.
pub fn spawn_position(world: &World, x: i32, z: i32) -> WorldVec {
    let feet_y = match world.height_at(x, z) {
        Some(surface_y) => surface_y as f64 + 1.0,
        None => SEA_LEVEL as f64 + 1.0,
    };
    WorldVec::new(x as f64 + 0.5, feet_y, z as f64 + 0.5)
}

/// The axis-aligned collision box of a player whose feet centre is at `pos`.
fn player_aabb_at(pos: WorldVec) -> Aabb {
    Aabb::new(
        WorldVec::new(pos.x - PLAYER_HALF_WIDTH, pos.y, pos.z - PLAYER_HALF_WIDTH),
        WorldVec::new(
            pos.x + PLAYER_HALF_WIDTH,
            pos.y + PLAYER_HEIGHT,
            pos.z + PLAYER_HALF_WIDTH,
        ),
    )
}

/// Unit camera forward vector from yaw/pitch in radians.
fn camera_forward(yaw: f32, pitch: f32) -> WorldVec {
    let (sin_yaw, cos_yaw) = f64::from(yaw).sin_cos();
    let (sin_pitch, cos_pitch) = f64::from(pitch).sin_cos();
    WorldVec::new(-sin_yaw * cos_pitch, sin_pitch, -cos_yaw * cos_pitch)
}

/// Whether the block at the player's feet or waist is a fluid.
fn player_in_water(world: &World, pos: WorldVec) -> bool {
    let feet = BlockPos::new(pos.x.floor() as i32, pos.y.floor() as i32, pos.z.floor() as i32);
    let waist = BlockPos::new(feet.x, (pos.y + PLAYER_HEIGHT * 0.5).floor() as i32, feet.z);
    is_fluid(world, feet) || is_fluid(world, waist)
}

/// Whether the block at `pos` is a fluid.
fn is_fluid(world: &World, pos: BlockPos) -> bool {
    world.block_type(world.get_block(pos)).fluid
}

/// Applies per-tick water drag scaled to `dt`.
fn apply_water_drag(player: &mut Player, dt: f64) {
    let tick_fraction = (dt / TICK_DT).clamp(0.0, 1.0);
    let drag = 1.0 - (1.0 - WATER_DRAG) * tick_fraction;
    player.velocity.x *= drag;
    player.velocity.y *= drag;
    player.velocity.z *= drag;
}

/// Clamps a velocity to the terminal speed, recovering from non-finite values.
fn clamp_velocity(velocity: &mut WorldVec) {
    if !velocity.x.is_finite() || !velocity.y.is_finite() || !velocity.z.is_finite() {
        *velocity = WorldVec::ZERO;
        return;
    }
    let max_speed_sq = TERMINAL_VELOCITY * TERMINAL_VELOCITY;
    let speed_sq = velocity.length_squared();
    if speed_sq > max_speed_sq {
        *velocity *= TERMINAL_VELOCITY / speed_sq.sqrt();
    }
}

/// Moves `pos` along one axis by `delta`, stopping at the first solid contact.
///
/// Each sub-step is at most [`MAX_SUBSTEP`] metres, so the body cannot tunnel
/// through a block. When a sub-step collides, the exact contact is found by
/// bisection and the safe position is committed. Returns whether the axis was
/// blocked.
fn sweep_axis(world: &World, pos: &mut WorldVec, axis: usize, delta: f64) -> bool {
    if delta == 0.0 || !delta.is_finite() {
        return false;
    }
    let sign = if delta > 0.0 { 1.0 } else { -1.0 };
    let mut remaining = delta.abs();
    while remaining > 0.0 {
        let step = remaining.min(MAX_SUBSTEP);
        let mut candidate = *pos;
        offset_axis(&mut candidate, axis, sign * step);
        if aabb_collides(world, &player_aabb_at(candidate)) {
            let mut low = 0.0;
            let mut high = step;
            for _ in 0..CONTACT_ITERATIONS {
                let mid = (low + high) * 0.5;
                let mut probe = *pos;
                offset_axis(&mut probe, axis, sign * mid);
                if aabb_collides(world, &player_aabb_at(probe)) {
                    high = mid;
                } else {
                    low = mid;
                }
            }
            offset_axis(pos, axis, sign * low);
            return true;
        }
        *pos = candidate;
        remaining -= step;
    }
    false
}

/// Adds `delta` to one component of a world-space vector.
fn offset_axis(vec: &mut WorldVec, axis: usize, delta: f64) {
    match axis {
        AXIS_X => vec.x += delta,
        AXIS_Y => vec.y += delta,
        _ => vec.z += delta,
    }
}

/// Attempts to lift the player over an obstacle no higher than [`STEP_HEIGHT`].
///
/// The lift is simulated on a copy of the player; it is only committed when the
/// lifted destination is free, the horizontal move succeeds, and descending
/// lands on support above the starting height.
fn try_step_up(world: &World, player: &mut Player, dx: f64, dz: f64) -> bool {
    let start = player.pos;
    let lifts = [STEP_HEIGHT, STEP_HEIGHT * 0.75, STEP_HEIGHT * 0.5, STEP_HEIGHT * 0.25];
    for lift in lifts {
        let mut probe = start;
        probe.y += lift;
        if aabb_collides(world, &player_aabb_at(probe)) {
            continue;
        }
        if dx != 0.0 && sweep_axis(world, &mut probe, AXIS_X, dx) {
            continue;
        }
        if dz != 0.0 && sweep_axis(world, &mut probe, AXIS_Z, dz) {
            continue;
        }
        if aabb_collides(world, &player_aabb_at(probe)) {
            continue;
        }
        if !sweep_axis(world, &mut probe, AXIS_Y, -(lift + STEP_EPSILON)) {
            continue;
        }
        if probe.y <= start.y + STEP_EPSILON {
            continue;
        }
        if aabb_collides(world, &player_aabb_at(probe)) {
            continue;
        }
        player.pos = probe;
        player.on_ground = true;
        player.velocity.y = 0.0;
        return true;
    }
    false
}

/// Largest distance a mob may move along one axis per collision sub-step.
const MOB_MAX_SUBSTEP: f64 = 0.45;
/// Speed clamp applied to mobs, in metres per second.
const MOB_TERMINAL_VELOCITY: f64 = 78.4;
/// Water buoyancy acceleration for mobs, in metres per second squared.
///
/// Deliberately stronger than [`GRAVITY`] so an idle mob floats back to the
/// surface instead of resting on the bottom; player swimming instead holds the
/// jump control.
const MOB_WATER_BUOYANCY: f64 = 34.0;
/// Fraction of velocity retained per tick by a mob in water.
const MOB_WATER_DRAG: f64 = 0.80;
/// Fraction of normal speed a mob may use while swimming.
const MOB_WATER_SPEED_FACTOR: f64 = 0.5;
/// Smallest wander-target distance from the mob, in metres.
pub const MOB_WANDER_MIN_RADIUS: f64 = 2.0;
/// Largest wander-target distance from the mob, in metres.
pub const MOB_WANDER_MAX_RADIUS: f64 = 8.0;
/// Shortest wander leg duration, in seconds.
const MOB_WANDER_MIN_TIME: f64 = 2.0;
/// Longest wander leg duration, in seconds.
const MOB_WANDER_MAX_TIME: f64 = 7.0;
/// How far ahead a passive mob probes for a floor before stepping, in metres.
pub const MOB_CLIFF_PROBE: f64 = 0.6;

/// A passive mob flees for this long after being hurt, in seconds.
pub const MOB_HURT_FLEE_TIME: f64 = 4.0;
/// A zombie notices and chases a living player within this distance, in metres.
pub const MOB_CHASE_RADIUS: f64 = 16.0;
/// A zombie attacks a player whose feet position is within this distance, in metres.
pub const MOB_ATTACK_RANGE: f64 = 1.6;
/// Damage one zombie attack deals before armor, in health points.
pub const MOB_ATTACK_DAMAGE: f32 = 3.0;
/// Seconds between zombie attacks.
pub const MOB_ATTACK_COOLDOWN: f64 = 1.0;
/// Horizontal knockback speed applied to a player hit by a mob, in metres per second.
pub const MOB_KNOCKBACK_HORIZONTAL: f64 = 4.0;
/// Upward knockback speed applied to a player hit by a mob, in metres per second.
pub const MOB_KNOCKBACK_UP: f64 = 3.0;
/// Mobs older than this despawn, in seconds (10 real minutes at 20 Hz).
pub const MOB_DESPAWN_AGE: f64 = 600.0;
/// Mobs farther than this from the player request a despawn, in metres.
pub const MOB_DESPAWN_DISTANCE: f64 = 64.0;
/// Mobs below this height request a despawn (the void).
pub const MOB_DESPAWN_Y: f64 = -64.0;

/// The current AI intent of a mob.
///
/// The interim brain is a tiny deterministic state machine rather than a goal
/// graph: passive mobs alternate between [`MobAi::Idle`] and
/// [`MobAi::Wander`], flee directly away from the player for
/// [`MOB_HURT_FLEE_TIME`] after damage, and zombies replace the wander intent
/// with a straight-line [`MobAi::Chase`]. There is no pathfinding, line of
/// sight, memory, breeding, projectile or villager logic in this slice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MobAi {
    /// Standing still for this tick; the next tick may pick a wander leg.
    Idle,
    /// Walking toward a deterministic point and a duration for this leg.
    Wander {
        /// World-space feet target.
        target: WorldVec,
        /// Seconds remaining in this leg.
        timer: f64,
    },
    /// A hostile mob walking straight toward a living player.
    Chase {
        /// Latest observed player feet position.
        target: WorldVec,
    },
    /// A hurt passive mob running away from the position it was hurt at.
    Flee {
        /// Position to move away from (usually the player).
        from: WorldVec,
        /// Seconds remaining in the flee reaction.
        timer: f64,
    },
    /// The mob asked the host to remove it (age, distance or the void).
    Despawn,
}

/// One mob's simulation state.
///
/// `pos` is the centre of the feet, exactly like [`Player::pos`]; the collision
/// box extends up by the species height and sideways by the species width. No
/// AI state is persisted in this slice, so a loaded mob restarts at
/// [`MobAi::Idle`].
#[derive(Debug, Clone, PartialEq)]
pub struct Mob {
    /// Species definition key.
    pub kind: MobKind,
    /// Feet centre in world space.
    pub pos: WorldVec,
    /// Velocity in metres per second.
    pub velocity: WorldVec,
    /// Facing angle in radians (zero faces `-Z`, matching the player).
    pub yaw: f32,
    /// Health in health points, `0..=MobSpec::health`.
    pub health: f32,
    /// Current deterministic AI intent.
    pub ai: MobAi,
    /// Seconds alive; drives the despawn timer.
    pub age: f64,
    /// Seconds until the next hostile attack is allowed.
    pub attack_cooldown: f64,
    /// Whether the downward sweep contacted solid ground last tick.
    pub on_ground: bool,
    /// Seconds of recent-damage reaction remaining (host sets this on a hit).
    pub hurt_timer: f64,
}

impl Mob {
    /// Creates a full-health mob at `pos` from its registry definition.
    pub fn new(spec: &MobSpec, pos: WorldVec) -> Self {
        Self {
            kind: spec.kind,
            pos,
            velocity: WorldVec::ZERO,
            yaw: 0.0,
            health: spec.health,
            ai: MobAi::Idle,
            age: 0.0,
            attack_cooldown: 0.0,
            on_ground: false,
            hurt_timer: 0.0,
        }
    }

    /// The mob's axis-aligned collision box in the given registry.
    pub fn aabb(&self, registry: &Registry) -> Aabb {
        mob_aabb(registry, self.kind, self.pos)
    }

    /// Whether the mob is still alive.
    pub fn alive(&self) -> bool {
        self.health > 0.0
    }
}

/// Facts produced by one [`step_mob`] call for the authoritative host.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MobEvents {
    /// A hostile mob attacked the player this tick.
    pub attacked: bool,
    /// Damage the attack would deal before armor mitigation.
    pub damage: f32,
    /// Knockback the host should add to the player's velocity, in m/s.
    pub knockback: WorldVec,
    /// The host should remove this mob (age, distance or the void).
    pub despawn: bool,
    /// The mob began its flee reaction to recent damage this tick.
    pub hurt: bool,
}

/// One ray-selectable mob.
///
/// The server implements this for its `MobEntity`; tests can use a tiny mock.
/// Keeping selection in `ob_sim` means the client's crosshair and the host's
/// reach validation use the same AABB math.
pub trait MobTarget {
    /// Stable session entity id.
    fn mob_id(&self) -> u64;
    /// Species, used to look up the collision-box size.
    fn mob_kind(&self) -> MobKind;
    /// Feet centre in world space.
    fn mob_pos(&self) -> WorldVec;
}

/// The axis-aligned box of a mob of `kind` whose feet centre is at `pos`.
pub fn mob_aabb(registry: &Registry, kind: MobKind, pos: WorldVec) -> Aabb {
    let spec = registry.mob(kind);
    let half_width = f64::from(spec.width) * 0.5;
    let height = f64::from(spec.height);
    Aabb::new(
        WorldVec::new(pos.x - half_width, pos.y, pos.z - half_width),
        WorldVec::new(pos.x + half_width, pos.y + height, pos.z + half_width),
    )
}

/// Advances one mob by `dt` seconds.
///
/// Physics mirrors the player solver at mob scale: gravity, a swept AABB on
/// each axis in Y/X/Z order, sub-steps capped at 0.45 m so a mob cannot tunnel
/// through a floor, and simple buoyancy in water. There is no step-up in this
/// slice, so a one-block ledge blocks a mob.
///
/// AI is a pure function of the mob, the world and `dt`; each decision hashes
/// the mob kind, quantised position/age and a purpose salt. No RNG state, wall
/// clock, thread identity or allocation address participates.
///
/// * Passive mobs wander within [`MOB_WANDER_MIN_RADIUS`]..
///   [`MOB_WANDER_MAX_RADIUS`] of their current position for 2..7 s, probe the
///   floor [`MOB_CLIFF_PROBE`] metres ahead and refuse a step without support,
///   and flee directly away from the player for [`MOB_HURT_FLEE_TIME`] seconds
///   after the host sets [`Mob::hurt_timer`].
/// * Zombies chase a living player within [`MOB_CHASE_RADIUS`] and attack when
///   within [`MOB_ATTACK_RANGE`] whenever [`Mob::attack_cooldown`] is zero,
///   dealing [`MOB_ATTACK_DAMAGE`] and returning a knockback vector.
/// * Mobs that are older than [`MOB_DESPAWN_AGE`], farther than
///   [`MOB_DESPAWN_DISTANCE`] from the player, or below [`MOB_DESPAWN_Y`] set
///   [`MobEvents::despawn`]; removal is the host's decision.
///
/// Water is not a barrier once a mob is in it: the mob receives buoyancy and
/// swims at half speed, which floats it back to the surface. A mob walking on
/// land refuses to step into a column without solid support, so it stops at a
/// shoreline or cliff edge instead of walking into deep water.
pub fn step_mob(world: &World, mob: &mut Mob, player_pos: WorldVec, player_alive: bool, dt: f64) -> MobEvents {
    let mut events = MobEvents::default();
    if !dt.is_finite() || dt <= 0.0 {
        return events;
    }
    let registry = world.registry();
    let spec = registry.mob(mob.kind);
    if mob.ai == MobAi::Despawn {
        // A caller that did not remove the mob (e.g. a test) may recover on a
        // later tick once the despawn condition no longer holds.
        mob.ai = MobAi::Idle;
    }
    mob.age += dt;

    mob.hurt_timer = (mob.hurt_timer - dt).max(0.0);
    if mob.hurt_timer < TIMER_EPSILON {
        mob.hurt_timer = 0.0;
    }
    mob.attack_cooldown = (mob.attack_cooldown - dt).max(0.0);

    // Passive mobs drop everything and run for the duration of the hurt timer.
    // A caller may also construct `Flee` directly (tests, future scripted
    // behaviour); its own timer drives the reaction when no fresh hurt exists.
    let was_fleeing = matches!(mob.ai, MobAi::Flee { .. });
    if !spec.hostile && mob.hurt_timer > 0.0 {
        let from = if player_pos.is_finite() { player_pos } else { mob.pos };
        match mob.ai {
            MobAi::Flee { from: old, timer } if mob.hurt_timer <= timer => {
                mob.ai = MobAi::Flee { from: old, timer };
            }
            _ => {
                events.hurt = !was_fleeing;
                mob.ai = MobAi::Flee {
                    from,
                    timer: mob.hurt_timer.max(MOB_HURT_FLEE_TIME),
                };
            }
        }
    }
    if let MobAi::Flee { from, timer } = mob.ai {
        let timer = (timer - dt).max(0.0);
        mob.ai = if timer <= TIMER_EPSILON {
            MobAi::Idle
        } else {
            MobAi::Flee { from, timer }
        };
    }

    let player_valid = player_alive && player_pos.is_finite();
    let player_distance = if player_pos.is_finite() {
        mob.pos.distance(player_pos)
    } else {
        f64::INFINITY
    };

    if spec.hostile && player_valid && player_distance <= MOB_CHASE_RADIUS {
        mob.ai = MobAi::Chase { target: player_pos };
    } else if !matches!(mob.ai, MobAi::Flee { .. }) {
        if matches!(mob.ai, MobAi::Chase { .. }) {
            // The target left the chase radius, died or was never there.
            mob.ai = MobAi::Idle;
        }
        update_wander(mob, dt);
    }

    let in_water = mob_in_water(world, mob);
    let mut wish = desired_velocity(mob, spec, in_water);
    if spec.hostile && player_valid && player_distance <= MOB_CHASE_RADIUS {
        // A zombie stops just inside attack range instead of walking through
        // the player.
        let horizontal = horizontal_distance(mob.pos, player_pos);
        if horizontal <= MOB_ATTACK_RANGE * 0.9 {
            wish = WorldVec::ZERO;
        }
    }

    if !spec.hostile
        && mob.on_ground
        && !in_water
        && let Some(direction) = horizontal_direction(wish)
        && !mob_has_support_ahead(world, registry, mob.kind, mob.pos, direction)
    {
        // Cliff/shore avoidance: cancel the step and pick a new wander leg on
        // the next tick rather than walking into the void or deep water.
        wish = WorldVec::ZERO;
        mob.velocity.x = 0.0;
        mob.velocity.z = 0.0;
        mob.ai = MobAi::Idle;
    }

    mob.velocity.x = wish.x;
    mob.velocity.z = wish.z;
    if wish.length_squared() > f64::EPSILON {
        mob.yaw = yaw_from_direction(wish);
    }

    // Vertical integration and collision.
    mob.velocity.y -= GRAVITY * dt;
    if in_water {
        mob.velocity.y += MOB_WATER_BUOYANCY * dt;
        apply_mob_water_drag(mob, dt);
    }
    clamp_mob_velocity(&mut mob.velocity);

    mob.on_ground = false;
    let blocked_y = sweep_mob_axis(world, registry, mob.kind, &mut mob.pos, AXIS_Y, mob.velocity.y * dt);
    if blocked_y {
        if mob.velocity.y < 0.0 {
            mob.on_ground = true;
        }
        mob.velocity.y = 0.0;
    }

    let blocked_x = sweep_mob_axis(world, registry, mob.kind, &mut mob.pos, AXIS_X, mob.velocity.x * dt);
    if blocked_x {
        mob.velocity.x = 0.0;
    }
    let blocked_z = sweep_mob_axis(world, registry, mob.kind, &mut mob.pos, AXIS_Z, mob.velocity.z * dt);
    if blocked_z {
        mob.velocity.z = 0.0;
    }

    // Hostile attack: only a living player can be hit. The host runs the
    // damage through armor and may die; the sim only reports the intent.
    if spec.hostile
        && player_valid
        && mob.attack_cooldown <= TIMER_EPSILON
        && mob.pos.distance(player_pos) <= MOB_ATTACK_RANGE
    {
        let direction = horizontal_direction(player_pos - mob.pos).unwrap_or_else(|| {
            horizontal_direction(WorldVec::new(-mob.yaw.sin() as f64, 0.0, -mob.yaw.cos() as f64))
                .unwrap_or(WorldVec::new(0.0, 0.0, 1.0))
        });
        events.attacked = true;
        events.damage = MOB_ATTACK_DAMAGE;
        events.knockback = WorldVec::new(
            direction.x * MOB_KNOCKBACK_HORIZONTAL,
            MOB_KNOCKBACK_UP,
            direction.z * MOB_KNOCKBACK_HORIZONTAL,
        );
        mob.attack_cooldown = MOB_ATTACK_COOLDOWN;
        mob.yaw = yaw_from_direction(direction);
    }

    let despawn_age = mob.age > MOB_DESPAWN_AGE;
    let despawn_void = mob.pos.y < MOB_DESPAWN_Y;
    let despawn_distance = player_pos.is_finite() && mob.pos.distance(player_pos) > MOB_DESPAWN_DISTANCE;
    if despawn_age || despawn_void || despawn_distance {
        events.despawn = true;
        mob.ai = MobAi::Despawn;
    }

    events
}

/// Chooses or advances a wander leg.
fn update_wander(mob: &mut Mob, dt: f64) {
    if let MobAi::Wander { target, timer } = mob.ai {
        let timer = (timer - dt).max(0.0);
        let reached = horizontal_distance(mob.pos, target) <= 0.6;
        if timer <= TIMER_EPSILON || reached {
            mob.ai = MobAi::Idle;
        } else {
            mob.ai = MobAi::Wander { target, timer };
        }
    }
    if matches!(mob.ai, MobAi::Idle) {
        let hash = mob_decision_hash(mob, 0x57A4_11D0_0DC5_1E11);
        let angle = unit_interval(hash) * std::f64::consts::TAU;
        let radius = MOB_WANDER_MIN_RADIUS
            + unit_interval(ob_math::mix64(hash)) * (MOB_WANDER_MAX_RADIUS - MOB_WANDER_MIN_RADIUS);
        let time = MOB_WANDER_MIN_TIME
            + unit_interval(ob_math::mix64(hash ^ 0x9E37_79B9_7F4A_7C15)) * (MOB_WANDER_MAX_TIME - MOB_WANDER_MIN_TIME);
        mob.ai = MobAi::Wander {
            target: WorldVec::new(
                mob.pos.x + angle.cos() * radius,
                mob.pos.y,
                mob.pos.z + angle.sin() * radius,
            ),
            timer: time,
        };
    }
}

/// Desired horizontal velocity for the current AI intent.
fn desired_velocity(mob: &Mob, spec: &MobSpec, in_water: bool) -> WorldVec {
    let water_factor = if in_water { MOB_WATER_SPEED_FACTOR } else { 1.0 };
    let (target, speed_factor) = match mob.ai {
        MobAi::Idle | MobAi::Despawn => return WorldVec::ZERO,
        MobAi::Wander { target, .. } => (target, 1.0),
        MobAi::Chase { target } => (target, 1.0),
        MobAi::Flee { from, .. } => {
            let away = horizontal_direction(mob.pos - from).unwrap_or_else(|| {
                let hash = mob_decision_hash(mob, 0x3F1D_9A2C_4B8E_7711);
                let angle = unit_interval(hash) * std::f64::consts::TAU;
                WorldVec::new(angle.cos(), 0.0, angle.sin())
            });
            return away * (f64::from(spec.speed) * 1.25 * water_factor);
        }
    };
    let Some(direction) = horizontal_direction(target - mob.pos) else {
        return WorldVec::ZERO;
    };
    direction * (f64::from(spec.speed) * speed_factor * water_factor)
}

/// Whether a passive mob has solid support just ahead of its feet.
fn mob_has_support_ahead(
    world: &World,
    registry: &Registry,
    kind: MobKind,
    pos: WorldVec,
    direction: WorldVec,
) -> bool {
    let probe_centre = pos + direction * MOB_CLIFF_PROBE;
    let spec = registry.mob(kind);
    let half = f64::from(spec.width) * 0.5;
    let probe = Aabb::new(
        WorldVec::new(probe_centre.x - half, pos.y - 0.12, probe_centre.z - half),
        WorldVec::new(probe_centre.x + half, pos.y, probe_centre.z + half),
    );
    aabb_collides(world, &probe)
}

/// Whether any block overlapping the mob's lower body or waist is a fluid.
fn mob_in_water(world: &World, mob: &Mob) -> bool {
    let spec = world.registry().mob(mob.kind);
    let feet = BlockPos::new(
        mob.pos.x.floor() as i32,
        mob.pos.y.floor() as i32,
        mob.pos.z.floor() as i32,
    );
    let waist = BlockPos::new(
        feet.x,
        (mob.pos.y + f64::from(spec.height) * 0.5).floor() as i32,
        feet.z,
    );
    is_fluid(world, feet) || is_fluid(world, waist)
}

/// Applies per-tick water drag to a mob's velocity.
fn apply_mob_water_drag(mob: &mut Mob, dt: f64) {
    let tick_fraction = (dt / TICK_DT).clamp(0.0, 1.0);
    let drag = 1.0 - (1.0 - MOB_WATER_DRAG) * tick_fraction;
    mob.velocity.x *= drag;
    mob.velocity.y *= drag;
    mob.velocity.z *= drag;
}

/// Clamps a mob velocity to the terminal speed and recovers from non-finite data.
fn clamp_mob_velocity(velocity: &mut WorldVec) {
    if !velocity.x.is_finite() || !velocity.y.is_finite() || !velocity.z.is_finite() {
        *velocity = WorldVec::ZERO;
        return;
    }
    let max_speed_sq = MOB_TERMINAL_VELOCITY * MOB_TERMINAL_VELOCITY;
    let speed_sq = velocity.length_squared();
    if speed_sq > max_speed_sq {
        *velocity *= MOB_TERMINAL_VELOCITY / speed_sq.sqrt();
    }
}

/// Moves a mob along one axis, stopping at the first solid contact.
fn sweep_mob_axis(
    world: &World,
    registry: &Registry,
    kind: MobKind,
    pos: &mut WorldVec,
    axis: usize,
    delta: f64,
) -> bool {
    if delta == 0.0 || !delta.is_finite() {
        return false;
    }
    let sign = if delta > 0.0 { 1.0 } else { -1.0 };
    let mut remaining = delta.abs();
    while remaining > 0.0 {
        let step = remaining.min(MOB_MAX_SUBSTEP);
        let mut candidate = *pos;
        offset_axis(&mut candidate, axis, sign * step);
        if aabb_collides(world, &mob_aabb(registry, kind, candidate)) {
            let mut low = 0.0;
            let mut high = step;
            for _ in 0..CONTACT_ITERATIONS {
                let mid = (low + high) * 0.5;
                let mut probe = *pos;
                offset_axis(&mut probe, axis, sign * mid);
                if aabb_collides(world, &mob_aabb(registry, kind, probe)) {
                    high = mid;
                } else {
                    low = mid;
                }
            }
            offset_axis(pos, axis, sign * low);
            return true;
        }
        *pos = candidate;
        remaining -= step;
    }
    false
}

/// Horizontal unit vector of `value`, or `None` when it is degenerate.
fn horizontal_direction(value: WorldVec) -> Option<WorldVec> {
    let horizontal = WorldVec::new(value.x, 0.0, value.z);
    if !horizontal.is_finite() || horizontal.length_squared() <= 1e-12 {
        return None;
    }
    Some(horizontal.normalize())
}

/// Horizontal distance between two positions.
fn horizontal_distance(a: WorldVec, b: WorldVec) -> f64 {
    let dx = a.x - b.x;
    let dz = a.z - b.z;
    (dx * dx + dz * dz).sqrt()
}

/// Yaw that faces `direction` with the player's convention (zero faces `-Z`).
fn yaw_from_direction(direction: WorldVec) -> f32 {
    (-direction.x).atan2(-direction.z) as f32
}

/// Stable 64-bit decision hash for one mob and purpose salt.
fn mob_decision_hash(mob: &Mob, salt: u64) -> u64 {
    let mut hash = 0x6A09_E667_F3BC_C909u64 ^ salt;
    hash = ob_math::mix64(hash ^ mob.kind.index() as u64);
    hash = ob_math::mix64(hash ^ mob.pos.x.to_bits());
    hash = ob_math::mix64(hash ^ mob.pos.y.to_bits());
    hash = ob_math::mix64(hash ^ mob.pos.z.to_bits());
    hash = ob_math::mix64(hash ^ mob.age.to_bits());
    hash
}

/// Maps the top 53 bits of `hash` to `[0, 1)`.
fn unit_interval(hash: u64) -> f64 {
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// Returns the nearest mob id hit by a ray from `origin` along `direction`.
///
/// The ray is clamped to `max_distance` and tested against each mob's
/// species-sized AABB (feet centre plus registry width/height). Ties at the
/// same entry distance break by ascending id. This is the one selection
/// primitive shared by the client crosshair and the host's reach check; the
/// host still validates damage itself.
pub fn raycast_mob<T: MobTarget>(
    registry: &Registry,
    mobs: &[T],
    origin: WorldVec,
    direction: WorldVec,
    max_distance: f64,
) -> Option<u64> {
    if !max_distance.is_finite() || max_distance < 0.0 || !origin.is_finite() {
        return None;
    }
    let direction = normalised_direction(direction)?;
    let mut best: Option<(f64, u64)> = None;
    for mob in mobs {
        let aabb = mob_aabb(registry, mob.mob_kind(), mob.mob_pos());
        let Some(distance) = ray_aabb(origin, direction, &aabb, max_distance) else {
            continue;
        };
        let id = mob.mob_id();
        best = match best {
            Some((best_distance, best_id))
                if distance > best_distance || (distance == best_distance && id > best_id) =>
            {
                Some((best_distance, best_id))
            }
            _ => Some((distance, id)),
        };
    }
    best.map(|(_, id)| id)
}

/// Normalises a ray direction, rejecting zero/non-finite vectors.
fn normalised_direction(direction: WorldVec) -> Option<WorldVec> {
    if !direction.is_finite() || direction.length_squared() <= 1e-12 {
        return None;
    }
    Some(direction.normalize())
}

/// Slab test returning the ray parameter where it enters `aabb`.
fn ray_aabb(origin: WorldVec, direction: WorldVec, aabb: &Aabb, max_distance: f64) -> Option<f64> {
    let mut t_min = 0.0f64;
    let mut t_max = max_distance;
    for axis in 0..3 {
        let (origin_axis, direction_axis, min, max) = match axis {
            AXIS_X => (origin.x, direction.x, aabb.min.x, aabb.max.x),
            AXIS_Y => (origin.y, direction.y, aabb.min.y, aabb.max.y),
            _ => (origin.z, direction.z, aabb.min.z, aabb.max.z),
        };
        if direction_axis.abs() <= 1e-12 {
            if origin_axis < min || origin_axis > max {
                return None;
            }
            continue;
        }
        let inverse = 1.0 / direction_axis;
        let mut near = (min - origin_axis) * inverse;
        let mut far = (max - origin_axis) * inverse;
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        t_min = t_min.max(near);
        t_max = t_max.min(far);
        if t_min > t_max {
            return None;
        }
    }
    (t_max >= 0.0).then_some(t_min.max(0.0))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_content::Registry;
    use ob_math::{ChunkPos, Rng};
    use ob_world::ChunkColumn;

    use super::*;

    const STONE: &str = "openblock:stone";
    const WATER: &str = "openblock:water";

    /// Builds a world whose chunks in `-range..=range` have a stone floor at
    /// `y = 0`.
    fn flat_world(range: i32) -> World {
        let registry = Arc::new(Registry::base());
        let stone = registry.id_or_air(STONE);
        let mut world = World::new(Arc::clone(&registry), 7);
        for chunk_z in -range..=range {
            for chunk_x in -range..=range {
                let mut chunk = ChunkColumn::new(ChunkPos::new(chunk_x, chunk_z));
                let (origin_x, origin_z) = ChunkPos::new(chunk_x, chunk_z).origin();
                for z in 0..16 {
                    for x in 0..16 {
                        chunk.set_block(BlockPos::new(origin_x + x, 0, origin_z + z), stone);
                    }
                }
                world.insert_chunk(chunk);
            }
        }
        world
    }

    /// Runs one simulation tick with `input`, returning its events.
    fn tick(world: &World, player: &mut Player, input: &PlayerInput) -> StepEvents {
        step_player(world, player, input, TICK_DT)
    }

    #[test]
    fn falling_player_lands_on_platform() {
        let world = flat_world(0);
        let mut player = Player::new(WorldVec::new(8.5, 10.0, 8.5));
        let input = PlayerInput::default();
        for _ in 0..200 {
            tick(&world, &mut player, &input);
            if player.on_ground {
                break;
            }
        }
        assert!(player.on_ground, "player did not land");
        assert!(
            player.velocity.y.abs() < 1e-9,
            "vertical velocity was {}",
            player.velocity.y
        );
        assert!((player.pos.y - 1.0).abs() < 1e-4, "feet ended at y = {}", player.pos.y);
        assert!(!aabb_collides(&world, &player.aabb()));
    }

    #[test]
    fn walking_into_wall_stops_without_tunnelling() {
        let mut world = flat_world(0);
        let stone = world.registry().id_or_air(STONE);
        for y in 1..=3 {
            for z in 0..16 {
                world.set_block(BlockPos::new(4, y, z), stone);
            }
        }
        let mut player = Player::new(WorldVec::new(1.0, 1.0, 8.5));
        player.on_ground = true;
        player.yaw = -std::f32::consts::FRAC_PI_2;
        let input = PlayerInput {
            forward: 1.0,
            ..PlayerInput::default()
        };
        for _ in 0..100 {
            tick(&world, &mut player, &input);
        }
        assert!(
            player.pos.x <= 4.0 - PLAYER_HALF_WIDTH + 1e-4,
            "player tunnelled to x = {}",
            player.pos.x
        );
        assert!(
            player.pos.x > 3.5,
            "player never reached the wall (x = {})",
            player.pos.x
        );
        assert!(player.on_ground, "player left the ground while walking");
        assert!(!aabb_collides(&world, &player.aabb()));
        let ground_y = player.pos.y;
        for _ in 0..100 {
            tick(&world, &mut player, &input);
        }
        assert!(
            (player.pos.y - ground_y).abs() < 1e-9,
            "grounding on flat floor was unstable: {} -> {}",
            ground_y,
            player.pos.y
        );
        assert!(player.pos.x <= 4.0 - PLAYER_HALF_WIDTH + 1e-4);
    }

    #[test]
    fn jump_raises_player_then_lands() {
        let world = flat_world(0);
        let mut player = Player::new(WorldVec::new(8.5, 1.0, 8.5));
        player.on_ground = true;
        let mut input = PlayerInput::default();
        for _ in 0..5 {
            tick(&world, &mut player, &input);
        }
        let ground_y = player.pos.y;
        assert!(player.on_ground);
        input.jump = true;
        tick(&world, &mut player, &input);
        input.jump = false;
        let mut highest = player.pos.y;
        let mut landed = false;
        for _ in 0..200 {
            tick(&world, &mut player, &input);
            highest = highest.max(player.pos.y);
            if player.on_ground {
                landed = true;
                break;
            }
        }
        assert!(
            highest > ground_y + 0.5,
            "jump only reached {} above the ground",
            highest - ground_y
        );
        assert!(landed, "player never landed after jumping");
        assert!(
            (player.pos.y - ground_y).abs() < 1e-4,
            "landed at y = {} instead of {}",
            player.pos.y,
            ground_y
        );
    }

    #[test]
    fn flight_ignores_gravity() {
        let world = flat_world(0);
        let mut player = Player::new(WorldVec::new(8.5, 10.0, 8.5));
        let mut input = PlayerInput {
            fly_toggle: true,
            ..PlayerInput::default()
        };
        tick(&world, &mut player, &input);
        assert!(player.flying, "fly toggle did not enable flight");
        let hover = player.pos;
        input.fly_toggle = false;
        for _ in 0..100 {
            tick(&world, &mut player, &input);
        }
        assert_eq!(player.pos, hover, "flying player drifted with no input");

        // Toggling back to walking restores gravity.
        input.fly_toggle = true;
        tick(&world, &mut player, &input);
        assert!(!player.flying);
        input.fly_toggle = false;
        for _ in 0..10 {
            tick(&world, &mut player, &input);
        }
        assert!(
            player.pos.y < hover.y - 0.1,
            "gravity did not resume after leaving flight"
        );
    }

    #[test]
    fn raycast_break_and_place() {
        let mut world = flat_world(0);
        let stone = world.registry().id_or_air(STONE);
        world.set_block(BlockPos::new(2, 2, 0), stone);
        let player = Player {
            pos: WorldVec::new(0.5, 1.0, 0.5),
            yaw: -std::f32::consts::FRAC_PI_2,
            ..Player::new(WorldVec::new(0.5, 1.0, 0.5))
        };

        let break_action = BlockAction {
            break_block: true,
            place_block: false,
            place: BlockId::AIR,
        };
        assert!(apply_block_actions(&mut world, &player, &break_action));
        assert_eq!(world.get_block(BlockPos::new(2, 2, 0)), BlockId::AIR);

        // Restore the target so the placement ray has a block to hit.
        world.set_block(BlockPos::new(2, 2, 0), stone);
        let place_action = BlockAction {
            break_block: false,
            place_block: true,
            place: stone,
        };
        assert!(apply_block_actions(&mut world, &player, &place_action));
        assert_eq!(world.get_block(BlockPos::new(1, 2, 0)), stone);

        // Looking straight down, the adjacent cell is inside the player, so
        // placement must be refused.
        let mut down = player.clone();
        down.pitch = -std::f32::consts::FRAC_PI_2;
        let before = world.get_block(BlockPos::new(0, 1, 0));
        assert!(!apply_block_actions(&mut world, &down, &place_action));
        assert_eq!(world.get_block(BlockPos::new(0, 1, 0)), before);
    }

    #[test]
    fn spawn_position_is_above_surface() {
        let mut world = flat_world(0);
        let stone = world.registry().id_or_air(STONE);
        world.set_block(BlockPos::new(3, 5, 4), stone);
        assert_eq!(world.height_at(3, 4), Some(5));
        let spawn = spawn_position(&world, 3, 4);
        assert_eq!(spawn.x, 3.5);
        assert_eq!(spawn.z, 4.5);
        assert!((spawn.y - 6.0).abs() < 1e-12);
        assert!(!aabb_collides(&world, &Player::new(spawn).aabb()));

        // A fluid surface is also cleared: the water top face is at y + 1.
        let water = world.registry().id_or_air(WATER);
        world.set_block(BlockPos::new(7, 1, 9), water);
        assert_eq!(world.height_at(7, 9), Some(1));
        let wet = spawn_position(&world, 7, 9);
        assert!((wet.y - 2.0).abs() < 1e-12);
        assert!(world.block_type(world.get_block(BlockPos::new(7, 1, 9))).fluid);
        assert!(!aabb_collides(&world, &Player::new(wet).aabb()));
    }

    #[test]
    fn swimming_rises_in_water() {
        let mut world = flat_world(0);
        let water = world.registry().id_or_air(WATER);
        for y in 1..=4 {
            for z in 0..16 {
                for x in 0..16 {
                    world.set_block(BlockPos::new(x, y, z), water);
                }
            }
        }
        let mut player = Player::new(WorldVec::new(8.5, 1.0, 8.5));
        let start = player.pos.y;
        let input = PlayerInput {
            jump: true,
            ..PlayerInput::default()
        };
        for _ in 0..10 {
            tick(&world, &mut player, &input);
        }
        assert!(
            player.pos.y > start + 0.2,
            "swimming did not rise: {} -> {}",
            start,
            player.pos.y
        );
    }

    #[test]
    fn thousand_tick_replay_is_deterministic() {
        let first_world = flat_world(1);
        let second_world = flat_world(1);
        let mut first = Player::new(WorldVec::new(8.5, 1.0, 8.5));
        let mut second = first.clone();
        let mut rng = Rng::new(0x0B1E_C7ED_5EED);
        for tick_index in 0..1_000 {
            let input = PlayerInput {
                forward: rng.next_f32() * 2.0 - 1.0,
                strafe: rng.next_f32() * 2.0 - 1.0,
                jump: rng.next_below(8) == 0,
                sprint: rng.next_below(2) == 0,
                sneak: rng.next_below(10) == 0,
                fly_toggle: tick_index == 500,
            };
            let yaw_delta = (rng.next_f32() - 0.5) * 0.04;
            first.yaw += yaw_delta;
            second.yaw += yaw_delta;
            step_player(&first_world, &mut first, &input, TICK_DT);
            step_player(&second_world, &mut second, &input, TICK_DT);
        }
        assert_eq!(first.pos.x.to_bits(), second.pos.x.to_bits());
        assert_eq!(first.pos.y.to_bits(), second.pos.y.to_bits());
        assert_eq!(first.pos.z.to_bits(), second.pos.z.to_bits());
        assert_eq!(first.velocity.x.to_bits(), second.velocity.x.to_bits());
        assert_eq!(first.velocity.y.to_bits(), second.velocity.y.to_bits());
        assert_eq!(first.velocity.z.to_bits(), second.velocity.z.to_bits());
        assert_eq!(first.on_ground, second.on_ground);
        assert_eq!(first.flying, second.flying);
    }

    #[test]
    fn new_player_starts_alive_and_full() {
        let player = Player::new(WorldVec::ZERO);
        assert_eq!(player.health, MAX_HEALTH);
        assert_eq!(player.hunger, MAX_HUNGER);
        assert!(!player.dead);
        assert_eq!(player.starvation_timer, 0.0);
        assert_eq!(player.regen_timer, 0.0);
    }

    #[test]
    fn step_events_report_landing_distance_sprint_and_jump() {
        let world = flat_world(0);
        let mut player = Player::new(WorldVec::new(8.5, 10.0, 8.5));
        let input = PlayerInput::default();
        let mut landing = StepEvents::default();
        for _ in 0..200 {
            landing = tick(&world, &mut player, &input);
            if player.on_ground {
                break;
            }
        }
        assert!(player.on_ground, "player did not land");
        assert!(
            landing.landing_impact > FALL_SAFE_IMPACT,
            "9 m fall impact was only {}",
            landing.landing_impact
        );
        // The impact speed is captured before the collision response zeroes it.
        assert_eq!(player.velocity.y, 0.0);
        assert_eq!(landing.moved_distance, 0.0);

        let grounded = tick(&world, &mut player, &input);
        assert_eq!(grounded.landing_impact, 0.0, "standing still counts as a landing");

        let walk = PlayerInput {
            forward: 1.0,
            sprint: true,
            ..PlayerInput::default()
        };
        let moved = tick(&world, &mut player, &walk);
        assert!(moved.moved_distance > 0.0, "walking reported no distance");
        assert!(moved.sprinted);
        assert!(!moved.jumped);

        let jump = PlayerInput {
            jump: true,
            ..PlayerInput::default()
        };
        let jumped = tick(&world, &mut player, &jump);
        assert!(jumped.jumped);
        assert_eq!(jumped.landing_impact, 0.0);
        assert!(!jumped.sprinted);
    }

    #[test]
    fn fall_damage_formula_and_death() {
        assert_eq!(FALL_SAFE_IMPACT, 13.0);
        assert_eq!(fall_damage(0.0), 0.0);
        assert_eq!(fall_damage(12.99), 0.0);
        assert_eq!(fall_damage(13.0), 0.0);
        assert_eq!(fall_damage(-5.0), 0.0);
        assert_eq!(fall_damage(f64::NAN), 0.0);
        assert!((fall_damage(20.0) - 5.6).abs() < 1e-6);
        assert_eq!(fall_damage(1_000.0), MAX_HEALTH);

        let mut player = Player::new(WorldVec::ZERO);
        let dealt = apply_fall_damage(&mut player, 20.0);
        assert!((dealt - 5.6).abs() < 1e-6);
        assert!((player.health - 14.4).abs() < 1e-6);
        assert!(!player.dead);
        assert_eq!(apply_fall_damage(&mut player, FALL_SAFE_IMPACT), 0.0);

        player.health = 3.0;
        let lethal = apply_fall_damage(&mut player, 40.0);
        assert_eq!(lethal, 3.0, "damage dealt must be clamped to remaining health");
        assert_eq!(player.health, 0.0);
        assert!(player.dead);
        assert_eq!(player.velocity, WorldVec::ZERO);
        assert_eq!(apply_fall_damage(&mut player, 40.0), 0.0, "dead players take no damage");
    }

    #[test]
    fn damage_marks_death_at_zero() {
        let mut player = Player::new(WorldVec::ZERO);
        player.health = 0.5;
        assert_eq!(apply_damage(&mut player, 1.0), 0.5);
        assert_eq!(player.health, 0.0);
        assert!(player.dead);
        assert_eq!(apply_damage(&mut player, 5.0), 0.0);
    }

    #[test]
    fn hunger_drain_is_bounded() {
        let mut player = Player::new(WorldVec::ZERO);
        advance_vitals(&mut player, &StepEvents::default(), 1.0);
        assert!((player.hunger - (MAX_HUNGER - HUNGER_IDLE_DRAIN)).abs() < 1e-4);

        let events = StepEvents {
            moved_distance: 10.0,
            jumped: true,
            ..StepEvents::default()
        };
        advance_vitals(&mut player, &events, 1.0);
        let expected = MAX_HUNGER - 2.0 * HUNGER_IDLE_DRAIN - 10.0 * HUNGER_DRAIN_PER_METRE - HUNGER_JUMP_COST;
        assert!((player.hunger - expected).abs() < 1e-4);

        // A huge movement event can only drain down to zero, never below.
        let mut sprinting = Player::new(WorldVec::ZERO);
        sprinting.hunger = 0.05;
        sprinting.health = 10.0;
        let events = StepEvents {
            moved_distance: 10_000.0,
            ..StepEvents::default()
        };
        advance_vitals(&mut sprinting, &events, 1.0);
        assert_eq!(sprinting.hunger, 0.0);
        assert_eq!(sprinting.health, 10.0, "starvation damage needs a full interval");
        assert!(!sprinting.dead);
    }

    #[test]
    fn starvation_damage_follows_four_second_cadence() {
        let mut player = Player::new(WorldVec::ZERO);
        player.hunger = 0.0;
        let events = StepEvents::default();
        for _ in 0..79 {
            advance_vitals(&mut player, &events, 0.05);
        }
        assert_eq!(player.health, 20.0, "starvation damage came early");
        advance_vitals(&mut player, &events, 0.05);
        assert_eq!(player.health, 19.0);
        for _ in 0..80 {
            advance_vitals(&mut player, &events, 0.05);
        }
        assert_eq!(player.health, 18.0);

        player.health = 2.0;
        player.starvation_timer = 0.0;
        for _ in 0..200 {
            advance_vitals(&mut player, &events, 0.05);
            if player.dead {
                break;
            }
        }
        assert!(player.dead, "starvation never killed the player");
        assert_eq!(player.health, 0.0);
        assert_eq!(player.velocity, WorldVec::ZERO);
    }

    #[test]
    fn regeneration_needs_high_hunger_and_heals_on_cadence() {
        let events = StepEvents::default();
        let mut player = Player::new(WorldVec::ZERO);
        player.hunger = MAX_HUNGER;
        player.health = 10.0;
        for _ in 0..79 {
            advance_vitals(&mut player, &events, 0.05);
        }
        assert_eq!(player.health, 10.0, "regeneration healed early");
        advance_vitals(&mut player, &events, 0.05);
        assert_eq!(player.health, 11.0);

        let mut hungry = Player::new(WorldVec::ZERO);
        hungry.hunger = REGEN_HUNGER_THRESHOLD - 0.1;
        hungry.health = 10.0;
        for _ in 0..160 {
            advance_vitals(&mut hungry, &events, 0.05);
        }
        assert_eq!(hungry.health, 10.0, "regenerated below the hunger threshold");
        assert!(hungry.hunger < REGEN_HUNGER_THRESHOLD - 0.1);
    }

    #[test]
    fn eating_requires_hunger_and_life() {
        let mut player = Player::new(WorldVec::ZERO);
        player.hunger = 19.0;
        assert!(eat(&mut player, 6));
        assert_eq!(player.hunger, MAX_HUNGER);
        assert!(!eat(&mut player, 6), "full players cannot eat");
        assert!(!eat(&mut player, 0), "zero-food items are not edible");

        player.hunger = 10.0;
        assert!(eat(&mut player, 6));
        assert_eq!(player.hunger, 16.0);
        assert!(eat(&mut player, 6));
        assert_eq!(player.hunger, 20.0);
        assert!(!eat(&mut player, 1));

        player.hunger = 5.0;
        player.dead = true;
        assert!(!eat(&mut player, 6));
        assert_eq!(player.hunger, 5.0);
    }

    #[test]
    fn reset_vitals_restores_survival_state() {
        let mut player = Player::new(WorldVec::new(1.0, 2.0, 3.0));
        player.health = 0.0;
        player.hunger = 0.0;
        player.dead = true;
        player.starvation_timer = 3.5;
        player.regen_timer = 2.0;
        reset_vitals(&mut player);
        assert_eq!(player.health, MAX_HEALTH);
        assert_eq!(player.hunger, MAX_HUNGER);
        assert!(!player.dead);
        assert_eq!(player.starvation_timer, 0.0);
        assert_eq!(player.regen_timer, 0.0);
        assert_eq!(player.pos, WorldVec::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn crouching_is_slower_and_lowers_the_eye() {
        let world = flat_world(0);
        let mut walker = Player::new(WorldVec::new(8.5, 1.0, 8.5));
        walker.on_ground = true;
        let mut sneaker = walker.clone();
        let walk = PlayerInput {
            forward: 1.0,
            ..PlayerInput::default()
        };
        let sneak = PlayerInput {
            forward: 1.0,
            sneak: true,
            ..PlayerInput::default()
        };
        for _ in 0..40 {
            tick(&world, &mut walker, &walk);
            tick(&world, &mut sneaker, &sneak);
        }
        assert!(!walker.sneaking);
        assert!(sneaker.sneaking);
        assert!(!sneaker.sprinting);
        let walk_eye = walker.eye().y - walker.pos.y;
        let sneak_eye = sneaker.eye().y - sneaker.pos.y;
        assert!((walk_eye - EYE_HEIGHT).abs() < 1e-9);
        assert!((sneak_eye - CROUCH_EYE_HEIGHT).abs() < 1e-9);
        let walk_speed = (walker.velocity.x * walker.velocity.x + walker.velocity.z * walker.velocity.z).sqrt();
        let sneak_speed = (sneaker.velocity.x * sneaker.velocity.x + sneaker.velocity.z * sneaker.velocity.z).sqrt();
        assert!(
            sneak_speed < walk_speed * 0.5,
            "sneak {sneak_speed} vs walk {walk_speed}"
        );
        let walk_travel = (walker.pos - WorldVec::new(8.5, 1.0, 8.5)).length();
        let sneak_travel = (sneaker.pos - WorldVec::new(8.5, 1.0, 8.5)).length();
        assert!(
            sneak_travel < walk_travel,
            "sneaking {sneak_travel} should cover less ground than walking {walk_travel}"
        );
    }

    #[test]
    fn sprinting_sets_state_and_is_faster_than_walking() {
        let world = flat_world(0);
        let mut walker = Player::new(WorldVec::new(8.5, 1.0, 8.5));
        walker.on_ground = true;
        let mut sprinter = walker.clone();
        let walk = PlayerInput {
            forward: 1.0,
            ..PlayerInput::default()
        };
        let sprint = PlayerInput {
            forward: 1.0,
            sprint: true,
            ..PlayerInput::default()
        };
        for _ in 0..40 {
            tick(&world, &mut walker, &walk);
            tick(&world, &mut sprinter, &sprint);
        }
        assert!(sprinter.sprinting);
        assert!(!walker.sprinting);
        let walk_speed = (walker.velocity.x * walker.velocity.x + walker.velocity.z * walker.velocity.z).sqrt();
        let sprint_speed =
            (sprinter.velocity.x * sprinter.velocity.x + sprinter.velocity.z * sprinter.velocity.z).sqrt();
        assert!(sprint_speed > walk_speed, "sprint {sprint_speed} vs walk {walk_speed}");
    }

    #[test]
    fn crouching_prevents_walking_off_a_ledge() {
        let registry = Arc::new(Registry::base());
        let air = ob_content::BlockId::AIR;
        let mut world = flat_world(0);
        // Carve a drop beyond x = 12 in the walking lane.
        for z in 6..11 {
            for x in 12..16 {
                world.set_block(BlockPos::new(x, 0, z), air);
            }
        }
        let _ = registry;
        let mut sneaker = Player::new(WorldVec::new(10.0, 1.0, 8.5));
        sneaker.on_ground = true;
        sneaker.yaw = -std::f32::consts::FRAC_PI_2; // faces +X
        let input = PlayerInput {
            forward: 1.0,
            sneak: true,
            ..PlayerInput::default()
        };
        for _ in 0..200 {
            tick(&world, &mut sneaker, &input);
        }
        assert!(
            sneaker.pos.y > 0.5,
            "sneaking player fell off the ledge at {:?}",
            sneaker.pos
        );
        // Minecraft-style edge safety lets the bounding box overhang the ledge,
        // so the centre may reach edge + half width before support is lost.
        assert!(
            sneaker.pos.x <= 12.0 + PLAYER_HALF_WIDTH + 1e-3,
            "sneaking player walked past the supported overhang to {}",
            sneaker.pos.x
        );

        let mut walker = Player::new(WorldVec::new(10.0, 1.0, 8.5));
        walker.on_ground = true;
        walker.yaw = -std::f32::consts::FRAC_PI_2;
        let walk = PlayerInput {
            forward: 1.0,
            ..PlayerInput::default()
        };
        for _ in 0..200 {
            tick(&world, &mut walker, &walk);
        }
        assert!(walker.pos.y < 0.5, "walking player should fall off the ledge");
    }

    /// Creates a full-health mob from the flat test world's registry.
    fn mob(world: &World, kind: MobKind, x: f64, y: f64, z: f64) -> Mob {
        Mob::new(world.registry().mob(kind), WorldVec::new(x, y, z))
    }

    /// A tiny ray target for `raycast_mob` tests.
    struct Target {
        id: u64,
        kind: MobKind,
        pos: WorldVec,
    }

    impl MobTarget for Target {
        fn mob_id(&self) -> u64 {
            self.id
        }

        fn mob_kind(&self) -> MobKind {
            self.kind
        }

        fn mob_pos(&self) -> WorldVec {
            self.pos
        }
    }

    #[test]
    fn mob_falls_gravity_settles_on_the_floor() {
        let world = flat_world(1);
        let player = WorldVec::new(8.5, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 8.5, 6.0, 8.5);
        for _ in 0..200 {
            step_mob(&world, &mut cow, player, true, TICK_DT);
            if cow.on_ground {
                break;
            }
        }
        assert!(cow.on_ground, "cow never landed");
        assert!((cow.pos.y - 1.0).abs() < 1e-4, "feet settled at {}", cow.pos.y);
        assert!(cow.velocity.y.abs() < 1e-9);
        assert!(!aabb_collides(&world, &cow.aabb(world.registry())));
        assert_eq!(cow.health, 10.0);
        assert_eq!(cow.kind, MobKind::Cow);
    }

    #[test]
    fn passive_wander_is_bounded_and_changes_position() {
        let world = flat_world(2);
        let player = WorldVec::new(40.5, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        let start = cow.pos;
        let mut distinct = std::collections::HashSet::new();
        let mut max_target_radius: f64 = 0.0;
        for _ in 0..300 {
            let events = step_mob(&world, &mut cow, player, true, TICK_DT);
            assert!(!events.attacked, "a passive cow must never attack");
            distinct.insert((cow.pos.x.to_bits(), cow.pos.z.to_bits()));
            if let MobAi::Wander { target, .. } = cow.ai {
                max_target_radius = max_target_radius.max(horizontal_distance(cow.pos, target));
            }
            assert!((cow.pos.y - 1.0).abs() < 1e-4, "cow left the floor at {}", cow.pos.y);
            assert!(!aabb_collides(&world, &cow.aabb(world.registry())));
        }
        assert!(distinct.len() > 10, "cow never moved ({} positions)", distinct.len());
        assert!(
            max_target_radius <= MOB_WANDER_MAX_RADIUS + 1e-6,
            "wander target was {max_target_radius} m away"
        );
        assert!(
            horizontal_distance(start, cow.pos) <= 24.0,
            "cow wandered {} m from its start",
            horizontal_distance(start, cow.pos)
        );
    }

    #[test]
    fn passive_mob_flees_when_hurt_then_recovers() {
        let world = flat_world(1);
        let player = WorldVec::new(6.5, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        cow.hurt_timer = MOB_HURT_FLEE_TIME;
        let first = step_mob(&world, &mut cow, player, true, TICK_DT);
        assert!(first.hurt, "the first hurt tick must report the flee reaction");
        assert!(matches!(cow.ai, MobAi::Flee { .. }));
        let distance_before = cow.pos.distance(player);
        for _ in 0..20 {
            step_mob(&world, &mut cow, player, true, TICK_DT);
        }
        assert!(
            cow.pos.distance(player) > distance_before,
            "cow did not run away: {} -> {}",
            distance_before,
            cow.pos.distance(player)
        );
        assert!(!matches!(cow.ai, MobAi::Idle), "cow stopped fleeing after one second");
        for _ in 0..100 {
            step_mob(&world, &mut cow, player, true, TICK_DT);
        }
        assert_eq!(cow.hurt_timer, 0.0);
        assert!(
            !matches!(cow.ai, MobAi::Flee { .. }),
            "flee reaction outlived the hurt timer"
        );

        // A scripted/future `Flee` state works on its own timer too.
        let mut scripted = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        scripted.ai = MobAi::Flee {
            from: player,
            timer: 1.0,
        };
        let before = scripted.pos.distance(player);
        for _ in 0..10 {
            step_mob(&world, &mut scripted, player, true, TICK_DT);
        }
        assert!(
            scripted.pos.distance(player) > before,
            "a constructed flee state must move away"
        );
        for _ in 0..40 {
            step_mob(&world, &mut scripted, player, true, TICK_DT);
        }
        assert!(!matches!(scripted.ai, MobAi::Flee { .. }), "flee timer must expire");
    }

    #[test]
    fn zombie_chases_and_attacks_with_a_cooldown() {
        let world = flat_world(1);
        let player = WorldVec::new(14.5, 1.0, 8.5);
        let mut zombie = mob(&world, MobKind::Zombie, 8.5, 1.0, 8.5);
        let start = zombie.pos.distance(player);
        let events = step_mob(&world, &mut zombie, player, true, TICK_DT);
        assert!(matches!(zombie.ai, MobAi::Chase { .. }));
        assert!(!events.attacked, "zombie attacked from 6 m");
        for _ in 0..20 {
            step_mob(&world, &mut zombie, player, true, TICK_DT);
        }
        assert!(
            zombie.pos.distance(player) < start,
            "zombie did not close distance: {start} -> {}",
            zombie.pos.distance(player)
        );

        // Place it inside attack range: one attack, then the cooldown blocks it.
        zombie.pos = WorldVec::new(13.0, 1.0, 8.5);
        zombie.attack_cooldown = 0.0;
        let hit = step_mob(&world, &mut zombie, player, true, TICK_DT);
        assert!(hit.attacked);
        assert_eq!(hit.damage, MOB_ATTACK_DAMAGE);
        assert!(hit.knockback.x > 0.0, "knockback must push away from the zombie");
        assert!((hit.knockback.y - MOB_KNOCKBACK_UP).abs() < 1e-9);
        let immediate = step_mob(&world, &mut zombie, player, true, TICK_DT);
        assert!(!immediate.attacked, "cooldown did not gate the second attack");
        let mut attacks = 0;
        for _ in 0..100 {
            let events = step_mob(&world, &mut zombie, player, true, TICK_DT);
            attacks += u32::from(events.attacked);
        }
        assert!(attacks >= 3, "only {attacks} attacks in five seconds");
    }

    #[test]
    fn passive_mobs_never_attack_and_creative_players_are_ignored() {
        let world = flat_world(0);
        let player = WorldVec::new(8.9, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        let mut zombie = mob(&world, MobKind::Zombie, 8.5, 1.0, 8.5);
        for _ in 0..100 {
            let events = step_mob(&world, &mut cow, player, true, TICK_DT);
            assert!(!events.attacked && events.damage == 0.0);
            let events = step_mob(&world, &mut zombie, player, false, TICK_DT);
            assert!(!events.attacked && events.damage == 0.0);
        }
        assert!(
            !matches!(zombie.ai, MobAi::Chase { .. }),
            "dead/creative player was chased"
        );
    }

    #[test]
    fn passive_mobs_stop_at_a_cliff_edge() {
        let mut world = flat_world(1);
        let air = BlockId::AIR;
        for z in -16..=31 {
            for x in 11..=31 {
                world.set_block(BlockPos::new(x, 0, z), air);
            }
        }
        let player = WorldVec::new(8.5, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 9.5, 1.0, 8.5);
        cow.ai = MobAi::Wander {
            target: WorldVec::new(20.5, 1.0, 8.5),
            timer: 100.0,
        };
        cow.on_ground = true;
        for _ in 0..150 {
            step_mob(&world, &mut cow, player, true, TICK_DT);
            assert!(!aabb_collides(&world, &cow.aabb(world.registry())));
            assert!(
                cow.pos.y > 0.9,
                "cow fell off the cliff at x = {} (y = {})",
                cow.pos.x,
                cow.pos.y
            );
        }
        assert!(
            cow.pos.x < 11.0,
            "cow walked past the supported edge to x = {}",
            cow.pos.x
        );
    }

    #[test]
    fn mob_step_is_deterministic() {
        let world_a = flat_world(1);
        let world_b = flat_world(1);
        let player = WorldVec::new(12.5, 1.0, 8.5);
        let mut zombie_a = mob(&world_a, MobKind::Zombie, 8.5, 1.0, 8.5);
        let mut zombie_b = mob(&world_b, MobKind::Zombie, 8.5, 1.0, 8.5);
        let mut cow_a = mob(&world_a, MobKind::Cow, 7.5, 1.0, 8.5);
        let mut cow_b = mob(&world_b, MobKind::Cow, 7.5, 1.0, 8.5);
        cow_a.hurt_timer = MOB_HURT_FLEE_TIME;
        cow_b.hurt_timer = MOB_HURT_FLEE_TIME;
        for _ in 0..200 {
            let za = step_mob(&world_a, &mut zombie_a, player, true, TICK_DT);
            let zb = step_mob(&world_b, &mut zombie_b, player, true, TICK_DT);
            let ca = step_mob(&world_a, &mut cow_a, player, true, TICK_DT);
            let cb = step_mob(&world_b, &mut cow_b, player, true, TICK_DT);
            assert_eq!(za, zb);
            assert_eq!(ca, cb);
        }
        assert_eq!(zombie_a.pos.x.to_bits(), zombie_b.pos.x.to_bits());
        assert_eq!(zombie_a.pos.y.to_bits(), zombie_b.pos.y.to_bits());
        assert_eq!(zombie_a.pos.z.to_bits(), zombie_b.pos.z.to_bits());
        assert_eq!(zombie_a.yaw.to_bits(), zombie_b.yaw.to_bits());
        assert_eq!(zombie_a.velocity, zombie_b.velocity);
        assert_eq!(zombie_a.ai, zombie_b.ai);
        assert_eq!(zombie_a.age.to_bits(), zombie_b.age.to_bits());
        assert_eq!(cow_a.pos, cow_b.pos);
        assert_eq!(cow_a.ai, cow_b.ai);
        assert_eq!(cow_a.hurt_timer.to_bits(), cow_b.hurt_timer.to_bits());
    }

    #[test]
    fn mobs_float_in_water_instead_of_sinking() {
        let mut world = flat_world(0);
        let water = world.registry().id_or_air(WATER);
        for y in 1..=4 {
            for z in 5..=11 {
                for x in 5..=11 {
                    world.set_block(BlockPos::new(x, y, z), water);
                }
            }
        }
        let player = WorldVec::new(8.5, 1.0, 8.5);
        let mut cow = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        let start_y = cow.pos.y;
        let mut highest = start_y;
        for _ in 0..100 {
            step_mob(&world, &mut cow, player, true, TICK_DT);
            highest = highest.max(cow.pos.y);
            assert!(!aabb_collides(&world, &cow.aabb(world.registry())));
        }
        assert!(
            highest > start_y + 0.2,
            "cow sank instead of floating (highest {highest}, start {start_y})"
        );
        assert!(cow.pos.y > start_y - 0.05, "cow sank below its start");
        assert!(
            world.block_type(world.get_block(BlockPos::new(8, 4, 8))).fluid,
            "water fixture must stay water"
        );
    }

    #[test]
    fn mobs_despawn_by_age_distance_and_the_void() {
        let world = flat_world(1);
        let player = WorldVec::new(8.5, 1.0, 8.5);

        let mut old = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        old.age = MOB_DESPAWN_AGE + 0.1;
        assert!(step_mob(&world, &mut old, player, true, TICK_DT).despawn);
        assert_eq!(old.ai, MobAi::Despawn);

        let mut far = mob(&world, MobKind::Cow, 8.5 + MOB_DESPAWN_DISTANCE + 1.0, 1.0, 8.5);
        assert!(step_mob(&world, &mut far, player, true, TICK_DT).despawn);

        let mut void = mob(&world, MobKind::Cow, 8.5, MOB_DESPAWN_Y - 1.0, 8.5);
        assert!(step_mob(&world, &mut void, player, true, TICK_DT).despawn);

        let mut fresh = mob(&world, MobKind::Cow, 8.5, 1.0, 8.5);
        assert!(!step_mob(&world, &mut fresh, player, true, TICK_DT).despawn);
    }

    #[test]
    fn raycast_mob_selects_the_nearest_aabb() {
        let registry = Registry::base();
        let mobs = vec![
            Target {
                id: 7,
                kind: MobKind::Cow,
                pos: WorldVec::new(8.5, 1.0, 8.5),
            },
            Target {
                id: 3,
                kind: MobKind::Pig,
                pos: WorldVec::new(8.5, 1.0, 5.5),
            },
        ];
        let origin = WorldVec::new(8.5, 1.5, 0.5);
        let forward = WorldVec::new(0.0, 0.0, 1.0);
        assert_eq!(raycast_mob(&registry, &mobs, origin, forward, 20.0), Some(3));
        assert_eq!(raycast_mob(&registry, &mobs, origin, forward, 2.0), None);
        assert_eq!(
            raycast_mob(&registry, &mobs, origin, WorldVec::new(0.0, 1.0, 0.0), 20.0),
            None
        );
        assert_eq!(raycast_mob(&registry, &mobs, origin, WorldVec::ZERO, 20.0), None);
        // Only the farther cow is selectable through a short reach.
        assert_eq!(raycast_mob(&registry, &mobs[..1], origin, forward, 20.0), Some(7));

        // Equal entry distance breaks the tie by ascending id.
        let tied = vec![
            Target {
                id: 9,
                kind: MobKind::Cow,
                pos: WorldVec::new(8.5, 1.0, 8.5),
            },
            Target {
                id: 4,
                kind: MobKind::Sheep,
                pos: WorldVec::new(8.5, 1.0, 8.5),
            },
        ];
        assert_eq!(raycast_mob(&registry, &tied, origin, forward, 20.0), Some(4));
    }
}

//! Authoritative player simulation: deterministic, headless movement and block interaction.
//!
//! The simulation is independent of the renderer and of wall-clock time. Player
//! positions and velocities are [`WorldVec`] (`f64`) metres; the body is an
//! axis-aligned box swept against the world's solid blocks on one axis at a time.
//! All decisions are made from the supplied [`PlayerInput`] and [`World`], so the
//! same input sequence always produces the same result.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles

use ob_content::BlockId;
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
/// Flying speed, in metres per second.
pub const FLY_SPEED: f64 = 12.0;
/// Maximum distance at which the player can break or place a block, in metres.
pub const REACH: f64 = 5.0;
/// Fixed simulation tick length, in seconds (20 Hz).
pub const TICK_DT: f64 = 1.0 / 20.0;

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
}

impl Player {
    /// Creates a motionless player whose feet are at `spawn`.
    pub fn new(spawn: WorldVec) -> Self {
        Self {
            pos: spawn,
            velocity: WorldVec::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            flying: false,
        }
    }

    /// World-space position of the camera eye.
    pub fn eye(&self) -> WorldVec {
        WorldVec::new(self.pos.x, self.pos.y + EYE_HEIGHT, self.pos.z)
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
pub fn step_player(world: &World, player: &mut Player, input: &PlayerInput, dt: f64) {
    if input.fly_toggle {
        player.flying = !player.flying;
    }

    let forward_input = f64::from(input.forward.clamp(-1.0, 1.0));
    let strafe_input = f64::from(input.strafe.clamp(-1.0, 1.0));
    let yaw = f64::from(player.yaw);
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let flat_forward = WorldVec::new(-sin_yaw, 0.0, -cos_yaw);
    let flat_right = WorldVec::new(cos_yaw, 0.0, -sin_yaw);
    let was_on_ground = player.on_ground;
    let in_water = !player.flying && player_in_water(world, player.pos);

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
        let mode_speed = if input.sprint { SPRINT_SPEED } else { WALK_SPEED };
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
        }
    }

    clamp_velocity(&mut player.velocity);

    let desired_x = player.velocity.x * dt;
    let desired_z = player.velocity.z * dt;
    player.on_ground = false;

    let blocked_y = sweep_axis(world, &mut player.pos, AXIS_Y, player.velocity.y * dt);
    if blocked_y {
        if player.velocity.y < 0.0 {
            player.on_ground = true;
        }
        player.velocity.y = 0.0;
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

    /// Runs one simulation tick with `input`.
    fn tick(world: &World, player: &mut Player, input: &PlayerInput) {
        step_player(world, player, input, TICK_DT);
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
}

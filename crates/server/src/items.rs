//! Authoritative dropped item entities.
//!
//! A broken block no longer transfers its loot straight into the inventory:
//! the host spawns one [`ItemEntity`] per dropped stack at the broken block's
//! centre, simulates a small deterministic impulse and gravity, and collects
//! the entity once the player is close enough and the pickup delay has
//! elapsed. The host owns the entities; clients read them through
//! [`Host::item_entities`](crate::Host::item_entities) and the pickup queue
//! through [`Host::take_pickup_events`](crate::Host::take_pickup_events).
//!
//! Physics is deliberately simple: the entity is a point probed slightly above
//! its base position, horizontal motion is blocked by solid blocks, and a
//! falling entity snaps to the top of the first solid block it would enter.
//! There is no buoyancy in water, no merging and no hazard destruction in this
//! slice.

use std::collections::HashSet;
use std::path::Path;

use ob_content::{ItemType, Registry};
use ob_math::{BlockPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
use ob_sim::{GRAVITY, Player};
use ob_world::World;

use crate::net_players::MAX_HOST_ENTITY_ID;
use crate::{ItemStack, MAX_STACK_SIZE, StableHasher};

/// Seconds after spawn before an item can be picked up (10 ticks at 20 Hz).
pub(crate) const ITEM_PICKUP_DELAY: f64 = 0.5;
/// Maximum lifetime of a dropped item in seconds (5 minutes).
pub(crate) const ITEM_DESPAWN_AGE: f64 = 300.0;
/// Dropped items below this height despawn (the void).
pub(crate) const ITEM_DESPAWN_Y: f64 = -64.0;
/// Radius around the player in which dropped items are attracted and collected.
pub(crate) const ITEM_PICKUP_RADIUS: f64 = 3.0;
/// Distance to the player's chest at which an attracted item is collected.
pub(crate) const ITEM_COLLECT_DISTANCE: f64 = 0.9;
/// Speed in metres per second at which attracted items fly toward the player.
pub(crate) const ITEM_MAGNET_SPEED: f64 = 7.0;
/// Small offset above a block top at which a settled item rests.
pub(crate) const ITEM_REST_OFFSET: f64 = 0.1;

/// Maximum horizontal spawn speed, in metres per second.
const SPAWN_HORIZONTAL_SPEED: f64 = 1.5;
/// Minimum upward spawn speed, in metres per second.
const SPAWN_UPWARD_MIN: f64 = 2.5;
/// Range added to the minimum upward spawn speed.
const SPAWN_UPWARD_RANGE: f64 = 1.5;
/// Largest movement resolved by one vertical collision sub-step, in metres.
const MAX_VERTICAL_SUBSTEP: f64 = 0.45;
/// Downward speed clamp, in metres per second, so a long fall cannot tunnel.
const MAX_FALL_SPEED: f64 = 40.0;
/// Probe offset above the entity base used to decide whether a block is solid.
const PROBE_OFFSET: f64 = 0.05;
/// Horizontal load clamp; keeps a malformed save inside `i32` block ranges.
const LOAD_HORIZONTAL_LIMIT: f64 = 30_000_000.0;
/// Velocity load clamp; valid drops never approach this.
const LOAD_VELOCITY_LIMIT: f64 = 100.0;

/// One dropped item stack in the world.
///
/// `pos` is the base/centre of the dropped item (a broken block's centre is
/// `block_pos + (0.5, 0.25, 0.5)`). Entities are ordered by ascending [`id`],
/// which is stable across saves.
///
/// [`id`]: ItemEntity::id
#[derive(Debug, Clone, PartialEq)]
pub struct ItemEntity {
    /// Entity id, unique among live entities, increasing within a session.
    pub id: u64,
    /// Base centre in world space, in metres.
    pub pos: WorldVec,
    /// Velocity in metres per second.
    pub velocity: WorldVec,
    /// The dropped stack.
    pub stack: ItemStack,
    /// Seconds alive; drives the pickup delay and the despawn timer.
    pub age: f64,
}

impl ItemEntity {
    /// Creates an entity at `pos` with the given velocity and stack.
    pub(crate) fn new(id: u64, pos: WorldVec, velocity: WorldVec, stack: ItemStack) -> Self {
        Self {
            id,
            pos,
            velocity,
            stack,
            age: 0.0,
        }
    }
}

/// Deterministic impulse for a newly spawned entity.
///
/// The velocity is a pure function of the world seed, the (already allocated)
/// entity id and the block position containing `pos`; no RNG state exists, so
/// two hosts replaying the same breaks produce identical drops. Each axis uses
/// an independent stable hash. The horizontal impulse is sampled per axis in
/// `[-1.5, 1.5]` m/s, projected onto the unit disk and scaled to at most 1.5 m/s;
/// the upward component is uniform in `[2.5, 4.0)` m/s.
pub(crate) fn spawn_velocity(seed: u64, id: u64, pos: WorldVec) -> WorldVec {
    let block = BlockPos::new(pos.x.floor() as i32, pos.y.floor() as i32, pos.z.floor() as i32);
    let hash = |salt: u8| {
        let mut hasher = StableHasher::new();
        hasher.write_u64(seed);
        hasher.write_u64(id);
        hasher.write_i32(block.x);
        hasher.write_i32(block.y);
        hasher.write_i32(block.z);
        hasher.write_u8(salt);
        hasher.finish()
    };
    let signed = |value: u64| unit_interval(value) * 2.0 - 1.0;
    let (mut horizontal_x, mut horizontal_z) = (signed(hash(0x11)), signed(hash(0x33)));
    // Project the sample onto the unit disk so the horizontal speed never
    // exceeds SPAWN_HORIZONTAL_SPEED, while axis-aligned impulses can still
    // reach the full spread.
    let length_squared = horizontal_x * horizontal_x + horizontal_z * horizontal_z;
    if length_squared > 1.0 {
        let inverse = 1.0 / length_squared.sqrt();
        horizontal_x *= inverse;
        horizontal_z *= inverse;
    }
    WorldVec::new(
        horizontal_x * SPAWN_HORIZONTAL_SPEED,
        SPAWN_UPWARD_MIN + unit_interval(hash(0x22)) * SPAWN_UPWARD_RANGE,
        horizontal_z * SPAWN_HORIZONTAL_SPEED,
    )
}

/// Maps the top 53 bits of `hash` to `[0, 1)`.
fn unit_interval(hash: u64) -> f64 {
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// Advances one entity by `dt` seconds (the host uses the fixed 20 Hz tick).
///
/// Fall speed is clamped so the point collision probe cannot tunnel through a
/// one-block floor; a landing zeroes the whole velocity ("settled"). Water is
/// not handled specially: dropped items sink and rest on the bottom.
pub(crate) fn step_item(world: &World, entity: &mut ItemEntity, dt: f64) {
    entity.age += dt;
    entity.velocity.y = (entity.velocity.y - GRAVITY * dt).max(-MAX_FALL_SPEED);
    let friction = 0.85_f64.powf(dt * 20.0);
    entity.velocity.x *= friction;
    entity.velocity.z *= friction;

    let dx = entity.velocity.x * dt;
    if dx != 0.0 {
        let mut next = entity.pos;
        next.x += dx;
        if blocked(world, next) {
            entity.velocity.x = 0.0;
        } else {
            entity.pos = next;
        }
    }
    let dz = entity.velocity.z * dt;
    if dz != 0.0 {
        let mut next = entity.pos;
        next.z += dz;
        if blocked(world, next) {
            entity.velocity.z = 0.0;
        } else {
            entity.pos = next;
        }
    }

    let dy = entity.velocity.y * dt;
    if dy != 0.0 {
        move_vertical(world, entity, dy);
    }
}

/// Whether the point at `pos` (with a small upward probe offset) is inside a
/// solid block.
fn blocked(world: &World, pos: WorldVec) -> bool {
    let block = BlockPos::new(
        pos.x.floor() as i32,
        (pos.y + PROBE_OFFSET).floor() as i32,
        pos.z.floor() as i32,
    );
    world.is_solid(block)
}

/// Moves vertically in bounded sub-steps so a fast fall cannot skip a floor.
fn move_vertical(world: &World, entity: &mut ItemEntity, delta_y: f64) {
    let steps = (delta_y.abs() / MAX_VERTICAL_SUBSTEP).ceil().max(1.0) as usize;
    let step = delta_y / steps as f64;
    for _ in 0..steps {
        let mut next = entity.pos;
        next.y += step;
        if blocked(world, next) {
            if step < 0.0 {
                // Snap to the top of the block the probe entered and come to
                // rest with the standard offset. Landing settles horizontally
                // too, matching the Minecraft-like "item is done moving" look.
                let block_y = (next.y + PROBE_OFFSET).floor() as i32;
                entity.pos.y = block_y as f64 + 1.0 + ITEM_REST_OFFSET;
                entity.velocity = WorldVec::ZERO;
            } else {
                entity.velocity.y = 0.0;
            }
            return;
        }
        entity.pos = next;
    }
}

/// Whether `entity` is within the player's pickup radius.
///
/// Distance is measured from the player's chest to the entity position; items
/// inside [`ITEM_PICKUP_RADIUS`] are attracted toward the player and collected
/// once they reach [`ITEM_COLLECT_DISTANCE`].
pub(crate) fn can_pick_up(player: &Player, entity: &ItemEntity) -> bool {
    player_chest(player).distance(entity.pos) <= ITEM_PICKUP_RADIUS
}

/// Player chest position used as the magnet/pickup target.
pub(crate) fn player_chest(player: &Player) -> WorldVec {
    WorldVec::new(player.pos.x, player.pos.y + 0.9, player.pos.z)
}

/// Moves an attracted item one step toward the player's chest.
///
/// Returns the distance remaining after the step. The caller still applies the
/// collect-distance test, so this only runs for entities inside the radius.
pub(crate) fn attract_item(player: &Player, entity: &mut ItemEntity, dt: f64) -> f64 {
    let target = player_chest(player);
    let delta = target - entity.pos;
    let distance = delta.length();
    if distance > f64::EPSILON {
        let step = (ITEM_MAGNET_SPEED * dt).min(distance);
        entity.pos += delta * (step / distance);
        entity.velocity = WorldVec::ZERO;
    }
    distance - (ITEM_MAGNET_SPEED * dt).min(distance)
}

/// Serializes the entities to deterministic, header-free `items.txt` lines.
///
/// One line per entity, sorted by id:
/// `id x y z vx vy vz item count durability age`. Floats use the full-precision
/// `Debug` representation; `durability` is `-` for `None`.
pub(crate) fn items_state(registry: &Registry, entities: &[ItemEntity]) -> String {
    let mut ordered: Vec<&ItemEntity> = entities.iter().collect();
    ordered.sort_by_key(|entity| entity.id);
    let mut text = String::new();
    for entity in ordered {
        if entity.stack.count == 0 || entity.stack.item.0 as usize >= registry.item_len() {
            log::warn!(
                "items: skipping entity {} with an invalid stack while saving",
                entity.id
            );
            continue;
        }
        let name = registry.item(entity.stack.item).id;
        let durability = match entity.stack.durability {
            Some(remaining) => remaining.to_string(),
            None => "-".to_string(),
        };
        text.push_str(&format!(
            "{} {:?} {:?} {:?} {:?} {:?} {:?} {} {} {} {:?}\n",
            entity.id,
            entity.pos.x,
            entity.pos.y,
            entity.pos.z,
            entity.velocity.x,
            entity.velocity.y,
            entity.velocity.z,
            name,
            entity.stack.count,
            durability,
            entity.age,
        ));
    }
    text
}

/// Loads `items.txt`; a missing file (old worlds) means no entities.
///
/// Malformed lines, duplicate ids and unknown item names are skipped with a
/// warning. Counts clamp to the item's `max_stack`, positions clamp to the
/// world's vertical range and a conservative horizontal range, velocities
/// clamp to a sane bound, and negative ages clamp to zero. Entity ids are
/// preserved; the caller derives `next_item_id` from the returned list.
pub(crate) fn load_items(world_dir: &Path, registry: &Registry) -> Vec<ItemEntity> {
    let path = world_dir.join("items.txt");
    let text = match crate::read_sidecar(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not read {}: {err}; starting with no dropped items",
                    path.display()
                );
            }
            return Vec::new();
        }
    };

    let mut entities: Vec<ItemEntity> = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 11 {
            log::warn!("items.txt:{line_number}: ignoring malformed line");
            continue;
        }
        let Ok(id) = fields[0].parse::<u64>() else {
            log::warn!("items.txt:{line_number}: invalid entity id '{}'", fields[0]);
            continue;
        };
        if id > MAX_HOST_ENTITY_ID {
            log::warn!(
                "items.txt:{line_number}: entity id {id} is outside the host id space (0..={MAX_HOST_ENTITY_ID}), dropping it"
            );
            continue;
        }
        if !seen.insert(id) {
            log::warn!("items.txt:{line_number}: duplicate entity id {id}, dropping it");
            continue;
        }

        let parse = |field: &str| field.parse::<f64>().ok().filter(|value| value.is_finite());
        let (Some(pos_x), Some(pos_y), Some(pos_z)) = (parse(fields[1]), parse(fields[2]), parse(fields[3])) else {
            log::warn!("items.txt:{line_number}: invalid position");
            continue;
        };
        let (Some(vel_x), Some(vel_y), Some(vel_z)) = (parse(fields[4]), parse(fields[5]), parse(fields[6])) else {
            log::warn!("items.txt:{line_number}: invalid velocity");
            continue;
        };
        let Some(age) = parse(fields[10]) else {
            log::warn!("items.txt:{line_number}: invalid age '{}'", fields[10]);
            continue;
        };

        let Some(item) = registry.item_id(fields[7]) else {
            log::warn!("items.txt:{line_number}: unknown item '{}', dropping it", fields[7]);
            continue;
        };
        let Ok(count) = fields[8].parse::<u32>() else {
            log::warn!("items.txt:{line_number}: invalid count '{}'", fields[8]);
            continue;
        };
        let durability = match fields[9] {
            "-" => None,
            text => match text.parse::<u32>() {
                Ok(value) => Some(value),
                Err(_) => {
                    log::warn!("items.txt:{line_number}: invalid durability '{text}'");
                    continue;
                }
            },
        };

        let item_type: &ItemType = registry.item(item);
        let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let count = count.clamp(1, u32::from(max_stack)) as u8;
        let mut stack = ItemStack::for_item(item, item_type, count);
        let durability_limit = item_type
            .tool
            .map(|spec| spec.durability)
            .or_else(|| item_type.armor.map(|spec| spec.durability));
        stack.durability = match (durability, durability_limit) {
            (Some(value), Some(max)) => Some(value.clamp(1, u32::from(max)) as u16),
            // A tool/armor line without the optional field starts undamaged.
            (None, Some(max)) => Some(max),
            // Non-durable items carry no durability component, matching
            // `sanitize_stack`; a hostile sidecar cannot attach one here.
            (Some(_), None) => {
                log::warn!(
                    "items.txt:{line_number}: item '{}' has no durability, ignoring it",
                    fields[7]
                );
                None
            }
            (None, None) => None,
        };

        let pos = WorldVec::new(
            pos_x.clamp(-LOAD_HORIZONTAL_LIMIT, LOAD_HORIZONTAL_LIMIT),
            pos_y.clamp(WORLD_MIN_Y as f64, WORLD_MAX_Y as f64),
            pos_z.clamp(-LOAD_HORIZONTAL_LIMIT, LOAD_HORIZONTAL_LIMIT),
        );
        let velocity = WorldVec::new(
            vel_x.clamp(-LOAD_VELOCITY_LIMIT, LOAD_VELOCITY_LIMIT),
            vel_y.clamp(-LOAD_VELOCITY_LIMIT, LOAD_VELOCITY_LIMIT),
            vel_z.clamp(-LOAD_VELOCITY_LIMIT, LOAD_VELOCITY_LIMIT),
        );
        let age = age.clamp(0.0, ITEM_DESPAWN_AGE);
        entities.push(ItemEntity::new(id, pos, velocity, stack));
        // `new` starts at age zero; restore the loaded age.
        if let Some(entity) = entities.last_mut() {
            entity.age = age;
        }
    }
    entities.sort_by_key(|entity| entity.id);
    entities
}

/// The id the next spawned entity must receive (`max id + 1`).
pub(crate) fn next_item_id(entities: &[ItemEntity]) -> u64 {
    entities
        .iter()
        .map(|entity| entity.id)
        .max()
        .map_or(1, |max| max.saturating_add(1))
}

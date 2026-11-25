//! Authoritative mob population, natural spawning, combat, drops and saves.
//!
//! The interim 1.0 slice keeps mobs deliberately simple: five code-defined
//! species, a tiny deterministic brain in [`ob_sim::step_mob`], straight-line
//! movement, melee combat and a fixed drop table. There is no pathfinding,
//! line-of-sight, projectile, breeding, villager or status-effect simulation.
//!
//! The host owns every mob. Clients read [`Host::mobs`](crate::Host::mobs),
//! select a target with [`Host::mob_hit_by_ray`](crate::Host::mob_hit_by_ray)
//! and send an attack intent through
//! [`Host::attack_mob`](crate::Host::attack_mob); they never write mob state.
//!
//! # Ticking and loaded chunks
//!
//! A mob in a loaded chunk column is stepped and can attack or despawn. A mob
//! whose column is not resident is *frozen*: it is not stepped, does not age,
//! cannot attack and contributes its state to saves and the state hash. It
//! resumes exactly where it left off when the column streams back in.
//!
//! # Spawning
//!
//! Every [`MOB_SPAWN_INTERVAL_TICKS`] ticks the host attempts
//! [`MOB_SPAWN_ATTEMPTS_PER_CATEGORY`] positions per category. Candidates are
//! deterministic hashes of the world seed, tick and attempt; there is no RNG
//! state. Passive animals spawn on a grass-topped surface column 12..40 m from
//! the player, up to [`MAX_PASSIVE_MOBS`]. Hostiles spawn 16..48 m away, up to
//! [`MAX_HOSTILE_MOBS`], at night (tick phase 13,000..23,000) on any solid
//! surface column, or within 16 m vertically of a player below Y=40 on a cave
//! floor. Both categories need two air cells above the floor and a loaded
//! column, and no candidate is closer than 8 m to the player.
//!
//! # Daylight
//!
//! The day phase is `tick_count % 24_000`, matching the existing client
//! day/night cycle. Phases `0..12_000` are day and `13_000..23_000` are night;
//! the two 1,000-tick dusk/dawn bands spawn no hostiles. A hostile zombie in
//! direct sky light during the day loses [`MOB_BURN_DAMAGE_PER_SECOND`] health
//! per second; it dies with its normal (empty) drop table.
//!
//! # Persistence
//!
//! `mobs.txt` is a header-free sidecar with one line per mob:
//! `id kind x y z yaw health age`. Rows are written in ascending id order.
//! AI intent, velocity, hurt and attack timers are transient and restart at
//! idle/full cooldown after a load. The state hash covers id, kind, quantised
//! position, yaw, health and age, so save/reload reproduces it exactly.

use std::collections::HashSet;
use std::path::Path;

use ob_content::{ItemId, MobKind, Registry};
use ob_math::{BlockPos, ChunkPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec, mix64};
use ob_sim::{MOB_HURT_FLEE_TIME, Mob, MobAi, MobTarget, REACH, TICK_DT, aabb_collides, raycast_mob, step_mob};
use ob_world::World;

use crate::net_players::MAX_HOST_ENTITY_ID;
use crate::{GameMode, Host, ItemStack, StableHasher, quantize};

/// World-directory filename for the mob sidecar.
pub(crate) const MOBS_FILE: &str = "mobs.txt";
/// Natural passive (creature) mob cap.
pub const MAX_PASSIVE_MOBS: usize = 10;
/// Natural hostile (monster) mob cap.
pub const MAX_HOSTILE_MOBS: usize = 8;
/// Ticks between natural-spawn batches (one second at 20 Hz).
pub const MOB_SPAWN_INTERVAL_TICKS: u64 = 20;
/// Spawn positions attempted per category per batch.
pub const MOB_SPAWN_ATTEMPTS_PER_CATEGORY: usize = 2;
/// Closest natural passive spawn to the player, in metres.
pub const PASSIVE_SPAWN_MIN_DISTANCE: f64 = 12.0;
/// Farthest natural passive spawn from the player, in metres.
pub const PASSIVE_SPAWN_MAX_DISTANCE: f64 = 40.0;
/// Closest natural hostile spawn to the player, in metres.
pub const HOSTILE_SPAWN_MIN_DISTANCE: f64 = 16.0;
/// Farthest natural hostile spawn from the player, in metres.
pub const HOSTILE_SPAWN_MAX_DISTANCE: f64 = 48.0;
/// Hard minimum distance for any natural spawn (a defensive rule independent
/// of the per-category ranges).
pub const MOB_SPAWN_MIN_DISTANCE: f64 = 8.0;
/// Zombies burn in daylight for this much damage per second.
pub const MOB_BURN_DAMAGE_PER_SECOND: f32 = 1.0;
/// Extra reach beyond [`REACH`] allowed for a melee hit at a mob AABB, in metres.
pub const MOB_ATTACK_REACH: f64 = REACH + 0.5;
/// Ticks in one day/night cycle.
pub const DAY_LENGTH_TICKS: u64 = 24_000;
/// First tick phase that counts as day.
pub const DAY_START_TICK: u64 = 0;
/// Exclusive end of the day phase. Dusk follows.
pub const DAY_END_TICK: u64 = 12_000;
/// First tick phase that counts as night.
pub const NIGHT_START_TICK: u64 = 13_000;
/// Exclusive end of the night phase. Dawn follows.
pub const NIGHT_END_TICK: u64 = 23_000;
/// Vertical player coordinate below which hostiles may spawn underground.
pub const HOSTILE_CAVE_PLAYER_Y: f64 = 40.0;
/// Vertical search radius for a cave floor around an underground player.
const CAVE_SEARCH_VERTICAL: i32 = 16;

/// One live mob in the host's authoritative population.
#[derive(Debug, Clone, PartialEq)]
pub struct MobEntity {
    /// Stable session id, unique among live mobs and increasing within a save.
    pub id: u64,
    /// Simulation state (position, health, AI, timers).
    pub mob: Mob,
}

impl MobEntity {
    /// Creates a live entity from an already-built mob state.
    pub fn new(id: u64, mob: Mob) -> Self {
        Self { id, mob }
    }
}

impl MobTarget for MobEntity {
    fn mob_id(&self) -> u64 {
        self.id
    }

    fn mob_kind(&self) -> MobKind {
        self.mob.kind
    }

    fn mob_pos(&self) -> WorldVec {
        self.mob.pos
    }
}

/// Result of one player melee attack against a mob.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttackOutcome {
    /// Target id supplied by the caller.
    pub id: u64,
    /// Whether the target existed and was within reach.
    pub hit: bool,
    /// Health actually removed (clamped to the remaining health).
    pub damage: f32,
    /// Whether this hit removed the mob.
    pub killed: bool,
    /// Deterministic drops spawned as item entities by this kill.
    pub drops: Vec<ItemStack>,
    /// The selected tool that broke on this hit, in survival mode.
    pub tool_broke: Option<ItemId>,
}

impl Host {
    /// The live mob population, in ascending id order.
    pub fn mobs(&self) -> &[MobEntity] {
        &self.mobs
    }

    /// Number of live mobs (passive and hostile).
    pub fn mob_count(&self) -> usize {
        self.mobs.len()
    }

    /// One live mob by session id.
    pub fn mob_entity(&self, id: u64) -> Option<&MobEntity> {
        self.mobs.iter().find(|entity| entity.id == id)
    }

    /// Number of live passive mobs (creature cap accounting).
    pub fn passive_mob_count(&self) -> usize {
        self.mobs
            .iter()
            .filter(|entity| !self.registry.mob(entity.mob.kind).hostile)
            .count()
    }

    /// Number of live hostile mobs (monster cap accounting).
    pub fn hostile_mob_count(&self) -> usize {
        self.mobs
            .iter()
            .filter(|entity| self.registry.mob(entity.mob.kind).hostile)
            .count()
    }

    /// The current day phase, `tick_count % 24_000`.
    pub fn day_phase(&self) -> u64 {
        self.tick_count % DAY_LENGTH_TICKS
    }

    /// Whether the tick phase is daylight (`0..12_000`).
    pub fn is_day(&self) -> bool {
        (DAY_START_TICK..DAY_END_TICK).contains(&self.day_phase())
    }

    /// Whether the tick phase is night (`13_000..23_000`).
    pub fn is_night(&self) -> bool {
        (NIGHT_START_TICK..NIGHT_END_TICK).contains(&self.day_phase())
    }

    /// Spawns one mob directly, bypassing natural-spawn rules.
    ///
    /// This is the command/debug/test entry point: it does not consult caps,
    /// distance rules, daylight or the floor. It still requires a finite
    /// position inside the world, a loaded chunk column and a collision-free
    /// species AABB. Returns the new session id, or `None` when the position is
    /// invalid. The initial AI state is idle; natural drops and combat then
    /// work exactly as for a naturally spawned mob.
    pub fn spawn_mob(&mut self, kind: MobKind, pos: WorldVec) -> Option<u64> {
        if !pos.is_finite()
            || pos.y < f64::from(WORLD_MIN_Y)
            || pos.y >= f64::from(WORLD_MAX_Y)
            || self.world.chunk(chunk_of(pos)).is_none()
        {
            return None;
        }
        let spec = *self.registry.mob(kind);
        let mob = Mob::new(&spec, pos);
        if aabb_collides(&self.world, &mob.aabb(&self.registry)) {
            return None;
        }
        let id = self.next_mob_id.max(1);
        self.next_mob_id = id.saturating_add(1);
        self.mobs.push(MobEntity::new(id, mob));
        self.mark_player_dirty();
        Some(id)
    }

    /// Selects the nearest mob under a ray, using the shared AABB helper.
    ///
    /// `origin` is normally [`Player::eye`](ob_sim::Player::eye) and
    /// `direction` the camera forward vector. Pass [`REACH`] (or the client's
    /// own crosshair reach) as `max_distance`; the host's attack validation
    /// uses [`MOB_ATTACK_REACH`] instead.
    pub fn mob_hit_by_ray(&self, origin: WorldVec, direction: WorldVec, max_distance: f64) -> Option<u64> {
        raycast_mob(&self.registry, &self.mobs, origin, direction, max_distance)
    }

    /// Applies one player melee attack to a mob.
    ///
    /// The attack hits when the player is alive and the distance from the eye
    /// to the mob's AABB is at most [`MOB_ATTACK_REACH`] metres. Damage is the
    /// selected item's `attack_damage` (hand/non-tool = 1.0). A hit sets the
    /// mob's hurt timer, applies knockback and, in survival, wears the selected
    /// tool by one durability (removing it at zero and reporting
    /// [`AttackOutcome::tool_broke`]). A kill spawns the species' fixed drops as
    /// ordinary item entities at the mob's centre and removes it before the
    /// next tick; creative mode deals damage but wears no tools.
    pub fn attack_mob(&mut self, id: u64) -> AttackOutcome {
        let mut outcome = AttackOutcome {
            id,
            ..AttackOutcome::default()
        };
        if self.player.dead {
            return outcome;
        }
        let Some(index) = self.mobs.iter().position(|entity| entity.id == id) else {
            return outcome;
        };
        let aabb = self.mobs[index].mob.aabb(&self.registry);
        if point_aabb_distance(self.player.eye(), &aabb) > MOB_ATTACK_REACH {
            return outcome;
        }
        let damage = self
            .registry
            .attack_damage(self.selected_stack().map(|stack| stack.item));
        if !damage.is_finite() || damage <= 0.0 {
            return outcome;
        }

        let health_before = self.mobs[index].mob.health.max(0.0);
        let dealt = damage.min(health_before);
        let position = self.mobs[index].mob.pos;
        let height = f64::from(self.registry.mob(self.mobs[index].mob.kind).height);
        let kind = self.mobs[index].mob.kind;
        let knockback = knockback_direction(position, self.player.pos);
        {
            let mob = &mut self.mobs[index].mob;
            mob.health = (health_before - dealt).max(0.0);
            mob.hurt_timer = MOB_HURT_FLEE_TIME;
            mob.velocity.x += knockback.x * 5.0;
            mob.velocity.z += knockback.z * 5.0;
            mob.velocity.y = mob.velocity.y.max(4.0);
        }
        outcome.hit = true;
        outcome.damage = dealt;

        if self.mobs[index].mob.health <= 0.0 {
            outcome.killed = true;
            self.mobs.remove(index);
            let drop_pos = WorldVec::new(position.x, position.y + height * 0.5, position.z);
            let spec = *self.registry.mob(kind);
            for &(item, count) in spec.drops {
                let stack = ItemStack::new(item, count);
                self.spawn_item_entity(drop_pos, stack);
                outcome.drops.push(stack);
            }
        }

        // Tool wear is survival-only, one point per landed hit. It applies even
        // when the hit killed the mob, matching block-break wear.
        if self.game_mode == GameMode::Survival
            && let Some(stack) = self.selected_stack()
            && let Some(spec) = self.registry.item(stack.item).tool
        {
            let remaining = stack.durability.unwrap_or(spec.durability);
            if remaining <= 1 {
                self.inventory[self.hotbar_selection] = None;
                outcome.tool_broke = Some(stack.item);
            } else {
                self.inventory[self.hotbar_selection] = Some(stack.with_durability(Some(remaining - 1)));
            }
            self.mark_player_dirty();
        }
        self.mark_player_dirty();
        outcome
    }

    /// Advances the mob population one tick and runs natural spawning.
    ///
    /// Mobs in unloaded columns are frozen. Hostile attacks are collected first
    /// and then applied through [`Host::damage_with_outcome`] so armor, death
    /// and creative immunity keep their existing semantics. Zombies exposed to
    /// the sky during the day lose health over time; a burn death resolves the
    /// normal drop table (empty for zombies) and removes the mob.
    pub(crate) fn tick_mobs(&mut self) {
        let targets = self.mob_targets();
        let burning_day = self.is_day();
        let mut pending_attacks: Vec<(Option<u64>, f32, WorldVec)> = Vec::new();
        let mut removed: HashSet<u64> = HashSet::new();
        let mut burn_deaths: Vec<(WorldVec, MobKind)> = Vec::new();

        for entity in &mut self.mobs {
            if self.world.chunk(chunk_of(entity.mob.pos)).is_none() {
                continue;
            }
            let Some((target_id, player_pos, player_alive)) = select_target(&targets, entity.mob.pos) else {
                continue;
            };
            let events = step_mob(&self.world, &mut entity.mob, player_pos, player_alive, TICK_DT);
            if events.attacked && player_alive {
                pending_attacks.push((target_id, events.damage, events.knockback));
            }
            if events.despawn {
                removed.insert(entity.id);
                continue;
            }
            let spec = *self.registry.mob(entity.mob.kind);
            if burning_day
                && spec.hostile
                && sky_exposed(&self.world, entity.mob.pos, spec.height)
                && entity.mob.health > 0.0
            {
                entity.mob.health -= MOB_BURN_DAMAGE_PER_SECOND * TICK_DT as f32;
                if entity.mob.health <= 0.0 {
                    entity.mob.health = 0.0;
                    removed.insert(entity.id);
                    burn_deaths.push((entity.mob.pos, entity.mob.kind));
                }
            }
        }

        for (target_id, damage, knockback) in pending_attacks {
            if let Some(player_id) = target_id {
                // Network movement is client-authoritative, so the server
                // cannot apply knockback to a network player's position. It
                // relays the damage; the client may add its own reaction.
                let _ = self.damage_network_player(player_id, damage, "mob");
                continue;
            }
            if self.player.dead {
                break;
            }
            let outcome = self.damage_with_outcome(damage);
            if outcome.dealt > 0.0 && !self.player.dead {
                self.player.velocity += knockback;
            }
        }

        if !removed.is_empty() {
            self.mobs.retain(|entity| !removed.contains(&entity.id));
            for id in &removed {
                self.network_entity_removals.push(crate::net_players::RemovedEntity {
                    id: *id,
                    item: false,
                    reason: ob_protocol::DespawnReason::Despawned,
                });
            }
        }
        for (position, kind) in burn_deaths {
            let spec = *self.registry.mob(kind);
            let drop_pos = WorldVec::new(position.x, position.y + f64::from(spec.height) * 0.5, position.z);
            for &(item, count) in spec.drops {
                self.spawn_item_entity(drop_pos, ItemStack::new(item, count));
            }
        }

        if self.tick_count.is_multiple_of(MOB_SPAWN_INTERVAL_TICKS) {
            self.try_spawn_mobs();
        }
    }

    /// Network-aware target list: `(player id, feet position, targetable)`.
    ///
    /// Single-player uses the host's internal player (id `None`); network mode
    /// uses every connected network player, with creative and dead players
    /// kept only as despawn-distance reference points.
    fn mob_targets(&self) -> Vec<(Option<u64>, WorldVec, bool)> {
        if self.network_players.is_empty() {
            return vec![(
                None,
                self.player.pos,
                self.game_mode == GameMode::Survival && !self.player.dead,
            )];
        }
        self.network_players
            .values()
            .map(|player| {
                (
                    Some(player.id),
                    player.pos,
                    player.game_mode == GameMode::Survival && !player.dead,
                )
            })
            .collect()
    }

    /// Position natural spawning is anchored to and whether its player is
    /// alive/targetable.
    fn spawn_anchor(&self) -> (WorldVec, bool) {
        if self.network_players.is_empty() {
            return (self.player.pos, !self.player.dead);
        }
        if let Some(player) = self.network_players.values().find(|player| !player.dead) {
            return (player.pos, true);
        }
        self.network_players
            .values()
            .next()
            .map_or((self.player.pos, false), |player| (player.pos, false))
    }

    /// One natural-spawn batch: at most two attempts per eligible category.
    pub(crate) fn try_spawn_mobs(&mut self) {
        let (anchor, targetable) = self.spawn_anchor();
        if self.is_day() {
            for attempt in 0..MOB_SPAWN_ATTEMPTS_PER_CATEGORY {
                if self.passive_mob_count() >= MAX_PASSIVE_MOBS {
                    break;
                }
                let _ = self.attempt_natural_spawn(false, attempt, anchor);
            }
        }
        if (self.is_night() || anchor.y < HOSTILE_CAVE_PLAYER_Y) && targetable {
            for attempt in 0..MOB_SPAWN_ATTEMPTS_PER_CATEGORY {
                if self.hostile_mob_count() >= MAX_HOSTILE_MOBS {
                    break;
                }
                let _ = self.attempt_natural_spawn(true, attempt, anchor);
            }
        }
    }

    /// Attempts one deterministic natural spawn around `anchor`.
    fn attempt_natural_spawn(&mut self, hostile: bool, attempt: usize, anchor: WorldVec) -> Option<u64> {
        if !anchor.is_finite() {
            return None;
        }
        let (min_distance, max_distance) = if hostile {
            (HOSTILE_SPAWN_MIN_DISTANCE, HOSTILE_SPAWN_MAX_DISTANCE)
        } else {
            (PASSIVE_SPAWN_MIN_DISTANCE, PASSIVE_SPAWN_MAX_DISTANCE)
        };
        let category = if hostile { 0xA11C_E5A1_u64 } else { 0x0B1E_C7ED_u64 };
        let base = mix64(
            self.world.seed()
                ^ self.tick_count.rotate_left(17)
                ^ category
                ^ (attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        let angle = unit_interval(base) * std::f64::consts::TAU;
        let distance = min_distance + unit_interval(mix64(base)) * (max_distance - min_distance);
        let x = (anchor.x + angle.cos() * distance).floor() as i32;
        let z = (anchor.z + angle.sin() * distance).floor() as i32;
        let candidate = WorldVec::new(x as f64 + 0.5, anchor.y, z as f64 + 0.5);
        let horizontal = horizontal_distance(anchor, candidate);
        if horizontal < MOB_SPAWN_MIN_DISTANCE || horizontal < min_distance || horizontal > max_distance {
            return None;
        }
        self.world.chunk(ChunkPos::new(x >> 4, z >> 4))?;

        let feet_y = if hostile {
            if anchor.y < HOSTILE_CAVE_PLAYER_Y {
                self.cave_floor_near(x, z, anchor.y).or_else(|| {
                    if self.is_night() {
                        self.surface_floor_at(x, z)
                    } else {
                        None
                    }
                })?
            } else {
                self.surface_floor_at(x, z)?
            }
        } else {
            self.grass_floor_at(x, z)?
        };
        let kind = if hostile {
            MobKind::Zombie
        } else {
            random_passive_kind(base)
        };
        let position = WorldVec::new(x as f64 + 0.5, feet_y, z as f64 + 0.5);
        self.spawn_mob(kind, position)
    }

    /// A grass-topped surface column with two air cells above the grass.
    fn grass_floor_at(&self, x: i32, z: i32) -> Option<f64> {
        let surface = self.world.height_at(x, z)?;
        let floor = BlockPos::new(x, surface, z);
        let grass = self.registry.id_or_air("openblock:grass");
        if self.world.get_block(floor) != grass {
            return None;
        }
        self.clearance_above(floor).map(|_| surface as f64 + 1.0)
    }

    /// Any solid surface column with two air cells above it.
    fn surface_floor_at(&self, x: i32, z: i32) -> Option<f64> {
        let surface = self.world.height_at(x, z)?;
        let floor = BlockPos::new(x, surface, z);
        if !self.world.is_solid(floor) {
            return None;
        }
        self.clearance_above(floor).map(|_| surface as f64 + 1.0)
    }

    /// A cave floor within [`CAVE_SEARCH_VERTICAL`] blocks of an underground
    /// player. The scan is downward in Y, so the highest valid ledge wins.
    fn cave_floor_near(&self, x: i32, z: i32, anchor_y: f64) -> Option<f64> {
        let surface = self.world.height_at(x, z)?;
        let player_y = anchor_y.floor() as i32;
        let top = (surface - 1).min(player_y + CAVE_SEARCH_VERTICAL);
        let bottom = (WORLD_MIN_Y + 1).max(player_y - CAVE_SEARCH_VERTICAL);
        if top < bottom {
            return None;
        }
        for y in (bottom..=top).rev() {
            let floor = BlockPos::new(x, y, z);
            if self.world.is_solid(floor) && self.clearance_above(floor).is_some() {
                return Some(y as f64 + 1.0);
            }
        }
        None
    }

    /// Whether the two cells above `floor` are air (and inside the world).
    fn clearance_above(&self, floor: BlockPos) -> Option<()> {
        for dy in 1..=2 {
            let pos = floor.offset(0, dy, 0);
            if !pos.in_world() || self.world.get_block(pos) != ob_content::BlockId::AIR {
                return None;
            }
        }
        Some(())
    }
}

/// Chooses the nearest targetable player position for one mob, falling back to
/// the nearest player (dead/creative) so distance despawn behaviour still has a
/// reference point.
fn select_target(targets: &[(Option<u64>, WorldVec, bool)], from: WorldVec) -> Option<(Option<u64>, WorldVec, bool)> {
    type TargetRef<'a> = &'a (Option<u64>, WorldVec, bool);
    let mut best_alive: Option<(f64, TargetRef<'_>)> = None;
    let mut best_any: Option<(f64, TargetRef<'_>)> = None;
    for target in targets {
        let distance = from.distance(target.1);
        if best_any.is_none_or(|(best, _)| distance < best) {
            best_any = Some((distance, target));
        }
        if target.2 && best_alive.is_none_or(|(best, _)| distance < best) {
            best_alive = Some((distance, target));
        }
    }
    best_alive.or(best_any).map(|(_, target)| *target)
}

/// The chunk containing a world-space position.
fn chunk_of(pos: WorldVec) -> ChunkPos {
    ChunkPos::new((pos.x.floor() as i32) >> 4, (pos.z.floor() as i32) >> 4)
}

/// Horizontal distance between two positions.
fn horizontal_distance(a: WorldVec, b: WorldVec) -> f64 {
    let dx = a.x - b.x;
    let dz = a.z - b.z;
    (dx * dx + dz * dz).sqrt()
}

/// Picks a passive species deterministically from one 64-bit hash.
fn random_passive_kind(hash: u64) -> MobKind {
    match mix64(hash ^ 0x005E_ED0F_C0FF_EE00) % 4 {
        0 => MobKind::Cow,
        1 => MobKind::Pig,
        2 => MobKind::Sheep,
        _ => MobKind::Chicken,
    }
}

/// Maps the top 53 bits of `hash` to `[0, 1)`.
fn unit_interval(hash: u64) -> f64 {
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// Horizontal unit knockback direction from `from` toward `to`.
fn knockback_direction(to: WorldVec, from: WorldVec) -> WorldVec {
    let dx = to.x - from.x;
    let dz = to.z - from.z;
    let length = (dx * dx + dz * dz).sqrt();
    if length <= 1e-9 {
        WorldVec::new(1.0, 0.0, 0.0)
    } else {
        WorldVec::new(dx / length, 0.0, dz / length)
    }
}

/// Distance from a point to an AABB (zero when the point is inside).
fn point_aabb_distance(point: WorldVec, aabb: &ob_math::Aabb) -> f64 {
    let dx = (aabb.min.x - point.x).max(point.x - aabb.max.x).max(0.0);
    let dy = (aabb.min.y - point.y).max(point.y - aabb.max.y).max(0.0);
    let dz = (aabb.min.z - point.z).max(point.z - aabb.max.z).max(0.0);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Whether the sky is open directly above a mob (no non-air block overhead).
fn sky_exposed(world: &World, pos: WorldVec, height: f32) -> bool {
    let x = pos.x.floor() as i32;
    let z = pos.z.floor() as i32;
    let Some(surface) = world.height_at(x, z) else {
        return false;
    };
    surface < (pos.y + f64::from(height)).floor() as i32
}

/// Serializes the mob population to deterministic `mobs.txt` rows.
///
/// One row per live mob, sorted by id:
/// `id kind x y z yaw health age`. Floats use their full-precision `Debug`
/// representation so a reload reproduces the exact state hash.
pub(crate) fn mobs_state(registry: &Registry, mobs: &[MobEntity]) -> String {
    let mut ordered: Vec<&MobEntity> = mobs.iter().collect();
    ordered.sort_by_key(|entity| entity.id);
    let mut text = String::new();
    for entity in ordered {
        if entity.mob.kind.index() >= registry.mob_len() {
            log::warn!("mobs: skipping entity {} with an unknown kind while saving", entity.id);
            continue;
        }
        text.push_str(&format!(
            "{} {} {:?} {:?} {:?} {:?} {:?} {:?}\n",
            entity.id,
            entity.mob.kind.name(),
            entity.mob.pos.x,
            entity.mob.pos.y,
            entity.mob.pos.z,
            entity.mob.yaw,
            entity.mob.health,
            entity.mob.age,
        ));
    }
    text
}

/// Loads `mobs.txt`; a missing file (old worlds) means an empty population.
///
/// Malformed rows, duplicate ids, unknown kinds and non-finite numbers are
/// skipped with a warning. Health clamps to `0..=spec.health`; a non-positive
/// health row is dropped (dead mobs are never saved by the host). Positions
/// clamp to the world's vertical range and a conservative horizontal range;
/// ages clamp to zero from below.
pub(crate) fn load_mobs(world_dir: &Path, registry: &Registry) -> Vec<MobEntity> {
    let path = world_dir.join(MOBS_FILE);
    let text = match crate::read_sidecar(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("could not read {}: {err}; starting with no mobs", path.display());
            }
            return Vec::new();
        }
    };

    let mut mobs: Vec<MobEntity> = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 8 {
            log::warn!("mobs.txt:{line_number}: ignoring malformed line");
            continue;
        }
        let Ok(id) = fields[0].parse::<u64>() else {
            log::warn!("mobs.txt:{line_number}: invalid mob id '{}'", fields[0]);
            continue;
        };
        if id > MAX_HOST_ENTITY_ID {
            log::warn!(
                "mobs.txt:{line_number}: mob id {id} is outside the host id space (0..={MAX_HOST_ENTITY_ID}), dropping it"
            );
            continue;
        }
        if !seen.insert(id) {
            log::warn!("mobs.txt:{line_number}: duplicate mob id {id}, dropping it");
            continue;
        }
        let Some(kind) = registry.mob_id(fields[1]) else {
            log::warn!("mobs.txt:{line_number}: unknown mob kind '{}', dropping it", fields[1]);
            continue;
        };
        let parse = |field: &str| field.parse::<f64>().ok().filter(|value| value.is_finite());
        let (Some(pos_x), Some(pos_y), Some(pos_z)) = (parse(fields[2]), parse(fields[3]), parse(fields[4])) else {
            log::warn!("mobs.txt:{line_number}: invalid position");
            continue;
        };
        let Some(yaw) = fields[5].parse::<f32>().ok().filter(|value| value.is_finite()) else {
            log::warn!("mobs.txt:{line_number}: invalid yaw '{}'", fields[5]);
            continue;
        };
        let Some(health) = parse(fields[6]).map(|value| value as f32) else {
            log::warn!("mobs.txt:{line_number}: invalid health '{}'", fields[6]);
            continue;
        };
        let Some(age) = parse(fields[7]) else {
            log::warn!("mobs.txt:{line_number}: invalid age '{}'", fields[7]);
            continue;
        };
        let spec = *registry.mob(kind);
        if !health.is_finite() || health <= 0.0 {
            log::warn!("mobs.txt:{line_number}: non-positive health, dropping the mob");
            continue;
        }
        let health = health.min(spec.health);
        let age = age.max(0.0);
        let pos = WorldVec::new(
            pos_x.clamp(-30_000_000.0, 30_000_000.0),
            pos_y.clamp(f64::from(WORLD_MIN_Y), f64::from(WORLD_MAX_Y)),
            pos_z.clamp(-30_000_000.0, 30_000_000.0),
        );
        let mut mob = Mob::new(&spec, pos);
        mob.yaw = yaw;
        mob.health = health;
        mob.age = age;
        mob.ai = MobAi::Idle;
        mobs.push(MobEntity::new(id, mob));
    }
    mobs.sort_by_key(|entity| entity.id);
    mobs
}

/// The id the next naturally spawned mob must receive (`max id + 1`).
pub(crate) fn next_mob_id(mobs: &[MobEntity]) -> u64 {
    mobs.iter()
        .map(|entity| entity.id)
        .max()
        .map_or(1, |max| max.saturating_add(1))
}

/// Feeds the deterministic state hash with mobs sorted by id.
///
/// Only persisted fields participate (id, kind name, quantised position, yaw,
/// health and age); transient AI, velocity, hurt and cooldown state would make
/// a save/reload hash differ and is explicitly excluded.
pub(crate) fn hash_mobs(hasher: &mut StableHasher, registry: &Registry, mobs: &[MobEntity]) {
    let mut ordered: Vec<&MobEntity> = mobs.iter().collect();
    ordered.sort_by_key(|entity| entity.id);
    hasher.write_u8(0xF0);
    hasher.write_u64(ordered.len() as u64);
    for entity in ordered {
        hasher.write_u64(entity.id);
        if entity.mob.kind.index() < registry.mob_len() {
            let name = entity.mob.kind.name();
            hasher.write_u16(name.len() as u16);
            hasher.write(name.as_bytes());
        } else {
            hasher.write_u16(0);
        }
        hasher.write_i64(quantize(entity.mob.pos.x));
        hasher.write_i64(quantize(entity.mob.pos.y));
        hasher.write_i64(quantize(entity.mob.pos.z));
        hasher.write_i64(quantize(f64::from(entity.mob.yaw)));
        hasher.write_i64(quantize(f64::from(entity.mob.health)));
        hasher.write_i64(quantize(entity.mob.age));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_interval_is_in_unit_range() {
        for value in [0, 1, 42, u64::MAX, 0x9E37_79B9_7F4A_7C15] {
            let unit = unit_interval(value);
            assert!((0.0..1.0).contains(&unit), "{value} -> {unit}");
        }
    }

    #[test]
    fn passive_kind_choice_is_deterministic_and_covers_the_catalogue() {
        let mut kinds = HashSet::new();
        for seed in 0..64u64 {
            let kind = random_passive_kind(seed);
            assert!(!kind.name().is_empty());
            assert_eq!(kind, random_passive_kind(seed));
            kinds.insert(kind);
        }
        assert_eq!(kinds.len(), 4, "all four passive species are reachable");
        assert!(!kinds.contains(&MobKind::Zombie));
    }

    #[test]
    fn point_aabb_distance_handles_inside_and_outside_points() {
        let aabb = ob_math::Aabb::new(WorldVec::new(0.0, 0.0, 0.0), WorldVec::new(1.0, 2.0, 1.0));
        assert_eq!(point_aabb_distance(WorldVec::new(0.5, 1.0, 0.5), &aabb), 0.0);
        assert_eq!(point_aabb_distance(WorldVec::new(2.0, 1.0, 0.5), &aabb), 1.0);
        assert_eq!(point_aabb_distance(WorldVec::new(0.5, 3.0, 0.5), &aabb), 1.0);
        assert!((point_aabb_distance(WorldVec::new(2.0, 3.0, 2.0), &aabb) - 3.0f64.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn knockback_direction_is_horizontal_and_safe() {
        let direction = knockback_direction(WorldVec::new(3.0, 5.0, 1.0), WorldVec::new(1.0, 0.0, 1.0));
        assert!((direction.x - 1.0).abs() < 1e-9);
        assert_eq!(direction.y, 0.0);
        assert!(direction.z.abs() < 1e-9);
        assert_eq!(
            knockback_direction(WorldVec::new(1.0, 0.0, 1.0), WorldVec::new(1.0, 0.0, 1.0)),
            WorldVec::new(1.0, 0.0, 0.0)
        );
    }

    #[test]
    fn chunk_of_is_floor_division_for_negative_coordinates() {
        assert_eq!(chunk_of(WorldVec::new(0.0, 0.0, 0.0)), ChunkPos::new(0, 0));
        assert_eq!(chunk_of(WorldVec::new(15.9, 0.0, 15.9)), ChunkPos::new(0, 0));
        assert_eq!(chunk_of(WorldVec::new(16.0, 0.0, -0.1)), ChunkPos::new(1, -1));
        assert_eq!(chunk_of(WorldVec::new(-16.0, 0.0, -16.1)), ChunkPos::new(-1, -2));
    }
}

//! Authoritative mob population, natural spawning, combat, drops and saves.
//!
//! The 1.3 slice has eight code-defined species (four passive, four hostile),
//! a deterministic brain in [`ob_sim::step_mob_with_environment`], charged
//! melee combat, skeleton arrows, creeper explosions, breeding/growth and fixed
//! drop/XP tables. There is still no pathfinding, villager or status-effect
//! simulation; movement is straight-line with a spider one-block step-up.
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
//! floor. Hostiles also need gameplay light <= 7, so torches suppress them.
//! Both categories need two air cells above the floor and a loaded column, and
//! no candidate is closer than 8 m to the player. Hostile kinds are weighted
//! zombie 5/16, spider 4/16, skeleton 4/16 and creeper 3/16; the passive cap is
//! 10 (local breeding cap 6 within 24 m).
//!
//! # Daylight
//!
//! The day phase is `tick_count % 24_000`, matching the existing client
//! day/night cycle. Phases `0..12_000` are day and `13_000..23_000` are night;
//! the two 1,000-tick dusk/dawn bands spawn no hostiles. A zombie or skeleton
//! in direct sky light during the day loses [`MOB_BURN_DAMAGE_PER_SECOND`]
//! health per second and dies with its normal drop table (no XP). Spiders are
//! neutral in gameplay light >= 9 and creepers never burn.
//!
//! # Persistence
//!
//! `mobs.txt` is a header-free sidecar with one line per mob:
//! `id kind x y z yaw health age variant love cooldown growth persistent`.
//! Rows are written in ascending id order; 1.0/1.2 eight-column rows still load
//! with neutral breeding defaults. AI intent, velocity, hurt, wind-up, fuse and
//! pending-birth state are transient and restart at idle/full cooldown after a
//! load. The state hash covers every persisted column, so save/reload reproduces
//! it exactly. Arrows are transient and not saved.

use std::collections::HashSet;
use std::path::Path;

use ob_content::{BlockId, ItemId, MobKind, Registry};
use ob_math::{Aabb, BlockPos, ChunkPos, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec, mix64};
use ob_sim::{
    MOB_BABY_GROWTH_TIME, MOB_BREED_COOLDOWN, MOB_BREED_RADIUS, MOB_HURT_FLEE_TIME, MOB_INTERACT_REACH, MOB_LOVE_TIME,
    Mob, MobAi, MobExplosion, MobShot, MobTarget, Projectile, REACH, TICK_DT, aabb_collides, add_xp, attack_charge,
    attack_cooldown_ms, charged_attack_damage, raycast_mob, step_mob_with_environment, step_projectile,
};
use ob_world::World;

use crate::net_players::MAX_HOST_ENTITY_ID;
use crate::{GameMode, Host, ItemStack, StableHasher, quantize};

/// World-directory filename for the mob sidecar.
pub(crate) const MOBS_FILE: &str = "mobs.txt";
/// Natural passive (creature) mob cap.
pub const MAX_PASSIVE_MOBS: usize = 10;
/// Natural hostile (monster) mob cap (raised from 8 in 1.3).
pub const MAX_HOSTILE_MOBS: usize = 12;
/// Passive-animal cap inside [`MOB_BREED_AREA_RADIUS`] for breeding.
pub const MAX_PASSIVE_MOBS_PER_AREA: usize = 6;
/// Radius used by the local breeding cap, in metres.
pub const MOB_BREED_AREA_RADIUS: f64 = 24.0;
/// Ticks between the second feed and the baby appearing.
pub const MOB_BIRTH_TICKS: u32 = 60;
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
/// Highest gameplay light at which a hostile may naturally spawn.
///
/// The 1.2 rule replaces the old depth-only check with
/// `World::gameplay_light(pos, day, weather) <= 7`, so a torch-lit room suppresses
/// monsters even underground while an unlit cave stays spawnable.
pub const HOSTILE_SPAWN_MAX_LIGHT: u8 = 7;
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

/// One live projectile in the host's authoritative population.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectileEntity {
    /// Stable session id (projectile namespace).
    pub id: u64,
    /// Simulation state.
    pub projectile: Projectile,
    /// Network player this arrow was fired at (`None` for the internal player).
    pub target_player: Option<u64>,
}

/// A deterministic drop count for a killed mob. Babies never drop.
fn mob_drops(spec: &ob_content::MobSpec, baby: bool) -> Vec<ItemStack> {
    if baby {
        return Vec::new();
    }
    spec.drops
        .iter()
        .map(|&(item, count)| ItemStack::new(item, count))
        .collect()
}

/// The breeding food for one passive species.
///
/// 1.3 uses wheat for cows, sheep and pigs and seeds for chickens. Carrots for
/// pigs are a documented cut line; no carrot item exists yet.
pub fn breeding_food(kind: MobKind) -> Option<&'static str> {
    match kind {
        MobKind::Cow | MobKind::Pig | MobKind::Sheep => Some("openblock:wheat"),
        MobKind::Chicken => Some("openblock:seeds"),
        _ => None,
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
    /// Server-computed charge (`0..=1`) for this swing.
    pub charge: f32,
    /// Whether the hit was a falling critical (1.5× damage).
    pub critical: bool,
    /// Milliseconds remaining before another swing is allowed.
    pub cooldown_remaining_ms: u32,
    /// Direct XP awarded by this kill.
    pub xp_awarded: u32,
}

/// Result of one mob interaction (feeding/breeding).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InteractOutcome {
    /// Target id supplied by the caller.
    pub id: u64,
    /// Whether the interaction was accepted.
    pub accepted: bool,
    /// Short machine-readable rejection reason (`wrong_food`, `cooldown`, ...).
    pub reason: String,
    /// Baby id when this feed completed a pair.
    pub child: Option<u64>,
    /// XP awarded by a successful pairing.
    pub xp_awarded: u32,
}

impl Host {
    /// The live mob population, in ascending id order.
    pub fn mobs(&self) -> &[MobEntity] {
        &self.mobs
    }

    /// XP points inside the internal player's current level.
    pub fn xp_points(&self) -> u32 {
        self.player.xp_points
    }

    /// The internal player's current XP level.
    pub fn xp_level(&self) -> u32 {
        self.player.xp_level
    }

    /// Milliseconds before the internal player's next charged melee hit.
    ///
    /// Zero means a swing is ready. This uses the same cooldown table as the
    /// server validation, so the local client's crosshair cue cannot disagree
    /// with the damage gate.
    pub fn attack_cooldown_remaining_ms(&self) -> u32 {
        let selected = self.selected_stack().map(|stack| stack.item);
        let cooldown = attack_cooldown_ms(&self.registry, selected);
        let Some(last) = self.last_attack_tick else {
            return 0;
        };
        let elapsed_ms = self.tick_count.saturating_sub(last).saturating_mul(50);
        u32::try_from(cooldown.saturating_sub(elapsed_ms)).unwrap_or(0)
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

    /// Whether the gameplay light at a candidate floor allows a hostile spawn.
    ///
    /// This is `World::gameplay_light(pos, day_factor, weather_factor) <=
    /// HOSTILE_SPAWN_MAX_LIGHT` (the 1.2 rule). A torch at level 14 suppresses
    /// spawns in its ~7-block radius; daylight and a lit room suppress surface
    /// monsters as well.
    pub fn hostile_spawn_light_ok(&self, pos: BlockPos) -> bool {
        self.world.gameplay_light(pos, self.day_factor(), self.weather_factor()) <= HOSTILE_SPAWN_MAX_LIGHT
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
        let mut mob = Mob::new(&spec, pos);
        mob.variant = (mix64(
            self.world.seed() ^ (kind.index() as u64).rotate_left(11) ^ (pos.x.to_bits() >> 3) ^ (pos.z.to_bits() >> 7),
        ) & 3) as u8;
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

    /// Applies one charged player melee attack to a mob.
    ///
    /// The swing is server-authoritative: the cooldown is 625 ms for a sword and
    /// 1,000 ms otherwise, a hit needs charge `>= 0.9`, and damage scales as
    /// `base * (0.2 + 0.8c²)` (×1.5 while falling and not sprinting). A too-early
    /// swing is reported as `hit == false` with the remaining cooldown and does
    /// not wear the tool. Reach and the existing drop/tool-wear rules are
    /// unchanged. Killing a mob awards its direct XP; babies drop nothing.
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
        let selected = self.selected_stack().map(|stack| stack.item);
        let cooldown_ms = attack_cooldown_ms(&self.registry, selected);
        let elapsed = self.last_attack_tick.map(|tick| self.tick_count.saturating_sub(tick));
        let charge = attack_charge(elapsed, cooldown_ms);
        outcome.charge = charge;
        if charge < ob_sim::ATTACK_MIN_CHARGE {
            let elapsed_ms = elapsed.unwrap_or(0).saturating_mul(50);
            outcome.cooldown_remaining_ms = u32::try_from(cooldown_ms.saturating_sub(elapsed_ms)).unwrap_or(0);
            return outcome;
        }
        let base = self.registry.attack_damage(selected);
        if !base.is_finite() || base <= 0.0 {
            return outcome;
        }
        let critical = !self.player.on_ground && self.player.velocity.y < -0.1 && !self.player.sprinting;
        let damage = charged_attack_damage(base, charge, critical);
        outcome.critical = critical;
        outcome.cooldown_remaining_ms = cooldown_ms as u32;

        let health_before = self.mobs[index].mob.health.max(0.0);
        let dealt = damage.min(health_before);
        let position = self.mobs[index].mob.pos;
        let height = f64::from(self.registry.mob(self.mobs[index].mob.kind).height);
        let kind = self.mobs[index].mob.kind;
        let baby = self.mobs[index].mob.is_baby();
        let knockback = knockback_direction(position, self.player.pos);
        {
            let mob = &mut self.mobs[index].mob;
            mob.health = (health_before - dealt).max(0.0);
            mob.hurt_timer = MOB_HURT_FLEE_TIME;
            mob.velocity.x += knockback.x * 5.0;
            mob.velocity.z += knockback.z * 5.0;
            mob.velocity.y = mob.velocity.y.max(4.0);
        }
        self.last_attack_tick = Some(self.tick_count);
        outcome.hit = true;
        outcome.damage = dealt;

        if self.mobs[index].mob.health <= 0.0 {
            outcome.killed = true;
            self.mobs.remove(index);
            let drop_pos = WorldVec::new(position.x, position.y + height * 0.5, position.z);
            let spec = *self.registry.mob(kind);
            if !baby {
                for &(item, count) in spec.drops {
                    let stack = ItemStack::new(item, count);
                    self.spawn_item_entity(drop_pos, stack);
                    outcome.drops.push(stack);
                }
                outcome.xp_awarded = spec.xp_reward;
                if outcome.xp_awarded > 0 {
                    add_xp(
                        &mut self.player.xp_points,
                        &mut self.player.xp_level,
                        outcome.xp_awarded,
                    );
                }
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

    /// Feeds/uses the selected item on one mob and runs the breeding rules.
    ///
    /// Rejections use stable reason codes: `no_target`, `dead`, `too_far`,
    /// `not_breedable`, `baby`, `cooldown`, `love_active`, `wrong_food`,
    /// `not_food`, `cap` and `full`. A successful first feed consumes one item
    /// in survival (creative keeps it) and starts 30 s of love mode. Feeding the
    /// second in-love adult of the same species within 8 m consumes its item,
    /// puts both parents on a 30 s cooldown and schedules a 60-tick birth. The
    /// baby inherits one parent's colour variant and is born persistent.
    pub fn interact_mob(&mut self, id: u64) -> InteractOutcome {
        let mut outcome = InteractOutcome {
            id,
            ..InteractOutcome::default()
        };
        if self.player.dead {
            outcome.reason = "dead".to_string();
            return outcome;
        }
        let Some(index) = self.mobs.iter().position(|entity| entity.id == id) else {
            outcome.reason = "no_target".to_string();
            return outcome;
        };
        let aabb = self.mobs[index].mob.aabb(&self.registry);
        if point_aabb_distance(self.player.eye(), &aabb) > MOB_INTERACT_REACH {
            outcome.reason = "too_far".to_string();
            return outcome;
        }
        let kind = self.mobs[index].mob.kind;
        let spec = *self.registry.mob(kind);
        if spec.hostile || breeding_food(kind).is_none() {
            outcome.reason = "not_breedable".to_string();
            return outcome;
        }
        if self.mobs[index].mob.is_baby() {
            outcome.reason = "baby".to_string();
            return outcome;
        }
        if self.mobs[index].mob.breed_cooldown > 0.0 {
            outcome.reason = "cooldown".to_string();
            return outcome;
        }
        if self.mobs[index].mob.in_love() {
            outcome.reason = "love_active".to_string();
            return outcome;
        }
        let Some(food_name) = breeding_food(kind) else {
            outcome.reason = "not_breedable".to_string();
            return outcome;
        };
        let Some(food) = self.registry.item_id(food_name) else {
            outcome.reason = "not_food".to_string();
            return outcome;
        };
        let Some(selected) = self.selected_stack() else {
            outcome.reason = "not_food".to_string();
            return outcome;
        };
        if selected.item != food {
            outcome.reason = "wrong_food".to_string();
            return outcome;
        }
        if self.passive_mob_count() >= MAX_PASSIVE_MOBS {
            outcome.reason = "cap".to_string();
            return outcome;
        }
        let nearby = self
            .mobs
            .iter()
            .filter(|entity| {
                !entity.mob.is_baby()
                    && self.registry.mob(entity.mob.kind).passive()
                    && entity.mob.pos.distance(self.mobs[index].mob.pos) <= MOB_BREED_AREA_RADIUS
            })
            .count();
        if nearby >= MAX_PASSIVE_MOBS_PER_AREA {
            outcome.reason = "cap".to_string();
            return outcome;
        }
        if self.game_mode == GameMode::Survival && !self.consume_selected(1) {
            outcome.reason = "not_food".to_string();
            return outcome;
        }
        self.mobs[index].mob.love_timer = MOB_LOVE_TIME;
        outcome.accepted = true;

        // Pair with the nearest other in-love adult of the same species.
        let parent_pos = self.mobs[index].mob.pos;
        let parent_variant = self.mobs[index].mob.variant;
        let partner = self
            .mobs
            .iter()
            .enumerate()
            .filter(|(other_index, entity)| {
                *other_index != index
                    && entity.mob.kind == kind
                    && entity.mob.in_love()
                    && entity.mob.breed_cooldown <= 0.0
                    && !entity.mob.is_baby()
                    && entity.mob.pos.distance(parent_pos) <= MOB_BREED_RADIUS
            })
            .min_by(|(_, left), (_, right)| {
                left.mob
                    .pos
                    .distance(parent_pos)
                    .total_cmp(&right.mob.pos.distance(parent_pos))
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|(partner_index, _)| partner_index);
        if let Some(partner_index) = partner {
            let partner_pos = self.mobs[partner_index].mob.pos;
            let partner_id = self.mobs[partner_index].id;
            let partner_variant = self.mobs[partner_index].mob.variant;
            self.mobs[index].mob.love_timer = 0.0;
            self.mobs[partner_index].mob.love_timer = 0.0;
            self.mobs[index].mob.breed_cooldown = MOB_BREED_COOLDOWN;
            self.mobs[partner_index].mob.breed_cooldown = MOB_BREED_COOLDOWN;
            self.mobs[index].mob.persistent = true;
            self.mobs[partner_index].mob.persistent = true;
            let xp = 1 + (mix64(self.world.seed() ^ id.rotate_left(17) ^ partner_id) % 7) as u32;
            let levels = add_xp(&mut self.player.xp_points, &mut self.player.xp_level, xp);
            let _ = levels;
            outcome.xp_awarded = xp;
            // The variant comes from one parent deterministically; the second
            // parent breaks ties in the hash.
            let variant = if mix64(id ^ partner_id) & 1 == 0 {
                parent_variant
            } else {
                partner_variant
            };
            self.pending_births.push(crate::PendingBirth {
                timer: MOB_BIRTH_TICKS as f64 * TICK_DT,
                kind,
                pos: WorldVec::new(
                    (parent_pos.x + partner_pos.x) * 0.5,
                    (parent_pos.y + partner_pos.y) * 0.5,
                    (parent_pos.z + partner_pos.z) * 0.5,
                ),
                variant,
            });
            self.mark_player_dirty();
        }
        outcome
    }

    /// Advances the mob population one tick and runs natural spawning.
    ///
    /// Mobs in unloaded columns are frozen. Melee wind-ups, skeleton shots and
    /// creeper fuses are all simulated in [`ob_sim`]; the host applies the
    /// resulting damage, spawns arrows, runs explosions, ticks projectiles and
    /// advances pending births. Daylight burns only species that declare
    /// `burns_in_daylight` (zombie and skeleton).
    pub(crate) fn tick_mobs(&mut self) {
        let targets = self.mob_targets();
        let burning_day = self.is_day();
        let day_factor = self.day_factor();
        let weather_factor = self.weather_factor();
        let mut pending_attacks: Vec<(Option<u64>, f32, WorldVec)> = Vec::new();
        let mut removed: HashSet<u64> = HashSet::new();
        let mut burn_deaths: Vec<(WorldVec, MobKind, bool)> = Vec::new();
        let mut shots: Vec<(MobShot, Option<u64>)> = Vec::new();
        let mut explosions: Vec<(u64, MobExplosion)> = Vec::new();

        for entity in &mut self.mobs {
            if self.world.chunk(chunk_of(entity.mob.pos)).is_none() {
                continue;
            }
            let Some((target_id, player_pos, player_alive)) = select_target(&targets, entity.mob.pos) else {
                continue;
            };
            let events = step_mob_with_environment(
                &self.world,
                &mut entity.mob,
                player_pos,
                player_alive,
                day_factor,
                weather_factor,
                TICK_DT,
            );
            if events.attacked && player_alive {
                pending_attacks.push((target_id, events.damage, events.knockback));
            }
            if let Some(shot) = events.shot {
                shots.push((shot, target_id));
            }
            if let Some(explosion) = events.exploded {
                explosions.push((entity.id, explosion));
            }
            if events.despawn {
                removed.insert(entity.id);
                continue;
            }
            let spec = *self.registry.mob(entity.mob.kind);
            if burning_day
                && spec.burns_in_daylight
                && sky_exposed(&self.world, entity.mob.pos, spec.height)
                && entity.mob.health > 0.0
            {
                entity.mob.health -= MOB_BURN_DAMAGE_PER_SECOND * TICK_DT as f32;
                if entity.mob.health <= 0.0 {
                    entity.mob.health = 0.0;
                    removed.insert(entity.id);
                    burn_deaths.push((entity.mob.pos, entity.mob.kind, entity.mob.is_baby()));
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

        // Creeper explosions happen before the per-id removal sweep so the
        // creeper itself and any mob it killed are removed exactly once.
        for (source, explosion) in explosions {
            removed.insert(source);
            self.explode(explosion, Some(source));
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
        for (position, kind, baby) in burn_deaths {
            let spec = *self.registry.mob(kind);
            let drop_pos = WorldVec::new(position.x, position.y + f64::from(spec.height) * 0.5, position.z);
            for stack in mob_drops(&spec, baby) {
                self.spawn_item_entity(drop_pos, stack);
            }
        }

        for (shot, target) in shots {
            self.spawn_host_projectile(shot, target);
        }
        self.tick_host_projectiles();
        self.tick_births();

        if self.tick_count.is_multiple_of(MOB_SPAWN_INTERVAL_TICKS) {
            self.try_spawn_mobs();
        }
    }

    /// The live arrow population, in ascending id order (insertion order).
    pub fn projectiles(&self) -> &[ProjectileEntity] {
        &self.projectiles
    }

    /// Number of live arrows.
    pub fn projectile_count(&self) -> usize {
        self.projectiles.len()
    }

    /// Spawns one arrow from a skeleton shot.
    fn spawn_host_projectile(&mut self, shot: MobShot, target_player: Option<u64>) -> u64 {
        let id = self.next_projectile_id.max(1);
        self.next_projectile_id = id.saturating_add(1);
        self.projectiles.push(ProjectileEntity {
            id,
            projectile: Projectile::new(shot.kind, shot.origin, shot.velocity, shot.damage),
            target_player,
        });
        id
    }

    /// Advances every arrow and applies player hits.
    fn tick_host_projectiles(&mut self) {
        let player_pos = self.player.pos;
        let player_dead = self.player.dead;
        let survival = self.game_mode == GameMode::Survival;
        let mut hits: Vec<(usize, Option<u64>, f32)> = Vec::new();
        let mut expired: Vec<usize> = Vec::new();
        for (index, entity) in self.projectiles.iter_mut().enumerate() {
            let before = entity.projectile.pos;
            let events = step_projectile(&self.world, &mut entity.projectile, TICK_DT);
            let after = entity.projectile.pos;
            let hit_player = match entity.target_player {
                None => {
                    !player_dead
                        && survival
                        && segment_hits_aabb(
                            before,
                            after,
                            &Aabb::new(
                                WorldVec::new(player_pos.x - 0.35, player_pos.y - 0.1, player_pos.z - 0.35),
                                WorldVec::new(player_pos.x + 0.35, player_pos.y + 1.9, player_pos.z + 0.35),
                            ),
                        )
                }
                Some(id) => self.network_players.get(&id).is_some_and(|player| {
                    !player.dead
                        && segment_hits_aabb(
                            before,
                            after,
                            &Aabb::new(
                                WorldVec::new(player.pos.x - 0.35, player.pos.y - 0.1, player.pos.z - 0.35),
                                WorldVec::new(player.pos.x + 0.35, player.pos.y + 1.9, player.pos.z + 0.35),
                            ),
                        )
                }),
            };
            if hit_player {
                hits.push((index, entity.target_player, entity.projectile.damage));
            } else if events.despawn {
                expired.push(index);
            }
        }
        for (index, target, damage) in hits {
            if !self
                .projectiles
                .get(index)
                .is_some_and(|entity| entity.projectile.age.is_finite())
            {
                continue;
            }
            if let Some(id) = target {
                let _ = self.damage_network_player(id, damage, "arrow");
            } else {
                self.damage_with_outcome(damage);
            }
            expired.push(index);
        }
        expired.sort_unstable();
        expired.dedup();
        for index in expired.into_iter().rev() {
            if index < self.projectiles.len() {
                self.projectiles.remove(index);
            }
        }
    }

    /// Counts down pending births and spawns every ready baby.
    fn tick_births(&mut self) {
        if self.pending_births.is_empty() {
            return;
        }
        for birth in &mut self.pending_births {
            birth.timer -= TICK_DT;
        }
        let ready: Vec<crate::PendingBirth> = self
            .pending_births
            .iter()
            .copied()
            .filter(|birth| birth.timer <= 0.0)
            .collect();
        if ready.is_empty() {
            return;
        }
        self.pending_births.retain(|birth| birth.timer > 0.0);
        for birth in ready {
            let offsets = [
                WorldVec::ZERO,
                WorldVec::new(0.8, 0.0, 0.0),
                WorldVec::new(-0.8, 0.0, 0.0),
                WorldVec::new(0.0, 0.0, 0.8),
                WorldVec::new(0.0, 0.0, -0.8),
                WorldVec::new(0.0, 1.0, 0.0),
            ];
            let id = offsets
                .into_iter()
                .find_map(|offset| self.spawn_mob(birth.kind, birth.pos + offset));
            let Some(id) = id else {
                // Retry on the next tick if the midpoint and its ring are
                // occupied.
                let mut retry = birth;
                retry.timer = TICK_DT;
                self.pending_births.push(retry);
                continue;
            };
            if let Some(entity) = self.mobs.iter_mut().find(|entity| entity.id == id) {
                entity.mob.growth_timer = MOB_BABY_GROWTH_TIME;
                entity.mob.variant = birth.variant;
                entity.mob.persistent = true;
                entity.mob.age = 0.0;
            }
        }
    }

    /// Applies one creeper explosion: block destruction, entity damage and a
    /// client cue.
    ///
    /// Protects bedrock, fluid blocks, chests/furnaces (their contents must not
    /// be destroyed without a proper container drop) and beds. Block drops are a
    /// documented 1.3 cut line: explosions remove blocks without item drops.
    /// At most [`ob_sim::EXPLOSION_MAX_BLOCKS`] blocks are removed.
    pub(crate) fn explode(&mut self, explosion: MobExplosion, source: Option<u64>) {
        let radius = explosion.radius.clamp(0.5, 8.0) as i32;
        let centre = BlockPos::new(
            explosion.pos.x.floor() as i32,
            explosion.pos.y.floor() as i32,
            explosion.pos.z.floor() as i32,
        );
        let bedrock = self.registry.id_or_air("openblock:bedrock");
        let chest = self.registry.id_or_air("openblock:chest");
        let furnace = self.registry.id_or_air("openblock:furnace");
        let bed_foot = self.registry.id_or_air("openblock:bed_foot");
        let bed_head = self.registry.id_or_air("openblock:bed_head");
        let fire = self.registry.id_or_air("openblock:fire");
        let mut destroyed = 0usize;
        'blocks: for dy in -radius..=radius {
            for dz in -radius..=radius {
                for dx in -radius..=radius {
                    let offset = WorldVec::new(dx as f64, dy as f64, dz as f64);
                    if offset.length() > f64::from(explosion.radius) {
                        continue;
                    }
                    let pos = centre.offset(dx, dy, dz);
                    if !pos.in_world() || self.world.chunk(pos.chunk()).is_none() {
                        continue;
                    }
                    let block = self.world.get_block(pos);
                    if block == BlockId::AIR
                        || block == bedrock
                        || block == chest
                        || block == furnace
                        || block == bed_foot
                        || block == bed_head
                        || self.registry.block(block).fluid
                        || !self.registry.block(block).breakable()
                    {
                        continue;
                    }
                    if self.world.set_block(pos, BlockId::AIR) {
                        self.crops.remove(&pos);
                        self.crops.remove(&pos.offset(0, 1, 0));
                        if block == fire {
                            self.fires.remove(&pos);
                        }
                        self.record_block_update(pos, BlockId::AIR);
                        destroyed += 1;
                        if destroyed >= ob_sim::EXPLOSION_MAX_BLOCKS {
                            break 'blocks;
                        }
                    }
                }
            }
        }

        // Damage mobs other than the exploding source. Killed mobs drop their
        // normal (non-baby) loot but award no player XP.
        let mut killed_mobs: Vec<(WorldVec, MobKind, bool)> = Vec::new();
        for entity in &mut self.mobs {
            if Some(entity.id) == source {
                continue;
            }
            let distance = entity.mob.pos.distance(explosion.pos);
            if distance > f64::from(explosion.radius) {
                continue;
            }
            let falloff = (1.0 - distance / (f64::from(explosion.radius) + 0.5)).clamp(0.0, 1.0) as f32;
            entity.mob.health = (entity.mob.health - explosion.damage * falloff).max(0.0);
            if entity.mob.health <= 0.0 {
                killed_mobs.push((entity.mob.pos, entity.mob.kind, entity.mob.is_baby()));
            }
        }
        if !killed_mobs.is_empty() {
            self.mobs.retain(|entity| entity.mob.health > 0.0);
            for (position, kind, baby) in killed_mobs {
                let spec = *self.registry.mob(kind);
                let drop_pos = WorldVec::new(position.x, position.y + f64::from(spec.height) * 0.5, position.z);
                for stack in mob_drops(&spec, baby) {
                    self.spawn_item_entity(drop_pos, stack);
                }
            }
        }

        // Damage every survival player in range with the same falloff.
        if self.game_mode == GameMode::Survival && !self.player.dead {
            let distance = self.player.pos.distance(explosion.pos);
            if distance <= f64::from(explosion.radius) {
                let falloff = (1.0 - distance / (f64::from(explosion.radius) + 0.5)).clamp(0.0, 1.0) as f32;
                self.damage_with_outcome(explosion.damage * falloff);
            }
        }
        let network_ids: Vec<u64> = self.network_players.keys().copied().collect();
        for id in network_ids {
            let Some(player) = self.network_players.get(&id) else {
                continue;
            };
            let distance = player.pos.distance(explosion.pos);
            if distance <= f64::from(explosion.radius) {
                let falloff = (1.0 - distance / (f64::from(explosion.radius) + 0.5)).clamp(0.0, 1.0) as f32;
                let _ = self.damage_network_player(id, explosion.damage * falloff, "explosion");
            }
        }

        self.explosion_events.push(crate::ExplosionEvent {
            pos: explosion.pos,
            radius: explosion.radius,
        });
    }

    /// Drains explosion cues for the local client or the network layer.
    pub fn take_explosion_events(&mut self) -> Vec<crate::ExplosionEvent> {
        std::mem::take(&mut self.explosion_events)
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
        let feet = BlockPos::new(x, feet_y.floor() as i32, z);
        if hostile && !self.hostile_spawn_light_ok(feet) {
            return None;
        }
        let kind = if hostile {
            random_hostile_kind(base)
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

/// Picks a hostile species deterministically from one 64-bit hash.
///
/// Weights are zombie 5/16, spider 4/16, skeleton 4/16 and creeper 3/16. The
/// fixed list keeps save/reload and test output stable when a kind is appended.
fn random_hostile_kind(hash: u64) -> MobKind {
    match mix64(hash ^ 0x00DA_11CE_5EED_1E13) % 16 {
        0..=4 => MobKind::Zombie,
        5..=8 => MobKind::Spider,
        9..=12 => MobKind::Skeleton,
        _ => MobKind::Creeper,
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

/// Whether the segment `from -> to` intersects an AABB, expanded by 0.2 m.
///
/// The slab test prevents a fast arrow (1 m per tick) from tunnelling past a
/// 0.7 m-wide player between two ticks.
fn segment_hits_aabb(from: WorldVec, to: WorldVec, aabb: &Aabb) -> bool {
    let min = aabb.min - WorldVec::splat(0.2);
    let max = aabb.max + WorldVec::splat(0.2);
    let direction = to - from;
    let mut t_min: f64 = 0.0;
    let mut t_max: f64 = 1.0;
    for axis in 0..3 {
        let origin = from[axis];
        let delta = direction[axis];
        if delta.abs() < 1e-12 {
            if origin < min[axis] || origin > max[axis] {
                return false;
            }
            continue;
        }
        let inv = 1.0 / delta;
        let mut t1 = (min[axis] - origin) * inv;
        let mut t2 = (max[axis] - origin) * inv;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
        }
        t_min = t_min.max(t1);
        t_max = t_max.min(t2);
        if t_min > t_max {
            return false;
        }
    }
    true
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
/// `id kind x y z yaw health age variant love cooldown growth persistent`.
/// Floats use their full-precision `Debug` representation so a reload reproduces
/// the exact state hash. Old eight-column rows (1.0–1.2) still load with neutral
/// breeding defaults and `persistent = false`.
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
            "{} {} {:?} {:?} {:?} {:?} {:?} {:?} {} {:?} {:?} {:?} {}\n",
            entity.id,
            entity.mob.kind.name(),
            entity.mob.pos.x,
            entity.mob.pos.y,
            entity.mob.pos.z,
            entity.mob.yaw,
            entity.mob.health,
            entity.mob.age,
            entity.mob.variant.min(3),
            entity.mob.love_timer,
            entity.mob.breed_cooldown,
            entity.mob.growth_timer,
            entity.mob.persistent as u8,
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
/// timers and ages clamp to zero from below. The appended breeding columns are
/// optional so pre-1.3 rows still load.
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
        if fields.len() != 8 && fields.len() != 13 {
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
        if fields.len() == 13 {
            let variant = fields[8].parse::<u8>().unwrap_or(0).min(3);
            let love = parse(fields[9]).unwrap_or(0.0).max(0.0);
            let cooldown = parse(fields[10]).unwrap_or(0.0).max(0.0);
            let growth = parse(fields[11]).unwrap_or(0.0).clamp(0.0, MOB_BABY_GROWTH_TIME);
            let persistent = fields[12].parse::<u8>().unwrap_or(0) != 0;
            mob.variant = variant;
            mob.love_timer = love.min(MOB_LOVE_TIME);
            mob.breed_cooldown = cooldown.min(MOB_BREED_COOLDOWN);
            mob.growth_timer = growth;
            mob.persistent = persistent;
        }
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
        hasher.write_u8(entity.mob.variant.min(3));
        hasher.write_i64(quantize(entity.mob.love_timer));
        hasher.write_i64(quantize(entity.mob.breed_cooldown));
        hasher.write_i64(quantize(entity.mob.growth_timer));
        hasher.write_u8(u8::from(entity.mob.persistent));
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

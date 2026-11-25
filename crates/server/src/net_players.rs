//! Network-player state, survival simulation and persistence.
//!
//! A dedicated server owns one [`ServerPlayer`] per connected client,
//! independent of the host's internal single-player [`Player`].
//! The network layer in [`crate::net`] validates intents, while this module
//! owns the authoritative state transitions: movement tracking and fall
//! damage, hunger/starvation/regeneration, item pickup, damage/armor/death,
//! respawn, inventory/equipment and saving to `network_players.txt`.
//!
//! # Reusing the single-player rules
//!
//! Rather than duplicate the well-tested block, combat, equip and damage code,
//! network actions temporarily install the network player's pose, inventory,
//! equipment and game mode into the [`Host`]'s single-player slots, run the
//! existing [`Host`] method, then copy the result back and restore the
//! single-player state. The host's internal player is never exposed to clients
//! and is not ticked in network mode.
//!
//! # Authority and limits
//!
//! Movement stays **client-authoritative** in this milestone. The server only
//! validates finite values, world bounds, per-message distance and rate, then
//! tracks reported vertical deltas to apply fall damage on an apparent
//! landing. A client can therefore claim a position (within the per-message
//! limits), spoof fall damage onto itself, or avoid fall damage by never
//! sending a "landing" move. There is no server-side player physics,
//! anti-cheat correction or identity/authentication; see
//! `docs/spec/18-networking-multiplayer.md` §3.13.9.
//!
//! # Persistence
//!
//! `network_players.txt` is a header-free sidecar in the world directory. One
//! line per known player (online first, then offline records), sorted by
//! lower-case name:
//!
//! ```text
//! name x y z yaw pitch health hunger dead selected game_mode
//!      [i<slot>:<item>,<count>[,<durability>]] [e<slot>:<item>,<durability>]
//! ```
//!
//! Records are kept after disconnect and restored on the next join with the
//! same (case-insensitive) name and across server restarts. There is no
//! authentication: a name is a label, not an identity, so anyone who knows a
//! name can resume that save. The state hash does **not** include network
//! players (single-player determinism is unchanged); network state has its own
//! save/restore round-trip tests.

use std::collections::BTreeMap;
use std::path::Path;

use ob_content::{ArmorSlot, BlockId, ItemId, Registry};
use ob_math::{Aabb, BlockPos, WorldVec};
use ob_protocol::{DespawnReason, ItemStackDto};
use ob_sim::{
    EYE_HEIGHT, GRAVITY, MAX_HEALTH, MAX_HUNGER, Player, StepEvents, TICK_DT, advance_vitals, fall_damage, reset_vitals,
};

use crate::{
    ARMOR_SLOTS, ActionOutcome, AttackOutcome, GameMode, HOTBAR_SLOTS, Host, INVENTORY_SLOTS, InteractOutcome,
    ItemStack, MAX_STACK_SIZE,
};

/// World-directory filename for persisted network players.
pub const NETWORK_PLAYERS_FILE: &str = "network_players.txt";
/// Distance at which mobs and dropped items are replicated to a player.
pub const ENTITY_INTEREST_RADIUS: f64 = 64.0;
/// Maximum distance from a player to a furnace/chest they may transact with.
///
/// Opening a window checks this, and the network transaction path re-checks it
/// on every move so a client cannot use a window it opened before teleporting
/// away (movement is client-authoritative in 1.0).
pub const MAX_CONTAINER_DISTANCE: f64 = 6.0;
/// Maximum accumulated fall distance tracked from reported moves, in metres.
const MAX_TRACKED_FALL: f64 = 512.0;
/// Reported vertical delta below which a move counts as descending.
const FALL_STEP_EPSILON: f64 = 0.01;
/// Horizontal world border accepted for a persisted position.
const MAX_WORLD_XZ: f64 = 30_000_000.0;

/// Authoritative state of one network player.
#[derive(Debug, Clone)]
pub struct ServerPlayer {
    /// Connection id for the lifetime of the connection; `0` for an offline
    /// record loaded from `network_players.txt`.
    pub id: u64,
    /// Display name (case-insensitively unique).
    pub name: String,
    /// Feet position in world space.
    pub pos: WorldVec,
    /// Horizontal look angle in radians.
    pub yaw: f32,
    /// Vertical look angle in radians.
    pub pitch: f32,
    /// Health points, `0..=20`.
    pub health: f32,
    /// Hunger points, `0..=20`.
    pub hunger: f32,
    /// Whether health reached zero; actions are ignored until respawn.
    pub dead: bool,
    /// Whether a `Death` was sent and a `Respawn` is expected.
    pub respawn_pending: bool,
    /// 36 player inventory slots: hotbar `0..9`, main `9..36`.
    pub inventory: [Option<ItemStack>; INVENTORY_SLOTS],
    /// Selected hotbar slot, `0..9`.
    pub selected_slot: usize,
    /// Four armor slots (helmet, chestplate, leggings, boots).
    pub equipment: [Option<ItemStack>; ARMOR_SLOTS],
    /// Survival or creative.
    pub game_mode: GameMode,
    /// Seconds accumulated toward starvation damage.
    pub starvation_timer: f64,
    /// Seconds accumulated toward regeneration.
    pub regen_timer: f64,
    /// Accumulated reported descent since the last apparent landing, in metres.
    pub fall_distance: f64,
    /// Horizontal distance reported since the last survival tick, in metres.
    pub pending_move_distance: f64,
    /// Whether the net layer should send a fresh `InventorySync`.
    pub inventory_dirty: bool,
    /// Whether the net layer should send fresh vitals.
    pub vitals_dirty: bool,
    /// Last damage cause, for logs and reconnect hints.
    pub last_damage_cause: Option<String>,
    /// Whether this player is currently asleep in a bed (runtime only).
    pub sleeping: bool,
    /// Occupied bed foot position while asleep (runtime only).
    pub bed: Option<BlockPos>,
    /// Personal bed respawn point, persisted with the player record.
    pub respawn_point: Option<WorldVec>,
    /// XP points inside the current level (1.3).
    pub xp_points: u32,
    /// Current XP level (1.3).
    pub xp_level: u32,
    /// Whether the net layer should send a fresh `XpSync`.
    pub xp_dirty: bool,
    /// Tick of this player's last charged melee hit (runtime only).
    pub last_attack_tick: Option<u64>,
}

impl ServerPlayer {
    /// Creates a fresh player at `pos` with full vitals and an empty
    /// inventory.
    pub fn new(id: u64, name: &str, pos: WorldVec, game_mode: GameMode) -> Self {
        Self {
            id,
            name: name.to_string(),
            pos,
            yaw: 0.0,
            pitch: 0.0,
            health: MAX_HEALTH,
            hunger: MAX_HUNGER,
            dead: false,
            respawn_pending: false,
            inventory: [None; INVENTORY_SLOTS],
            selected_slot: 0,
            equipment: [None; ARMOR_SLOTS],
            game_mode,
            starvation_timer: 0.0,
            regen_timer: 0.0,
            fall_distance: 0.0,
            pending_move_distance: 0.0,
            inventory_dirty: true,
            vitals_dirty: true,
            last_damage_cause: None,
            sleeping: false,
            bed: None,
            respawn_point: None,
            xp_points: 0,
            xp_level: 0,
            xp_dirty: true,
            last_attack_tick: None,
        }
    }

    /// Camera eye position (feet plus the standing eye height).
    pub fn eye(&self) -> WorldVec {
        WorldVec::new(self.pos.x, self.pos.y + EYE_HEIGHT, self.pos.z)
    }

    /// Unit camera-forward vector from the player's yaw and pitch.
    pub fn forward(&self) -> WorldVec {
        let (sin_yaw, cos_yaw) = f64::from(self.yaw).sin_cos();
        let (sin_pitch, cos_pitch) = f64::from(self.pitch).sin_cos();
        WorldVec::new(-sin_yaw * cos_pitch, sin_pitch, -cos_yaw * cos_pitch)
    }

    /// Sum of the equipped armor's defense points.
    ///
    /// Returns `0` in creative mode or while dead, matching the single-player
    /// HUD rule.
    pub fn armor_defense(&self, registry: &Registry) -> u8 {
        if self.game_mode != GameMode::Survival || self.dead {
            return 0;
        }
        self.equipment
            .iter()
            .flatten()
            .filter(|stack| (stack.item.0 as usize) < registry.item_len())
            .filter_map(|stack| registry.item(stack.item).armor.map(|spec| spec.defense))
            .fold(0u8, u8::saturating_add)
    }

    /// A detached snapshot for the admin console and tests.
    pub fn is_online(&self) -> bool {
        self.id != 0
    }
}

/// One entity removal to relay to interested clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovedEntity {
    /// Host-local entity id (mob or item namespace).
    pub id: u64,
    /// Whether this is an item entity id namespace.
    pub item: bool,
    /// Why it was removed.
    pub reason: DespawnReason,
}

/// Namespaced wire id for an item entity.
///
/// Mobs and item entities have independent, overlapping host id counters. The
/// protocol uses one entity id space, so item ids carry the high bit. Clients
/// treat entity ids as opaque.
pub const ITEM_ENTITY_ID_BIT: u64 = 1 << 63;
/// Namespaced wire id for a projectile entity (1.3).
pub const PROJECTILE_ENTITY_ID_BIT: u64 = 1 << 62;

/// Largest host-local mob or item entity id accepted from a sidecar.
///
/// Host ids must stay below [`ITEM_ENTITY_ID_BIT`] so the two namespaces remain
/// disjoint on the wire. The bound leaves one more id available for the
/// `max + 1` next-id allocation, so loading the largest accepted id still
/// produces a valid next id instead of overflowing into the item bit or
/// saturating at `u64::MAX`.
pub const MAX_HOST_ENTITY_ID: u64 = ITEM_ENTITY_ID_BIT - 2;

/// Maps a host entity id to the protocol entity id.
pub fn wire_entity_id(item: bool, id: u64) -> u64 {
    if item { id | ITEM_ENTITY_ID_BIT } else { id }
}

/// One damage event to relay to a client after a tick or action.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkDamageEvent {
    /// Player connection id.
    pub player: u64,
    /// Damage actually removed after armor.
    pub amount: f32,
    /// Short cause (`fall`, `starvation`, `mob`).
    pub cause: String,
    /// Whether this hit killed the player.
    pub died: bool,
}

/// Read-only sleep state for the network layer's `SleepState` broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkSleepSnapshot {
    /// Connected sleepers, ascending.
    pub sleepers: Vec<u64>,
    /// Whether every eligible (connected, alive) player is asleep.
    pub all_sleeping: bool,
    /// Fast-forward ticks left in the 100-tick sunrise skip.
    pub skip_remaining_ticks: u32,
}

/// Result of one accepted network move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetworkMoveResult {
    /// Accepted feet position.
    pub pos: WorldVec,
    /// Fall damage applied by this move (zero when none).
    pub fall_damage: f32,
    /// Whether the player is dead after this move.
    pub died: bool,
}

/// The container a network inventory move addresses, when any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkWindowRef {
    /// Block position of the furnace or chest.
    pub pos: BlockPos,
    /// Slot count of the window (`3` furnace, `27` chest).
    pub slots: usize,
    /// Whether the window is a furnace.
    pub furnace: bool,
}

impl Host {
    /// All currently connected network players, in ascending connection-id
    /// order.
    pub fn network_players(&self) -> &BTreeMap<u64, ServerPlayer> {
        &self.network_players
    }

    /// One connected network player by connection id.
    pub fn network_player(&self, id: u64) -> Option<&ServerPlayer> {
        self.network_players.get(&id)
    }

    /// One connected network player mutably (admin/test and net internals).
    pub fn network_player_mut(&mut self, id: u64) -> Option<&mut ServerPlayer> {
        self.network_players.get_mut(&id)
    }

    /// All offline records loaded from `network_players.txt`, keyed by
    /// lower-case name.
    pub fn saved_network_players(&self) -> &BTreeMap<String, ServerPlayer> {
        &self.saved_network_players
    }

    /// A saved offline record by case-insensitive name.
    pub fn saved_network_player(&self, name: &str) -> Option<&ServerPlayer> {
        self.saved_network_players.get(&name.to_ascii_lowercase())
    }

    /// Registers a network player, restoring a saved record with the same
    /// name when one exists.
    ///
    /// A restored player keeps its position, vitals, inventory, equipment and
    /// game mode. A dead record stays dead (`respawn_pending` is set), so the
    /// client must request `Respawn`. A missing record starts at the host
    /// spawn with the server's default network game mode.
    pub fn join_network_player(&mut self, id: u64, name: &str, default_mode: GameMode) -> Option<&ServerPlayer> {
        let key = name.to_ascii_lowercase();
        let mut player = self
            .saved_network_players
            .remove(&key)
            .unwrap_or_else(|| ServerPlayer::new(id, name, self.spawn, default_mode));
        player.id = id;
        player.name = name.to_string();
        player.respawn_pending = player.dead;
        // Sleep is transient; a reconnecting player wakes at the spawn/bed.
        player.sleeping = false;
        player.bed = None;
        player.inventory_dirty = true;
        player.vitals_dirty = true;
        player.fall_distance = 0.0;
        player.pending_move_distance = 0.0;
        if !player.pos.is_finite()
            || player.pos.y < f64::from(ob_math::WORLD_MIN_Y) - 32.0
            || player.pos.y > f64::from(ob_math::WORLD_MAX_Y) + 256.0
        {
            player.pos = self.spawn;
        }
        player.pos.x = player.pos.x.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ);
        player.pos.z = player.pos.z.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ);
        player.yaw = normalise_angle(player.yaw);
        player.pitch = player
            .pitch
            .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        // Make sure the saved position's column is resident before the client
        // receives chunks and entity snapshots around it.
        self.ensure_streaming_around(player.pos, 2, 16);
        self.network_players.insert(id, player);
        self.mark_player_dirty();
        self.network_players.get(&id)
    }

    /// Removes a connected network player and keeps its state for the next
    /// join with the same name.
    ///
    /// Returns the removed state (also stored under `saved_network_players`).
    pub fn leave_network_player(&mut self, id: u64) -> Option<ServerPlayer> {
        let mut player = self.network_players.remove(&id)?;
        player.id = 0;
        player.inventory_dirty = true;
        player.vitals_dirty = true;
        player.fall_distance = 0.0;
        player.pending_move_distance = 0.0;
        self.saved_network_players
            .insert(player.name.to_ascii_lowercase(), player.clone());
        self.mark_player_dirty();
        Some(player)
    }

    /// Updates an accepted client-authoritative move and applies fall damage
    /// when the reported vertical deltas look like a landing.
    ///
    /// Returns `None` only when `id` is not connected. The caller validates
    /// bounds and per-message distance first.
    pub fn update_network_player_move(
        &mut self,
        id: u64,
        pos: WorldVec,
        yaw: f32,
        pitch: f32,
    ) -> Option<NetworkMoveResult> {
        if !pos.is_finite() || !yaw.is_finite() || !pitch.is_finite() {
            return None;
        }
        let mut fall_impact = 0.0;
        {
            let player = self.network_players.get_mut(&id)?;
            let previous = player.pos;
            let dy = pos.y - previous.y;
            let dx = pos.x - previous.x;
            let dz = pos.z - previous.z;
            player.pending_move_distance += (dx * dx + dz * dz).sqrt();
            player.pos = pos;
            player.yaw = normalise_angle(yaw);
            player.pitch = pitch.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
            if player.game_mode == GameMode::Survival && !player.dead {
                if dy < -FALL_STEP_EPSILON {
                    player.fall_distance = (player.fall_distance - dy).min(MAX_TRACKED_FALL);
                } else if dy > FALL_STEP_EPSILON {
                    player.fall_distance = 0.0;
                } else if player.fall_distance > 0.0 {
                    // The client reported a level move after descending: treat
                    // this as the landing frame.
                    fall_impact = (2.0 * GRAVITY * player.fall_distance).sqrt();
                    player.fall_distance = 0.0;
                }
            } else {
                player.fall_distance = 0.0;
            }
        }

        let mut result = NetworkMoveResult {
            pos,
            fall_damage: 0.0,
            died: false,
        };
        if fall_impact > 0.0 {
            let damage = fall_damage(fall_impact);
            if damage > 0.0 {
                let outcome = self.damage_network_player(id, damage, "fall")?;
                result.fall_damage = outcome.dealt;
            }
        }
        let player = self.network_players.get(&id)?;
        result.pos = player.pos;
        result.died = player.dead;
        Some(result)
    }

    /// Applies survival damage to a network player through the shared armor
    /// formula and records a [`NetworkDamageEvent`] for the net layer.
    ///
    /// Creative players, dead players and non-positive amounts take no damage.
    /// A killing hit sets [`ServerPlayer::respawn_pending`].
    pub fn damage_network_player(&mut self, id: u64, amount: f32, cause: &str) -> Option<crate::DamageOutcome> {
        if !amount.is_finite() || amount <= 0.0 {
            return None;
        }
        let outcome = self.with_network_player_action(id, |host| host.damage_with_outcome(amount))?;
        let (dead, mode) = self
            .network_players
            .get(&id)
            .map(|player| (player.dead, player.game_mode))?;
        if outcome.dealt > 0.0 && mode == GameMode::Survival {
            let mut woke_sleeper = false;
            if let Some(player) = self.network_players.get_mut(&id) {
                player.vitals_dirty = true;
                player.last_damage_cause = Some(cause.to_string());
                if player.sleeping {
                    // Damage wakes a sleeper and cancels the shared sunrise
                    // fast-forward for the whole session, matching the spec's
                    // "damage wakes a sleeper" rule; the remaining sleepers must
                    // not immediately restart the skip next tick.
                    player.sleeping = false;
                    player.bed = None;
                    woke_sleeper = true;
                }
                if dead {
                    player.respawn_pending = true;
                }
            }
            if woke_sleeper {
                self.wake_all_network_sleepers();
            }
            self.sleep_skip = None;
            self.network_damage_events.push(NetworkDamageEvent {
                player: id,
                amount: outcome.dealt,
                cause: cause.to_string(),
                died: dead,
            });
        }
        Some(outcome)
    }

    /// Drains damage events produced since the last call.
    pub fn take_network_damage_events(&mut self) -> Vec<NetworkDamageEvent> {
        std::mem::take(&mut self.network_damage_events)
    }

    /// Drains entity removals (killed/despawned/picked up) that entity
    /// replication should broadcast immediately.
    pub fn take_network_entity_removals(&mut self) -> Vec<RemovedEntity> {
        std::mem::take(&mut self.network_entity_removals)
    }

    /// Respawns a dead network player at the host spawn.
    ///
    /// Vitals are reset, the inventory and equipment are kept (documented
    /// death rule) and the player becomes actionable again. Returns `false`
    /// when the player is unknown or not dead.
    pub fn respawn_network_player(&mut self, id: u64) -> bool {
        let dead = self.network_players.get(&id).is_some_and(|player| player.dead);
        if !dead {
            return false;
        }
        let target = self
            .network_players
            .get(&id)
            .and_then(|player| player.respawn_point)
            .unwrap_or(self.spawn);
        self.ensure_streaming_around(target, 2, 16);
        let mut respawned = false;
        self.with_network_player_action(id, |host| {
            let inventory = host.inventory;
            host.player.pos = target;
            host.player.velocity = WorldVec::ZERO;
            host.player.on_ground = false;
            reset_vitals(&mut host.player);
            debug_assert_eq!(host.inventory, inventory, "respawn keeps the network inventory");
            respawned = true;
        });
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        player.dead = false;
        player.respawn_pending = false;
        player.fall_distance = 0.0;
        player.pending_move_distance = 0.0;
        player.last_damage_cause = None;
        player.sleeping = false;
        player.bed = None;
        player.inventory_dirty = true;
        player.vitals_dirty = true;
        self.mark_player_dirty();
        respawned
    }

    /// Selects a network player's hotbar slot.
    pub fn select_network_slot(&mut self, id: u64, slot: usize) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        if slot >= HOTBAR_SLOTS || player.dead {
            return false;
        }
        player.selected_slot = slot;
        player.inventory_dirty = true;
        self.mark_player_dirty();
        true
    }

    /// Breaks the block at an exact position for a network player, applying
    /// survival loot, block-entity drops and tool wear through the shared
    /// host action rules.
    pub(crate) fn network_break_block(&mut self, id: u64, pos: BlockPos) -> Result<ActionOutcome, String> {
        self.with_network_player_action(id, |host| host.network_break_block_exact(pos))
            .unwrap_or_else(|| Err("not_connected".to_string()))
    }

    /// Places a block at an exact position for a network player, consuming
    /// one matching selected item in survival.
    pub(crate) fn network_place_block(
        &mut self,
        id: u64,
        pos: BlockPos,
        block: BlockId,
    ) -> Result<ActionOutcome, String> {
        self.with_network_player_action(id, |host| host.network_place_block_exact(pos, block))
            .unwrap_or_else(|| Err("not_connected".to_string()))
    }

    /// Uses the selected item on a block for a network player.
    ///
    /// Bed interaction is routed to the per-player sleep state; every other
    /// block runs the same [`Host::interact_block_as_player`] rules as
    /// single-player, with the sender's pose and inventory installed by
    /// [`Host::with_network_player_action`]. The client cannot place, plant,
    /// till or sleep without passing the server-side reach, support, light,
    /// inventory and sleep-window checks.
    pub(crate) fn network_use_block(&mut self, id: u64, pos: BlockPos) -> Result<ActionOutcome, String> {
        let Some(player) = self.network_players.get(&id) else {
            return Err("not_connected".to_string());
        };
        if player.dead {
            return Err("dead".to_string());
        }
        if player.sleeping {
            return Err("sleeping".to_string());
        }
        let block = self.world.get_block(pos);
        let bed_foot = self.registry.id_or_air("openblock:bed_foot");
        let bed_head = self.registry.id_or_air("openblock:bed_head");
        if block == bed_foot || block == bed_head {
            self.network_sleep_request(id, pos)?;
            return Ok(ActionOutcome {
                changed: true,
                ..ActionOutcome::default()
            });
        }
        self.with_network_player_action(id, |host| host.interact_block_as_player(pos))
            .unwrap_or_else(|| Err("not_connected".to_string()))
    }

    /// Puts one network player to sleep in the bed at `pos`.
    ///
    /// Validates reach (3.5 m), the sleep window, the two-block bed shape and
    /// obstruction, then records the personal respawn point. The sunrise skip
    /// starts only when every eligible player is asleep; see
    /// [`Host::tick_network_sleep`].
    pub(crate) fn network_sleep_request(&mut self, id: u64, pos: BlockPos) -> Result<(), String> {
        let Some(player) = self.network_players.get(&id) else {
            return Err("not_connected".to_string());
        };
        if player.dead {
            return Err("dead".to_string());
        }
        if player.sleeping {
            return Err("already_sleeping".to_string());
        }
        let (foot, head) = self.bed_pair(pos)?;
        let distance = player
            .pos
            .distance(foot.center())
            .min(player.pos.distance(head.center()));
        if distance > crate::BED_USE_DISTANCE {
            return Err("out_of_reach".to_string());
        }
        // A valid bed use sets the personal respawn point even if it is not
        // currently possible to sleep; the night window and obstruction checks
        // below still refuse the sleep itself.
        let respawn = WorldVec::new(foot.x as f64 + 0.5, foot.y as f64 + 1.0, foot.z as f64 + 0.5);
        if let Some(player) = self.network_players.get_mut(&id) {
            player.respawn_point = Some(respawn);
            player.vitals_dirty = true;
        }
        self.mark_player_dirty();
        if !self.is_sleep_window() {
            return Err("not_night".to_string());
        }
        if !self.bed_is_clear(foot) || !self.bed_is_clear(head) {
            return Err("obstructed".to_string());
        }
        let Some(player) = self.network_players.get_mut(&id) else {
            return Err("not_connected".to_string());
        };
        player.sleeping = true;
        player.bed = Some(foot);
        player.vitals_dirty = true;
        if self.sleep_wait_start.is_none() {
            self.sleep_wait_start = Some(self.tick_count);
        }
        Ok(())
    }

    /// Wakes one network player and cancels a running shared skip.
    ///
    /// Returns whether the player was asleep.
    pub fn network_wake(&mut self, id: u64) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        if !player.sleeping {
            return false;
        }
        player.sleeping = false;
        player.bed = None;
        if self.sleep_skip.is_some() {
            // Waking one sleeper ends the shared skip and wakes the others.
            self.wake_all_network_sleepers();
        } else if !self.network_players.values().any(|player| player.sleeping) {
            self.sleep_wait_start = None;
        }
        self.mark_player_dirty();
        true
    }

    /// Sleep state for the network layer's `SleepState` broadcast.
    pub fn network_sleep_snapshot(&self) -> NetworkSleepSnapshot {
        let mut sleepers: Vec<u64> = self
            .network_players
            .values()
            .filter(|player| player.sleeping)
            .map(|player| player.id)
            .collect();
        sleepers.sort_unstable();
        let eligible = self.network_players.values().filter(|player| !player.dead).count();
        NetworkSleepSnapshot {
            all_sleeping: eligible > 0 && sleepers.len() == eligible,
            sleepers,
            skip_remaining_ticks: self
                .sleep_skip
                .map_or(0, |skip| crate::SLEEP_FAST_FORWARD_TICKS.saturating_sub(skip.elapsed)),
        }
    }

    /// The receiving player's bed foot position, if they are asleep.
    pub fn network_player_bed(&self, id: u64) -> Option<BlockPos> {
        self.network_players.get(&id).and_then(|player| player.bed)
    }

    /// Whether a network player is asleep.
    pub fn network_player_sleeping(&self, id: u64) -> bool {
        self.network_players.get(&id).is_some_and(|player| player.sleeping)
    }

    /// Cancels every sleeper and the shared sunrise skip.
    fn wake_all_network_sleepers(&mut self) {
        let mut changed = false;
        for player in self.network_players.values_mut() {
            if player.sleeping {
                player.sleeping = false;
                player.bed = None;
                changed = true;
            }
        }
        self.sleep_skip = None;
        self.sleep_wait_start = None;
        if changed {
            self.mark_player_dirty();
        }
    }

    /// Advances the multiplayer sleep vote and the 100-tick sunrise skip.
    ///
    /// A skip runs only while every eligible (connected, alive) player is
    /// asleep. Damage clears the damaged player's sleep and the skip in
    /// [`Host::damage_network_player`]; the next tick wakes the remaining
    /// sleepers. A partial sleep waiting for another player times out after
    /// [`crate::SLEEP_REQUEST_TIMEOUT_TICKS`].
    pub(crate) fn tick_network_sleep(&mut self) {
        let eligible: Vec<u64> = self
            .network_players
            .values()
            .filter(|player| !player.dead)
            .map(|player| player.id)
            .collect();
        let sleepers: Vec<u64> = self
            .network_players
            .values()
            .filter(|player| player.sleeping)
            .map(|player| player.id)
            .collect();
        if eligible.is_empty() {
            self.sleep_skip = None;
            self.sleep_wait_start = None;
            return;
        }
        if sleepers.len() == eligible.len() {
            self.sleep_wait_start = None;
            if self.sleep_skip.is_none() {
                self.start_sleep_skip();
            }
            let Some(mut skip) = self.sleep_skip else {
                return;
            };
            self.tick_count = self.tick_count.saturating_add(skip.step).min(skip.target_tick);
            skip.elapsed = skip.elapsed.saturating_add(1);
            if self.tick_count >= skip.target_tick || skip.elapsed >= crate::SLEEP_FAST_FORWARD_TICKS {
                self.tick_count = skip.target_tick;
                self.wake_all_network_sleepers();
            } else {
                self.sleep_skip = Some(skip);
            }
            return;
        }
        if self.sleep_skip.is_some() {
            // An awake player cancels the skip and wakes everyone.
            self.wake_all_network_sleepers();
            return;
        }
        if sleepers.is_empty() {
            self.sleep_wait_start = None;
            return;
        }
        let started = *self.sleep_wait_start.get_or_insert(self.tick_count);
        if self.tick_count.saturating_sub(started) >= crate::SLEEP_REQUEST_TIMEOUT_TICKS {
            self.wake_all_network_sleepers();
        }
    }

    /// Applies one melee attack for a network player.
    ///
    /// `target` is validated against the live mob population; when `None`,
    /// the server raycasts along the player's look direction. Reach, damage,
    /// tool wear and drops use the existing single-player rules.
    pub(crate) fn network_attack(&mut self, id: u64, target: Option<u64>) -> Result<AttackOutcome, String> {
        let Some(player) = self.network_players.get(&id) else {
            return Err("not_connected".to_string());
        };
        if player.dead {
            return Err("dead".to_string());
        }
        let origin = player.eye();
        let direction = player.forward();
        let target = match target {
            Some(target) => target,
            None => self
                .mob_hit_by_ray(origin, direction, ob_sim::REACH + 0.5)
                .ok_or_else(|| "no_target".to_string())?,
        };
        if self.mob_entity(target).is_none() {
            return Err("no_target".to_string());
        }
        let outcome = self
            .with_network_player_action(id, |host| host.attack_mob(target))
            .ok_or_else(|| "not_connected".to_string())?;
        Ok(outcome)
    }

    /// Applies one mob feeding/breeding interaction for a network player.
    pub(crate) fn network_interact_mob(&mut self, id: u64, target: u64) -> Result<InteractOutcome, String> {
        let Some(player) = self.network_players.get(&id) else {
            return Err("not_connected".to_string());
        };
        if player.dead {
            return Err("dead".to_string());
        }
        if self.mob_entity(target).is_none() {
            return Err("no_target".to_string());
        }
        let outcome = self
            .with_network_player_action(id, |host| host.interact_mob(target))
            .ok_or_else(|| "not_connected".to_string())?;
        Ok(outcome)
    }

    /// Eats one item from a network player's selected hotbar slot.
    pub(crate) fn network_eat(&mut self, id: u64) -> bool {
        self.with_network_player_action(id, |host| host.eat_selected())
            .unwrap_or(false)
    }

    /// Equips the armor in one hotbar slot for a network player.
    pub(crate) fn network_equip(&mut self, id: u64, hotbar_slot: usize) -> bool {
        if hotbar_slot >= HOTBAR_SLOTS {
            return false;
        }
        self.with_network_player_action(id, |host| host.equip_armor(hotbar_slot))
            .unwrap_or(false)
    }

    /// Gives items to a network player's inventory; returns the overflow.
    ///
    /// Admin/test entry point. Newly created items start at full tool/armor
    /// durability; use [`Host::give_network_stack`] to preserve an exact stack.
    pub fn give_network_item(&mut self, id: u64, item: ItemId, count: u8) -> u8 {
        self.with_network_player_action(id, |host| host.give_item(item, count))
            .unwrap_or(count)
    }

    /// Gives an exact stack (count and durability included) to a network
    /// player's inventory and returns the leftover.
    ///
    /// Item-entity pickup uses this path so a damaged tool keeps its remaining
    /// durability instead of being repaired. Returns the whole stack when the
    /// player is not connected and `None` when everything fitted.
    pub fn give_network_stack(&mut self, id: u64, stack: ItemStack) -> Option<ItemStack> {
        match self.with_network_player_action(id, |host| host.give_stack(stack)) {
            Some(leftover) => leftover,
            None => Some(stack),
        }
    }

    /// Sets a network player's vitals directly (admin/test entry point).
    pub fn set_network_player_vitals(&mut self, id: u64, health: f32, hunger: f32) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        player.health = if health.is_finite() {
            health.clamp(0.0, MAX_HEALTH)
        } else {
            player.health
        };
        player.hunger = if hunger.is_finite() {
            hunger.clamp(0.0, MAX_HUNGER)
        } else {
            player.hunger
        };
        player.dead = player.health <= 0.0;
        player.respawn_pending = player.dead;
        player.vitals_dirty = true;
        if player.dead {
            player.fall_distance = 0.0;
        }
        self.mark_player_dirty();
        true
    }

    /// Takes and clears a network player's inventory-dirty flag.
    pub fn take_network_inventory_dirty(&mut self, id: u64) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        let dirty = player.inventory_dirty;
        player.inventory_dirty = false;
        dirty
    }

    /// Takes and clears the XP-dirty flag for one network player.
    pub fn take_network_xp_dirty(&mut self, id: u64) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        let dirty = player.xp_dirty;
        player.xp_dirty = false;
        dirty
    }

    /// Marks a network player's XP as needing a fresh `XpSync`.
    pub fn mark_network_xp_dirty(&mut self, id: u64) {
        if let Some(player) = self.network_players.get_mut(&id) {
            player.xp_dirty = true;
        }
    }

    /// Marks a network player's inventory as needing a fresh `InventorySync`.
    pub fn mark_network_inventory_dirty(&mut self, id: u64) {
        if let Some(player) = self.network_players.get_mut(&id) {
            player.inventory_dirty = true;
        }
    }

    /// Returns and clears a network player's vitals-dirty flag.
    pub fn take_network_vitals_dirty(&mut self, id: u64) -> bool {
        let Some(player) = self.network_players.get_mut(&id) else {
            return false;
        };
        let dirty = player.vitals_dirty;
        player.vitals_dirty = false;
        dirty
    }

    /// Runs one survival tick for every network player, then mob AI, furnace
    /// cooking and per-player item pickup.
    ///
    /// This is the network-mode counterpart of [`Host::tick`]: the host's
    /// internal single-player is not advanced, so single-player behaviour is
    /// unchanged.
    pub fn tick_network(&mut self) {
        self.tick_count += 1;
        tick_network_players(self);
        self.tick_mobs();
        self.tick_furnaces();
        tick_network_items(self);
        self.tick_crops();
        // After mob damage has had a chance to wake a sleeper.
        self.tick_network_sleep();
        self.mark_player_dirty();
    }

    /// Temporarily installs one network player's state into the host's
    /// single-player slots, runs `action`, and restores the single-player
    /// state.
    ///
    /// This is how network intents reuse the tested survival action, combat,
    /// armor and inventory code without duplicating it. The host's internal
    /// player is never ticked in network mode.
    pub(crate) fn with_network_player_action<T>(&mut self, id: u64, action: impl FnOnce(&mut Host) -> T) -> Option<T> {
        let mut actor = self.network_players.remove(&id)?;
        let inventory_before = actor.inventory;
        let equipment_before = actor.equipment;
        let selected_before = actor.selected_slot;
        let xp_before = (actor.xp_points, actor.xp_level);

        let mut player = Player::new(actor.pos);
        player.yaw = actor.yaw;
        player.pitch = actor.pitch;
        player.health = actor.health;
        player.hunger = actor.hunger;
        player.dead = actor.dead;
        player.starvation_timer = actor.starvation_timer;
        player.regen_timer = actor.regen_timer;
        player.flying = actor.game_mode == GameMode::Creative;
        player.on_ground = true;
        player.xp_points = actor.xp_points;
        player.xp_level = actor.xp_level;

        let previous_player = std::mem::replace(&mut self.player, player);
        let previous_inventory = std::mem::replace(&mut self.inventory, actor.inventory);
        let previous_selected = std::mem::replace(&mut self.hotbar_selection, actor.selected_slot);
        let previous_mode = std::mem::replace(&mut self.game_mode, actor.game_mode);
        let previous_equipment = std::mem::replace(&mut self.equipment, actor.equipment);
        let previous_attack_tick = std::mem::replace(&mut self.last_attack_tick, actor.last_attack_tick);

        let result = action(self);

        actor.pos = self.player.pos;
        actor.yaw = self.player.yaw;
        actor.pitch = self.player.pitch;
        actor.health = self.player.health.clamp(0.0, MAX_HEALTH);
        actor.hunger = self.player.hunger.clamp(0.0, MAX_HUNGER);
        actor.dead = self.player.dead;
        actor.starvation_timer = self.player.starvation_timer;
        actor.regen_timer = self.player.regen_timer;
        actor.xp_points = self.player.xp_points;
        actor.xp_level = self.player.xp_level;
        actor.last_attack_tick = self.last_attack_tick;
        actor.inventory = std::mem::replace(&mut self.inventory, previous_inventory);
        actor.selected_slot = std::mem::replace(&mut self.hotbar_selection, previous_selected);
        actor.game_mode = std::mem::replace(&mut self.game_mode, previous_mode);
        actor.equipment = std::mem::replace(&mut self.equipment, previous_equipment);
        self.player = previous_player;
        self.last_attack_tick = previous_attack_tick;

        if actor.inventory != inventory_before
            || actor.equipment != equipment_before
            || actor.selected_slot != selected_before
        {
            actor.inventory_dirty = true;
        }
        if (actor.xp_points, actor.xp_level) != xp_before {
            actor.xp_dirty = true;
        }
        self.network_players.insert(id, actor);
        Some(result)
    }

    /// Exact-position survival break used by network intents.
    fn network_break_block_exact(&mut self, pos: BlockPos) -> Result<ActionOutcome, String> {
        if !pos.in_world() || self.world.chunk(pos.chunk()).is_none() {
            return Err("invalid_position".to_string());
        }
        let block = self.world.get_block(pos);
        if block == BlockId::AIR {
            return Err("no_block".to_string());
        }
        if !self.registry.block(block).breakable() {
            return Err("unbreakable".to_string());
        }
        Ok(self.break_block_at(pos))
    }

    /// Exact-position survival placement used by network intents.
    fn network_place_block_exact(&mut self, pos: BlockPos, block: BlockId) -> Result<ActionOutcome, String> {
        if !pos.in_world() || self.world.chunk(pos.chunk()).is_none() {
            return Err("invalid_position".to_string());
        }
        if block == BlockId::AIR || block.0 as usize >= self.registry.len() {
            return Err("invalid_block".to_string());
        }
        if self.registry.block(block).fluid {
            return Err("invalid_block".to_string());
        }
        if self.world.get_block(pos) != BlockId::AIR {
            return Err("occupied".to_string());
        }
        if Aabb::block(pos).intersects(&self.player.aabb()) {
            return Err("player_overlap".to_string());
        }
        let torch = self.registry.id_or_air("openblock:torch");
        let bed_foot = self.registry.id_or_air("openblock:bed_foot");
        let bed_head = self.registry.id_or_air("openblock:bed_head");
        if block == torch {
            // 1.2 torches are floor-only.
            let floor = pos.offset(0, -1, 0);
            if !floor.in_world() || !self.world.is_solid(floor) {
                return Err("no_support".to_string());
            }
        }
        if block == bed_foot || block == bed_head {
            let (foot, head) = if block == bed_foot {
                (pos, pos.offset(0, 0, 1))
            } else {
                (pos.offset(0, 0, -1), pos)
            };
            if !foot.in_world()
                || !head.in_world()
                || self.world.chunk(foot.chunk()).is_none()
                || self.world.chunk(head.chunk()).is_none()
            {
                return Err("invalid_position".to_string());
            }
            if self.world.get_block(foot) != BlockId::AIR || self.world.get_block(head) != BlockId::AIR {
                return Err("occupied".to_string());
            }
            if !self.world.is_solid(foot.offset(0, -1, 0)) || !self.world.is_solid(head.offset(0, -1, 0)) {
                return Err("no_support".to_string());
            }
            if Aabb::block(head).intersects(&self.player.aabb()) {
                return Err("player_overlap".to_string());
            }
        }
        let supported = [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
            .into_iter()
            .any(|(dx, dy, dz)| self.world.is_solid(pos.offset(dx, dy, dz)));
        if !supported {
            return Err("no_support".to_string());
        }
        let held = self.selected_stack().map(|stack| stack.item);
        let matches = held.is_some_and(|item| self.registry.item(item).block == Some(block));
        if !matches {
            return Err("not_held".to_string());
        }
        Ok(self.place_selected_at(pos, block))
    }
}

/// Advances vitals, starvation, regeneration and death for every network
/// player.
fn tick_network_players(host: &mut Host) {
    let ids: Vec<u64> = host.network_players.keys().copied().collect();
    for id in ids {
        let (moved, health_before, hunger_before, mode, dead_before, sleeping) = match host.network_players.get_mut(&id)
        {
            Some(player) => {
                let moved = std::mem::take(&mut player.pending_move_distance);
                (
                    moved,
                    player.health,
                    player.hunger,
                    player.game_mode,
                    player.dead,
                    player.sleeping,
                )
            }
            None => continue,
        };
        if sleeping {
            // Sleep freezes hunger/regen and clears fall tracking; damage is
            // still applied by the mob tick and wakes the player.
            if let Some(player) = host.network_players.get_mut(&id) {
                player.pending_move_distance = 0.0;
                player.fall_distance = 0.0;
            }
            continue;
        }

        let mut starvation = 0.0f32;
        let mut starvation_died = false;
        host.with_network_player_action(id, |host| {
            if mode == GameMode::Creative {
                reset_vitals(&mut host.player);
                return;
            }
            if host.player.dead {
                host.player.velocity = WorldVec::ZERO;
                return;
            }
            let events = StepEvents {
                moved_distance: moved,
                ..StepEvents::default()
            };
            let before = host.player.health;
            advance_vitals(&mut host.player, &events, TICK_DT);
            let starved = before - host.player.health;
            if starved > 0.0 {
                // Route starvation through the armor formula, exactly like the
                // single-player host does.
                host.player.health = before;
                host.player.dead = false;
                let outcome = host.damage_with_outcome(starved);
                starvation = outcome.dealt;
                starvation_died = host.player.dead;
            }
            if host.player.health <= 0.0 {
                host.player.health = 0.0;
                host.player.dead = true;
                host.player.velocity = WorldVec::ZERO;
            }
        });

        let Some(player) = host.network_players.get_mut(&id) else {
            continue;
        };
        if starvation > 0.0 {
            player.vitals_dirty = true;
            host.network_damage_events.push(NetworkDamageEvent {
                player: id,
                amount: starvation,
                cause: "starvation".to_string(),
                died: starvation_died || player.dead,
            });
        }
        if player.dead {
            player.respawn_pending = true;
            player.fall_distance = 0.0;
        }
        if player.health != health_before || player.hunger != hunger_before || player.dead != dead_before {
            player.vitals_dirty = true;
        }
        if starvation_died {
            player.last_damage_cause = Some("starvation".to_string());
        }
    }
}

/// Advances dropped items and runs per-network-player pickup.
fn tick_network_items(host: &mut Host) {
    for entity in &mut host.items {
        crate::items::step_item(&host.world, entity, TICK_DT);
    }

    let ids: Vec<u64> = host.network_players.keys().copied().collect();

    // Attract ready items toward the nearest eligible living survival player.
    for entity in &mut host.items {
        if entity.age < crate::items::ITEM_PICKUP_DELAY {
            continue;
        }
        let mut best: Option<(f64, WorldVec)> = None;
        for id in &ids {
            let Some(player) = host.network_players.get(id) else {
                continue;
            };
            if player.game_mode != GameMode::Survival || player.dead {
                continue;
            }
            let chest = chest_position(player.pos);
            let distance = chest.distance(entity.pos);
            if distance <= crate::items::ITEM_PICKUP_RADIUS && best.is_none_or(|(closest, _)| distance < closest) {
                best = Some((distance, chest));
            }
        }
        if let Some((_, target)) = best {
            attract(entity, target);
        }
    }

    let mut index = 0;
    let mut removed = Vec::new();
    while index < host.items.len() {
        if host.items[index].age < crate::items::ITEM_PICKUP_DELAY {
            index += 1;
            continue;
        }
        let entity_pos = host.items[index].pos;
        let mut candidates: Vec<(f64, u64)> = Vec::new();
        for id in &ids {
            let Some(player) = host.network_players.get(id) else {
                continue;
            };
            if player.game_mode != GameMode::Survival || player.dead {
                continue;
            }
            let distance = chest_position(player.pos).distance(entity_pos);
            if distance <= crate::items::ITEM_COLLECT_DISTANCE {
                candidates.push((distance, *id));
            }
        }
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut handled = false;
        for (_, id) in candidates {
            let stack = host.items[index].stack;
            let leftover = host.give_network_stack(id, stack);
            let added = stack.count - leftover.map_or(0, |left| left.count);
            if added == 0 {
                // This player's inventory is full; try the next-nearest
                // eligible player before leaving the entity in the world.
                continue;
            }
            host.mark_network_inventory_dirty(id);
            if let Some(left) = leftover {
                host.items[index].stack.count = left.count;
                index += 1;
            } else {
                let id = host.items[index].id;
                removed.push(RemovedEntity {
                    id,
                    item: true,
                    reason: DespawnReason::PickedUp,
                });
                host.items.remove(index);
            }
            handled = true;
            break;
        }
        if !handled {
            index += 1;
        }
    }

    let mut keep = Vec::with_capacity(host.items.len());
    for entity in std::mem::take(&mut host.items) {
        if entity.age >= crate::items::ITEM_DESPAWN_AGE || entity.pos.y < crate::items::ITEM_DESPAWN_Y {
            removed.push(RemovedEntity {
                id: entity.id,
                item: true,
                reason: DespawnReason::Despawned,
            });
        } else {
            keep.push(entity);
        }
    }
    host.items = keep;
    host.network_entity_removals.extend(removed);
}

/// Chest-height position used for item pickup.
fn chest_position(pos: WorldVec) -> WorldVec {
    WorldVec::new(pos.x, pos.y + 0.9, pos.z)
}

/// Moves one item toward a target chest at the shared magnet speed.
fn attract(entity: &mut crate::ItemEntity, target: WorldVec) {
    let delta = target - entity.pos;
    let distance = delta.length();
    if distance > f64::EPSILON {
        let step = (crate::items::ITEM_MAGNET_SPEED * TICK_DT).min(distance);
        entity.pos += delta * (step / distance);
        entity.velocity = WorldVec::ZERO;
    }
}

/// Normalises an angle to `-PI..=PI`; non-finite input becomes zero.
///
/// Save/load and the state hash call this, so a debug-built non-finite look
/// angle cannot produce a `player.txt` that the loader later rejects.
pub(crate) fn normalise_angle(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    let mut angle = angle % std::f32::consts::TAU;
    if angle > std::f32::consts::PI {
        angle -= std::f32::consts::TAU;
    } else if angle < -std::f32::consts::PI {
        angle += std::f32::consts::TAU;
    }
    angle
}

/// Clamps a look pitch to the camera's `-PI/2..=PI/2` range; non-finite input
/// becomes zero.
pub(crate) fn clamp_pitch(pitch: f32) -> f32 {
    if pitch.is_finite() {
        pitch.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2)
    } else {
        0.0
    }
}

/// Serializes online and offline network players to deterministic
/// `network_players.txt` rows sorted by lower-case name.
pub(crate) fn network_players_state(
    registry: &Registry,
    online: &BTreeMap<u64, ServerPlayer>,
    saved: &BTreeMap<String, ServerPlayer>,
) -> String {
    let mut all: BTreeMap<String, &ServerPlayer> = BTreeMap::new();
    for player in saved.values() {
        all.insert(player.name.to_ascii_lowercase(), player);
    }
    for player in online.values() {
        all.insert(player.name.to_ascii_lowercase(), player);
    }
    let mut text = String::new();
    for (key, player) in all {
        if ob_protocol::validate_player_name(&player.name).is_err() || key.is_empty() {
            log::warn!("network_players: skipping invalid name {:?}", player.name);
            continue;
        }
        if !player.pos.is_finite() {
            log::warn!("network_players: skipping '{}' with a non-finite position", player.name);
            continue;
        }
        text.push_str(&format!(
            "{} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {} {} {}",
            player.name,
            player.pos.x,
            player.pos.y,
            player.pos.z,
            player.yaw,
            player.pitch,
            player.health,
            player.hunger,
            player.dead as u8,
            player.selected_slot.min(HOTBAR_SLOTS - 1),
            player.game_mode.name(),
        ));
        if let Some(point) = player.respawn_point
            && point.is_finite()
        {
            text.push_str(&format!(" r{:?},{:?},{:?}", point.x, point.y, point.z));
        }
        for (slot, stack) in player.inventory.iter().enumerate() {
            let Some(stack) = stack.filter(|stack| stack.count > 0) else {
                continue;
            };
            if stack.item.0 as usize >= registry.item_len() {
                continue;
            }
            let name = registry.item(stack.item).id;
            match stack.durability {
                Some(durability) => text.push_str(&format!(" i{slot}:{name},{},{durability}", stack.count)),
                None => text.push_str(&format!(" i{slot}:{name},{}", stack.count)),
            }
        }
        for (slot, stack) in player.equipment.iter().enumerate() {
            let Some(stack) = stack.filter(|stack| stack.count > 0) else {
                continue;
            };
            if stack.item.0 as usize >= registry.item_len() {
                continue;
            }
            let name = registry.item(stack.item).id;
            let durability = stack.durability.unwrap_or(0);
            text.push_str(&format!(" e{slot}:{name},{durability}"));
        }
        text.push_str(&format!(" xp{}:{}", player.xp_points, player.xp_level));
        text.push('\n');
    }
    text
}

/// Loads `network_players.txt`; a missing file means no saved players.
///
/// Malformed lines, invalid names, unknown items and out-of-range values are
/// skipped or clamped with a warning. The returned map is keyed by lower-case
/// name; ids are `0` until the player joins.
pub(crate) fn load_network_players(world_dir: &Path, registry: &Registry) -> BTreeMap<String, ServerPlayer> {
    let path = world_dir.join(NETWORK_PLAYERS_FILE);
    let text = match crate::read_sidecar(&path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not read {}: {error}; starting with no network players",
                    path.display()
                );
            }
            return BTreeMap::new();
        }
    };
    let mut players = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            log::warn!("network_players.txt:{line_number}: ignoring malformed line");
            continue;
        }
        let name = fields[0];
        if ob_protocol::validate_player_name(name).is_err() {
            log::warn!("network_players.txt:{line_number}: invalid player name '{name}'");
            continue;
        }
        let parse_f64 = |field: &str| field.parse::<f64>().ok().filter(|value| value.is_finite());
        let parse_f32 = |field: &str| field.parse::<f32>().ok().filter(|value| value.is_finite());
        let (Some(x), Some(y), Some(z)) = (parse_f64(fields[1]), parse_f64(fields[2]), parse_f64(fields[3])) else {
            log::warn!("network_players.txt:{line_number}: invalid position");
            continue;
        };
        let (Some(yaw), Some(pitch)) = (parse_f32(fields[4]), parse_f32(fields[5])) else {
            log::warn!("network_players.txt:{line_number}: invalid look angles");
            continue;
        };
        let (Some(health), Some(hunger)) = (parse_f32(fields[6]), parse_f32(fields[7])) else {
            log::warn!("network_players.txt:{line_number}: invalid vitals");
            continue;
        };
        let Ok(dead) = fields[8].parse::<u8>() else {
            log::warn!("network_players.txt:{line_number}: invalid dead flag");
            continue;
        };
        let Ok(selected) = fields[9].parse::<usize>() else {
            log::warn!("network_players.txt:{line_number}: invalid selected slot");
            continue;
        };
        let game_mode = match fields[10] {
            "creative" => GameMode::Creative,
            "survival" => GameMode::Survival,
            other => {
                log::warn!("network_players.txt:{line_number}: unknown game mode '{other}'");
                continue;
            }
        };
        let mut player = ServerPlayer::new(
            0,
            name,
            WorldVec::new(
                x.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
                y.clamp(-64.0, 320.0),
                z.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
            ),
            game_mode,
        );
        player.yaw = normalise_angle(yaw);
        player.pitch = pitch.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        player.health = health.clamp(0.0, MAX_HEALTH);
        player.hunger = hunger.clamp(0.0, MAX_HUNGER);
        player.dead = dead != 0 || player.health <= 0.0;
        player.respawn_pending = player.dead;
        player.selected_slot = selected.min(HOTBAR_SLOTS - 1);

        for token in &fields[11..] {
            if let Some(rest) = token.strip_prefix('r') {
                let parts: Vec<&str> = rest.split(',').collect();
                if parts.len() != 3 {
                    log::warn!("network_players.txt:{line_number}: malformed respawn token");
                    continue;
                }
                let parse = |text: &str| text.parse::<f64>().ok().filter(|value| value.is_finite());
                let (Some(x), Some(y), Some(z)) = (parse(parts[0]), parse(parts[1]), parse(parts[2])) else {
                    log::warn!("network_players.txt:{line_number}: invalid respawn point");
                    continue;
                };
                player.respawn_point = Some(WorldVec::new(
                    x.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
                    y.clamp(-64.0, 320.0),
                    z.clamp(-MAX_WORLD_XZ, MAX_WORLD_XZ),
                ));
            } else if let Some(rest) = token.strip_prefix('i') {
                let Some((slot_text, stack_text)) = rest.split_once(':') else {
                    log::warn!("network_players.txt:{line_number}: malformed inventory token");
                    continue;
                };
                let Ok(slot) = slot_text.parse::<usize>() else {
                    log::warn!("network_players.txt:{line_number}: invalid inventory slot");
                    continue;
                };
                if slot >= INVENTORY_SLOTS {
                    log::warn!("network_players.txt:{line_number}: inventory slot {slot} out of range");
                    continue;
                }
                if let Some(stack) = parse_stack_token(stack_text, registry, line_number, false) {
                    player.inventory[slot] = Some(stack);
                }
            } else if let Some(rest) = token.strip_prefix('e') {
                let Some((slot_text, stack_text)) = rest.split_once(':') else {
                    log::warn!("network_players.txt:{line_number}: malformed equipment token");
                    continue;
                };
                let Ok(slot) = slot_text.parse::<usize>() else {
                    log::warn!("network_players.txt:{line_number}: invalid equipment slot");
                    continue;
                };
                if slot >= ARMOR_SLOTS {
                    log::warn!("network_players.txt:{line_number}: equipment slot {slot} out of range");
                    continue;
                }
                if let Some(stack) = parse_stack_token(stack_text, registry, line_number, true) {
                    match registry.item(stack.item).armor {
                        Some(armor) if equipment_armor_slot(slot) == Some(armor.slot) => {
                            let mut stack = stack;
                            stack.count = 1;
                            player.equipment[slot] = Some(stack);
                        }
                        Some(_) => log::warn!("network_players.txt:{line_number}: armor in the wrong equipment slot"),
                        None => log::warn!("network_players.txt:{line_number}: non-armor item in equipment"),
                    }
                }
            } else {
                log::warn!("network_players.txt:{line_number}: unknown token '{token}'");
            }
        }
        players.insert(player.name.to_ascii_lowercase(), player);
    }
    players
}

/// Parses `item,count[,durability]` or `item,durability` (equipment).
fn parse_stack_token(token: &str, registry: &Registry, line_number: usize, equipment: bool) -> Option<ItemStack> {
    let mut parts = token.split(',');
    let name = parts.next()?;
    let Some(item) = registry.item_id(name) else {
        log::warn!("network_players.txt:{line_number}: unknown item '{name}'");
        return None;
    };
    let item_type = registry.item(item);
    let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
    let (count, durability_text) = if equipment {
        (1u8, parts.next())
    } else {
        let Some(count) = parts.next().and_then(|text| text.parse::<u32>().ok()) else {
            log::warn!("network_players.txt:{line_number}: invalid item count");
            return None;
        };
        (count.clamp(1, u32::from(max_stack)) as u8, parts.next())
    };
    if parts.next().is_some() {
        log::warn!("network_players.txt:{line_number}: malformed stack token");
        return None;
    }
    let mut stack = ItemStack::for_item(item, item_type, count);
    if let Some(text) = durability_text {
        match text.parse::<u32>() {
            Ok(value) => {
                let max = item_type
                    .tool
                    .map(|spec| spec.durability)
                    .or_else(|| item_type.armor.map(|spec| spec.durability));
                match max {
                    Some(max) => stack.durability = Some(value.clamp(1, u32::from(max)) as u16),
                    // Matches `sanitize_stack`: non-tool/non-armor items carry
                    // no durability component even if a hostile sidecar says so.
                    None => {
                        log::warn!("network_players.txt:{line_number}: item '{name}' has no durability, ignoring it")
                    }
                }
            }
            Err(_) => {
                log::warn!("network_players.txt:{line_number}: invalid durability '{text}'");
                return None;
            }
        }
    }
    Some(stack)
}

/// Converts one host stack to the wire representation.
pub fn stack_dto(registry: &Registry, stack: &Option<ItemStack>) -> Option<ItemStackDto> {
    let stack = stack.filter(|stack| stack.count > 0 && (stack.item.0 as usize) < registry.item_len())?;
    Some(ItemStackDto {
        item: registry.item(stack.item).id.to_string(),
        count: stack.count,
        durability: stack.durability,
    })
}

/// Resolves a wire item name to a content id.
pub fn item_from_dto(registry: &Registry, dto: &ItemStackDto) -> Option<ItemId> {
    if dto.count == 0 {
        return None;
    }
    registry.item_id(&dto.item)
}

/// Resolves an equipment-array index (`0..4`) to its armor slot.
pub fn equipment_armor_slot(index: usize) -> Option<ArmorSlot> {
    match index {
        0 => Some(ArmorSlot::Helmet),
        1 => Some(ArmorSlot::Chestplate),
        2 => Some(ArmorSlot::Leggings),
        3 => Some(ArmorSlot::Boots),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CHEST_SLOTS, ContainerState, FurnaceState, HostConfig};
    use std::sync::Arc;

    fn temp_host(label: &str) -> (std::path::PathBuf, Host) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("ob-net-players-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let registry = Arc::new(Registry::base());
        let host = Host::open(
            registry,
            HostConfig {
                seed: 7,
                world_name: "net-players".to_string(),
                data_dir: Some(dir.clone()),
                render_distance_chunks: 1,
                generate_budget: 8,
            },
        )
        .expect("open host");
        (dir, host)
    }

    #[test]
    fn server_player_helpers_are_finite_and_defensive() {
        let mut player = ServerPlayer::new(1, "alice", WorldVec::new(0.5, 70.0, 0.5), GameMode::Survival);
        assert_eq!(player.eye(), WorldVec::new(0.5, 70.0 + EYE_HEIGHT, 0.5));
        let forward = player.forward();
        assert!(forward.is_finite());
        assert!((forward.length() - 1.0).abs() < 1e-9);
        player.yaw = 40.0;
        player.pitch = 4.0;
        assert!(normalise_angle(player.yaw).abs() <= std::f32::consts::PI);
        assert_eq!(normalise_angle(f32::NAN), 0.0);
        assert_eq!(normalise_angle(f32::INFINITY), 0.0);
        assert_eq!(clamp_pitch(f32::NEG_INFINITY), 0.0);
        assert_eq!(clamp_pitch(2.0), std::f32::consts::FRAC_PI_2);
        player.dead = true;
        assert_eq!(player.armor_defense(&Registry::base()), 0);
    }

    #[test]
    fn give_network_stack_preserves_durability_and_rejects_unknown_players() {
        let (dir, mut host) = temp_host("give-stack");
        host.join_network_player(1, "alice", GameMode::Survival).expect("join");
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").expect("pickaxe");
        let damaged = host.make_stack(pickaxe, 1).expect("tool stack").with_durability(7);

        assert_eq!(host.give_network_stack(1, damaged), None);
        let player = host.network_player(1).expect("online");
        assert_eq!(player.inventory[0], Some(damaged), "durability must be preserved");
        assert_eq!(host.give_network_stack(99, damaged), Some(damaged));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn network_pickup_preserves_damaged_tool_durability() {
        let (dir, mut host) = temp_host("pickup-durability");
        let spawn = host.spawn();
        host.join_network_player(1, "alice", GameMode::Survival).expect("join");
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").expect("pickaxe");
        let damaged = host.make_stack(pickaxe, 1).expect("tool stack").with_durability(7);
        host.spawn_item_entity(WorldVec::new(spawn.x, spawn.y + 0.1, spawn.z), damaged);

        let mut picked = false;
        for _ in 0..80 {
            host.tick_network();
            if host.item_entities().is_empty() {
                picked = true;
                break;
            }
        }
        assert!(picked, "network pickup did not collect the damaged tool");
        let player = host.network_player(1).expect("online");
        assert_eq!(
            player.inventory[0],
            Some(damaged),
            "network pickup must not repair the tool"
        );
        assert!(
            host.take_network_entity_removals()
                .iter()
                .any(|removal| removal.reason == DespawnReason::PickedUp)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_restore_round_trips_state_and_is_deterministic() {
        let (dir, mut host) = temp_host("save");
        let spawn = host.spawn();
        host.join_network_player(1, "alice", GameMode::Survival);
        let stone = host.registry().item_id("openblock:stone").expect("stone");
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").expect("pickaxe");
        let helmet = host.registry().item_id("openblock:iron_helmet").expect("helmet");
        assert_eq!(host.give_network_item(1, stone, 32), 0);
        assert_eq!(host.give_network_item(1, pickaxe, 1), 0);
        {
            let player = host.network_player_mut(1).expect("online");
            player.pos = WorldVec::new(spawn.x + 2.0, spawn.y + 1.0, spawn.z - 3.0);
            player.yaw = 1.25;
            player.pitch = -0.5;
            player.health = 13.0;
            player.hunger = 8.5;
            player.selected_slot = 2;
            player.inventory[9] = Some(ItemStack::new(stone, 9));
            player.inventory[35] = Some(ItemStack::new(pickaxe, 1).with_durability(7));
            player.equipment[0] = Some(ItemStack {
                item: helmet,
                count: 1,
                durability: Some(99),
            });
        }
        let first = network_players_state(host.registry(), host.network_players(), host.saved_network_players());
        let second = network_players_state(host.registry(), host.network_players(), host.saved_network_players());
        assert_eq!(first, second);
        host.save().expect("save");
        let reopen = Host::open(
            Arc::clone(host.registry()),
            HostConfig {
                seed: 7,
                world_name: "net-players".to_string(),
                data_dir: Some(dir.clone()),
                render_distance_chunks: 1,
                generate_budget: 8,
            },
        )
        .expect("reopen");
        let saved = reopen.saved_network_player("ALICE").expect("saved by name");
        assert_eq!(saved.pos, WorldVec::new(spawn.x + 2.0, spawn.y + 1.0, spawn.z - 3.0));
        assert_eq!(saved.health, 13.0);
        assert_eq!(saved.hunger, 8.5);
        assert_eq!(saved.selected_slot, 2);
        assert_eq!(saved.inventory[0].expect("stone").count, 32);
        assert_eq!(saved.inventory[1].expect("pickaxe").item, pickaxe);
        assert_eq!(saved.inventory[9].expect("main stone").count, 9);
        assert_eq!(saved.inventory[35].expect("main pickaxe").durability, Some(7));
        assert_eq!(saved.equipment[0].expect("helmet").durability, Some(99));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_network_players_sidecar_loads_nine_hotbar_rows_and_new_main_rows() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("ob-net-players-sidecar-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let registry = Registry::base();
        // A 1.1 record has only i0..i8 hotbar tokens (and e0..e3 equipment).
        std::fs::write(
            dir.join(NETWORK_PLAYERS_FILE),
            "alice 0.5 70.0 0.5 0 0 20 20 0 3 survival i0:openblock:stone,12 i8:openblock:apple,2 e0:openblock:iron_helmet,99\n",
        )
        .expect("write old sidecar");
        let players = load_network_players(&dir, &registry);
        let alice = players.get("alice").expect("alice");
        assert_eq!(alice.inventory[0].expect("stone").count, 12);
        assert_eq!(alice.inventory[8].expect("apple").count, 2);
        assert!(alice.inventory[HOTBAR_SLOTS..].iter().all(Option::is_none));
        assert_eq!(alice.equipment[0].expect("helmet").durability, Some(99));

        // A 1.2 record can address main slots 9..36; 36 is out of range and is
        // dropped instead of being clamped onto slot 35.
        std::fs::write(
            dir.join(NETWORK_PLAYERS_FILE),
            "alice 0.5 70.0 0.5 0 0 20 20 0 0 survival i9:openblock:dirt,4 i35:openblock:stick,3 i36:openblock:coal,1\n",
        )
        .expect("write new sidecar");
        let players = load_network_players(&dir, &registry);
        let alice = players.get("alice").expect("alice");
        assert_eq!(alice.inventory[9].expect("dirt").count, 4);
        assert_eq!(alice.inventory[35].expect("stick").count, 3);
        let coal = registry.item_id("openblock:coal").unwrap();
        assert!(
            alice.inventory.iter().flatten().all(|stack| stack.item != coal),
            "row 36 must be dropped, not clamped onto row 35"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn network_state_is_not_part_of_the_single_player_state_hash() {
        let (dir, mut host) = temp_host("hash");
        // Warm the chunk set first: loading new chunks legitimately changes the
        // hash because the hash covers resident world blocks.
        let spawn = host.spawn();
        while host.ensure_streaming_around(spawn, 2, 64) > 0 {}
        let hash = host.state_hash();
        host.join_network_player(1, "alice", GameMode::Survival);
        assert_eq!(
            hash,
            host.state_hash(),
            "joining must not change the single-player hash"
        );
        assert_eq!(
            host.give_network_item(1, host.registry().item_id("openblock:stone").unwrap(), 9),
            0
        );
        host.damage_network_player(1, 3.0, "test");
        assert_eq!(host.state_hash(), hash, "network vitals/inventory are excluded");
        host.leave_network_player(1);
        assert_eq!(host.state_hash(), hash, "offline network records are excluded");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn offline_records_restore_on_rejoin_and_dead_stays_dead() {
        let (dir, mut host) = temp_host("rejoin");
        host.join_network_player(1, "alice", GameMode::Survival);
        host.network_player_mut(1).expect("online").health = 4.0;
        assert_eq!(
            host.give_network_item(1, host.registry().item_id("openblock:apple").expect("apple"), 3),
            0
        );
        let left = host.leave_network_player(1).expect("leave");
        assert_eq!(left.id, 0);
        assert!(host.saved_network_player("alice").is_some());
        let player = host
            .join_network_player(2, "Alice", GameMode::Survival)
            .expect("rejoin");
        assert_eq!(player.id, 2);
        assert_eq!(player.health, 4.0);
        assert_eq!(
            player.inventory.iter().flatten().map(|stack| stack.count).sum::<u8>(),
            3
        );
        host.network_player_mut(2).expect("online").health = 0.0;
        host.network_player_mut(2).expect("online").dead = true;
        host.leave_network_player(2);
        let rejoin = host
            .join_network_player(3, "alice", GameMode::Survival)
            .expect("rejoin");
        assert!(rejoin.dead);
        assert!(rejoin.respawn_pending);
        assert_eq!(rejoin.health, 0.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn furnace_output_is_take_only_into_any_player_slot() {
        let (dir, mut host) = temp_host("furnace-output");
        host.join_network_player(1, "alice", GameMode::Survival).expect("join");
        let pos = BlockPos::new(3, 70, -2);
        let raw_iron = host.registry().item_id("openblock:raw_iron").expect("raw iron");
        let ingot = host.registry().item_id("openblock:iron_ingot").expect("ingot");
        let input = host.make_stack(raw_iron, 2).expect("input stack");
        let output = host.make_stack(ingot, 1).expect("output stack");
        host.furnaces.insert(
            pos,
            FurnaceState {
                input: Some(input),
                output: Some(output),
                ..FurnaceState::default()
            },
        );
        let window = NetworkWindowRef {
            pos,
            slots: 3,
            furnace: true,
        };

        // Moving the output into the furnace input (or fuel) must not swap the
        // destination stack into the take-only output slot.
        for destination in [100u16, 101] {
            assert_eq!(
                host.network_inventory_move(1, 102, destination, 1, Some(window)),
                Err("output_take_only"),
                "output -> {destination} must be rejected"
            );
            let state = host.furnace_at(pos).expect("furnace");
            assert_eq!(state.input, Some(input));
            assert_eq!(state.output, Some(output));
        }
        // The output is also not a valid destination; the exact reason depends
        // on whether the destination is occupied, but nothing may be written.
        let destination_result = host.network_inventory_move(1, 100, 102, 1, Some(window));
        assert!(
            matches!(destination_result, Err("output_take_only" | "destination_occupied")),
            "{destination_result:?}"
        );
        let state = host.furnace_at(pos).expect("furnace");
        assert_eq!(state.input, Some(input));
        assert_eq!(state.output, Some(output));
        // A hotbar destination is one legal take path.
        assert_eq!(host.network_inventory_move(1, 102, 0, 1, Some(window)), Ok(true));
        assert_eq!(host.furnace_at(pos).expect("furnace").output, None);
        assert_eq!(host.network_player(1).expect("online").inventory[0], Some(output));

        // A main-inventory destination (9..36) works too; v3 output routing is
        // not restricted to the hotbar.
        host.furnace_at_mut(pos).expect("furnace").output = Some(output);
        assert_eq!(host.network_inventory_move(1, 102, 9, 1, Some(window)), Ok(true));
        assert_eq!(host.network_player(1).expect("online").inventory[9], Some(output));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn network_player_stack_tokens_reject_extra_fields_and_invalid_durability() {
        let registry = Registry::base();
        let stone = parse_stack_token("openblock:stone,1,5", &registry, 1, false).expect("stone parses");
        assert!(
            stone.durability.is_none(),
            "non-durable sidecar stacks must not carry durability"
        );
        assert!(
            parse_stack_token("openblock:stone,1,5,extra", &registry, 1, false).is_none(),
            "extra comma fields must reject the token"
        );
        let pickaxe =
            parse_stack_token("openblock:wooden_pickaxe,1,9999", &registry, 1, false).expect("pickaxe parses");
        assert!(
            pickaxe
                .durability
                .is_some_and(|durability| (1..=59).contains(&durability))
        );
        let helmet = parse_stack_token("openblock:iron_helmet,9999", &registry, 1, true).expect("armor parses");
        assert_eq!(helmet.count, 1);
        assert!(
            helmet
                .durability
                .is_some_and(|durability| (1..=165).contains(&durability))
        );
    }

    #[test]
    fn checked_window_moves_enforce_distance_block_and_record() {
        let (dir, mut host) = temp_host("window-checked");
        host.join_network_player(1, "alice", GameMode::Survival).expect("join");
        let pos = BlockPos::new(0, 70, 0);
        let chest_block = host.registry().id("openblock:chest").expect("chest block");
        host.ensure_streaming_around(WorldVec::new(0.5, 71.0, 0.5), 0, 4);
        assert!(
            host.world_mut().set_block(pos, chest_block),
            "the test chest column must be loaded"
        );
        let dirt = host.registry().item_id("openblock:dirt").expect("dirt");
        let mut chest = ContainerState::default();
        chest.slots[0] = Some(host.make_stack(dirt, 2).expect("dirt stack"));
        host.chests.insert(pos, chest);
        let window = NetworkWindowRef {
            pos,
            slots: CHEST_SLOTS,
            furnace: false,
        };
        host.network_player_mut(1).expect("online").pos = WorldVec::new(20.5, 71.0, 0.5);
        assert_eq!(
            host.network_inventory_move_checked(1, 100, 0, 1, Some(window)),
            Err("out_of_range"),
            "a window transaction from 20 m away must be rejected"
        );
        assert_eq!(
            host.chest_at(pos).expect("chest").slots[0].expect("dirt").count,
            2,
            "the rejected move must not consume container contents"
        );
        assert!(host.network_player(1).expect("online").inventory[0].is_none());

        host.network_player_mut(1).expect("online").pos = WorldVec::new(0.5, 71.0, 0.5);
        assert_eq!(
            host.network_inventory_move_checked(1, 100, 0, 1, Some(window)),
            Ok(true),
            "a nearby move must still succeed"
        );
        assert_eq!(
            host.network_player(1).expect("online").inventory[0]
                .expect("dirt")
                .count,
            1
        );

        // A stale window whose block was replaced is rejected before any slot
        // is written, even if the block-entity record still exists.
        let stone_block = host.registry().id("openblock:stone").expect("stone block");
        assert!(host.world_mut().set_block(pos, stone_block));
        assert_eq!(
            host.network_inventory_move_checked(1, 0, 100, 1, Some(window)),
            Err("block_removed")
        );
        assert!(host.world_mut().set_block(pos, chest_block));
        host.chests.remove(&pos);
        assert_eq!(
            host.network_inventory_move_checked(1, 0, 100, 1, Some(window)),
            Err("container_missing")
        );
        assert_eq!(
            host.network_player(1).expect("online").inventory[0]
                .expect("dirt")
                .count,
            1,
            "no rejected path may remove the source stack"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn randomized_moves_conserve_stacks_across_inventory_equipment_and_container() {
        let (dir, mut host) = temp_host("inventory-fuzz");
        host.join_network_player(1, "alice", GameMode::Survival).expect("join");
        let registry = std::sync::Arc::clone(&host.registry);
        let stack = |name: &str, count: u8| {
            let item = registry.item_id(name).unwrap_or_else(|| panic!("item {name}"));
            host.make_stack(item, count).expect("stack")
        };
        let stone = stack("openblock:stone", 64);
        let apple = stack("openblock:apple", 3);
        let pickaxe = stack("openblock:wooden_pickaxe", 1);
        let helmet = stack("openblock:iron_helmet", 1);
        let dirt = stack("openblock:dirt", 10);
        {
            let player = host.network_player_mut(1).expect("online");
            player.inventory[0] = Some(stone);
            player.inventory[1] = Some(apple);
            player.inventory[2] = Some(pickaxe);
            player.inventory[3] = Some(helmet);
        }
        let pos = BlockPos::new(0, 65, 0);
        let mut container = ContainerState::default();
        container.slots[0] = Some(dirt);
        container.slots[1] = Some(host.make_stack(stone.item, 5).expect("stack"));
        host.chests.insert(pos, container);
        let window = NetworkWindowRef {
            pos,
            slots: crate::CHEST_SLOTS,
            furnace: false,
        };

        fn totals(host: &Host, pos: BlockPos) -> std::collections::BTreeMap<ItemId, u32> {
            let mut totals = std::collections::BTreeMap::new();
            let player = host.network_player(1).expect("online");
            let mut add = |stack: &Option<ItemStack>| {
                if let Some(stack) = stack {
                    *totals.entry(stack.item).or_insert(0) += u32::from(stack.count);
                }
            };
            for stack in &player.inventory {
                add(stack);
            }
            for stack in &player.equipment {
                add(stack);
            }
            if let Some(chest) = host.chest_at(pos) {
                for stack in &chest.slots {
                    add(stack);
                }
            }
            totals
        }

        let before = totals(&host, pos);
        assert_eq!(before.values().sum::<u32>(), 64 + 3 + 1 + 1 + 10 + 5);
        assert_eq!(host.network_inventory_move(1, 0, 0, 1, Some(window)), Err("same_slot"));
        assert_eq!(
            host.network_inventory_move(1, 0, 1, 0, Some(window)),
            Err("invalid_count")
        );
        assert_eq!(
            host.network_inventory_move(1, 0, 1, 255, Some(window)),
            Err("invalid_count")
        );
        assert_eq!(
            host.network_inventory_move(1, 40, 1, 1, Some(window)),
            Err("invalid_slot")
        );
        assert_eq!(
            host.network_inventory_move(1, 99, 1, 1, Some(window)),
            Err("invalid_slot")
        );
        assert_eq!(host.network_inventory_move(1, 100, 1, 1, None), Err("no_window"));

        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as usize) % bound
        };
        for iteration in 0..2_000 {
            let from = if next(4) == 0 {
                100 + next(27) as u16
            } else {
                next(40) as u16
            };
            let to = if next(4) == 0 {
                100 + next(27) as u16
            } else {
                next(40) as u16
            };
            let count = [0u8, 1, 2, 3, 5, 64, 255][next(7)];
            let _ = host.network_inventory_move(1, from, to, count, Some(window));

            let registry = host.registry();
            let player = host.network_player(1).expect("online");
            for stack in player.inventory.iter().chain(player.equipment.iter()).flatten() {
                assert!(stack.count > 0, "iteration {iteration}: zero-count stack stored");
                assert!(
                    (stack.item.0 as usize) < registry.item_len(),
                    "iteration {iteration}: bad item"
                );
                let item = registry.item(stack.item);
                let max = item.max_stack.clamp(1, MAX_STACK_SIZE);
                assert!(
                    stack.count <= max,
                    "iteration {iteration}: count {} > {max}",
                    stack.count
                );
                if let Some(durability) = stack.durability {
                    let spec = item
                        .tool
                        .map(|tool| tool.durability)
                        .or_else(|| item.armor.map(|armor| armor.durability));
                    if let Some(max_durability) = spec {
                        assert!(
                            (1..=max_durability).contains(&durability),
                            "iteration {iteration}: durability {durability} outside 1..={max_durability}"
                        );
                    }
                }
            }
            if let Some(chest) = host.chest_at(pos) {
                for stack in chest.slots.iter().flatten() {
                    assert!(stack.count > 0, "iteration {iteration}: zero-count chest stack");
                    assert!((stack.item.0 as usize) < registry.item_len());
                }
            }
            assert_eq!(
                totals(&host, pos),
                before,
                "iteration {iteration}: item conservation violated"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Parsed inventory-move address.
#[derive(Debug, Clone, Copy)]
enum SlotAddress {
    /// Player inventory slot `0..36` (hotbar `0..9`, main `9..36`).
    Player(usize),
    /// Armor slot `0..4` (wire addresses `36..40`).
    Equipment(usize),
    /// Slot inside the open window (`100..`).
    Window(usize),
}

/// Parses a wire slot address against the open-window descriptor.
fn parse_slot_address(slot: u16, window: Option<NetworkWindowRef>) -> Result<SlotAddress, &'static str> {
    match slot {
        0..=35 => Ok(SlotAddress::Player(usize::from(slot))),
        36..=39 => Ok(SlotAddress::Equipment(usize::from(slot - 36))),
        value if value >= ob_protocol::SLOT_WINDOW_START => {
            let Some(window) = window else {
                return Err("no_window");
            };
            let index = usize::from(value - ob_protocol::SLOT_WINDOW_START);
            if index >= window.slots {
                return Err("invalid_slot");
            }
            Ok(SlotAddress::Window(index))
        }
        _ => Err("invalid_slot"),
    }
}

impl SlotAddress {
    fn is_window(self) -> bool {
        matches!(self, SlotAddress::Window(_))
    }
}

impl Host {
    /// Applies one network slot move after revalidating the open window.
    ///
    /// This is the entry point the network layer uses. Window transactions are
    /// only accepted while the player is within [`MAX_CONTAINER_DISTANCE`] of
    /// the container, the container's block is still the expected furnace/chest
    /// and its block-entity record still exists. The per-tick window close in
    /// `net` is not sufficient on its own: a client can report a teleport and
    /// send a container move in the same tick before that check runs.
    ///
    /// Non-window moves (player inventory/equipment only) do not require an
    /// open window or a nearby container and behave exactly like
    /// [`Host::network_inventory_move`].
    pub(crate) fn network_inventory_move_checked(
        &mut self,
        id: u64,
        from: u16,
        to: u16,
        count: u8,
        window: Option<NetworkWindowRef>,
    ) -> Result<bool, &'static str> {
        let window_involved = from >= ob_protocol::SLOT_WINDOW_START || to >= ob_protocol::SLOT_WINDOW_START;
        if window_involved {
            let Some(window) = window else {
                return Err("no_window");
            };
            let Some(player) = self.network_players.get(&id) else {
                return Err("not_connected");
            };
            if !window.pos.in_world() || self.world.chunk(window.pos.chunk()).is_none() {
                return Err("block_removed");
            }
            let expected = if window.furnace {
                self.registry.id_or_air("openblock:furnace")
            } else {
                self.registry.id_or_air("openblock:chest")
            };
            if self.world.get_block(window.pos) != expected {
                return Err("block_removed");
            }
            let exists = if window.furnace {
                self.furnace_at(window.pos).is_some()
            } else {
                self.chest_at(window.pos).is_some()
            };
            if !exists {
                return Err("container_missing");
            }
            if window.pos.center().distance(player.pos) > MAX_CONTAINER_DISTANCE {
                return Err("out_of_range");
            }
        }
        self.network_inventory_move(id, from, to, count, window)
    }

    /// Applies one validated slot move for a network player.
    ///
    /// Slot addresses use the protocol layout: player inventory `0..36`
    /// (hotbar `0..9`, main `9..36`), equipment `36..40` and an open window from
    /// `100`. Window moves require the
    /// [`NetworkWindowRef`] the net layer is tracking for that player. Counts
    /// are bounded by the source stack; merges respect content max stacks;
    /// incompatible destinations need a full-stack swap. All writes are
    /// rollback-safe and the function never creates or destroys items.
    ///
    /// This lower-level helper intentionally trusts `window`, because the host
    /// unit tests exercise transactions without a live connection. Network
    /// callers must use [`Host::network_inventory_move_checked`].
    ///
    /// Returns `Ok(true)` when anything changed. Errors are short
    /// machine-readable reasons (`empty_source`, `destination_full`,
    /// `output_take_only`, `invalid_equipment_move`, ...).
    pub(crate) fn network_inventory_move(
        &mut self,
        id: u64,
        from: u16,
        to: u16,
        count: u8,
        window: Option<NetworkWindowRef>,
    ) -> Result<bool, &'static str> {
        if count == 0 {
            return Err("invalid_count");
        }
        if from == to {
            return Err("same_slot");
        }
        let Some(player) = self.network_players.get(&id) else {
            return Err("not_connected");
        };
        if player.dead {
            return Err("dead");
        }
        let from_address = parse_slot_address(from, window)?;
        let to_address = parse_slot_address(to, window)?;
        match (from_address, to_address) {
            (SlotAddress::Equipment(_), SlotAddress::Equipment(_))
            | (SlotAddress::Equipment(_), SlotAddress::Window(_))
            | (SlotAddress::Window(_), SlotAddress::Equipment(_)) => {
                return Err("invalid_equipment_move");
            }
            _ => {}
        }

        let source = self.read_slot(id, from_address, window)?.ok_or("empty_source")?;
        if count > source.count {
            return Err("invalid_count");
        }
        let destination = self.read_slot(id, to_address, window)?;

        if let SlotAddress::Equipment(index) = to_address {
            let Some(armor) = self.registry.item(source.item).armor else {
                return Err("not_armor");
            };
            if crate::armor_slot_index(armor.slot) != index {
                return Err("wrong_armor_slot");
            }
            if count != 1 {
                return Err("equipment_single_item");
            }
        }
        if let SlotAddress::Equipment(index) = from_address {
            if count != source.count {
                return Err("equipment_single_item");
            }
            if let Some(swap) = destination {
                // A swap must put valid armor for the same slot back on the
                // body.
                let Some(armor) = self.registry.item(swap.item).armor else {
                    return Err("invalid_equipment_swap");
                };
                if crate::armor_slot_index(armor.slot) != index {
                    return Err("invalid_equipment_swap");
                }
            }
        }
        let from_furnace_output = from_address.is_window()
            && window.is_some_and(|window| window.furnace)
            && matches!(from_address, SlotAddress::Window(2));
        if from_furnace_output && !matches!(to_address, SlotAddress::Player(_)) {
            // The furnace output is take-only and the only legal destination is
            // the player's hotbar. A window destination could otherwise swap the
            // destination stack *into* the output slot, because the source write
            // below has to allow clearing the output.
            return Err("output_take_only");
        }
        if from_address.is_window()
            && let Some(window) = window
            && window.furnace
            && matches!(from_address, SlotAddress::Window(2))
            && destination.is_some_and(|destination| {
                destination.item != source.item || destination.durability != source.durability
            })
        {
            // Never swap an unrelated item into the furnace output slot.
            return Err("destination_occupied");
        }

        let max_stack = self.registry.item(source.item).max_stack.clamp(1, MAX_STACK_SIZE);
        let (new_source, new_destination) = match destination {
            None => {
                let moved = count.min(max_stack);
                let mut destination_stack = source;
                destination_stack.count = moved;
                let mut remainder = source;
                remainder.count -= moved;
                ((remainder.count > 0).then_some(remainder), Some(destination_stack))
            }
            Some(destination_stack)
                if destination_stack.item == source.item
                    && destination_stack.durability == source.durability
                    && max_stack > 1 =>
            {
                let room = max_stack.saturating_sub(destination_stack.count);
                let moved = count.min(room);
                if moved == 0 {
                    return Err("destination_full");
                }
                let mut remainder = source;
                remainder.count -= moved;
                let mut merged = destination_stack;
                merged.count += moved;
                ((remainder.count > 0).then_some(remainder), Some(merged))
            }
            Some(destination_stack) => {
                if count != source.count {
                    return Err("destination_occupied");
                }
                (Some(destination_stack), Some(source))
            }
        };

        let destination_old = self.write_slot(id, to_address, new_destination, window, false)?;
        let allow_output =
            window.is_some_and(|window| window.furnace) && matches!(from_address, SlotAddress::Window(2));
        match self.write_slot(id, from_address, new_source, window, allow_output) {
            Ok(_) => {}
            Err(error) => {
                // Restore the destination; the source was not written yet.
                let _ = self.write_slot(id, to_address, destination_old, window, false);
                return Err(error);
            }
        }
        self.mark_player_dirty();
        self.mark_network_inventory_dirty(id);
        Ok(true)
    }

    /// Reads one player or window slot.
    fn read_slot(
        &self,
        id: u64,
        address: SlotAddress,
        window: Option<NetworkWindowRef>,
    ) -> Result<Option<ItemStack>, &'static str> {
        match address {
            SlotAddress::Player(slot) => Ok(self
                .network_players
                .get(&id)
                .and_then(|player| player.inventory.get(slot).copied().flatten())),
            SlotAddress::Equipment(index) => Ok(self
                .network_players
                .get(&id)
                .and_then(|player| player.equipment.get(index).copied().flatten())),
            SlotAddress::Window(index) => {
                let Some(window) = window else {
                    return Err("no_window");
                };
                self.read_container_slot(window, index)
            }
        }
    }

    /// Writes one player or window slot, returning the previous stack.
    fn write_slot(
        &mut self,
        id: u64,
        address: SlotAddress,
        stack: Option<ItemStack>,
        window: Option<NetworkWindowRef>,
        allow_output: bool,
    ) -> Result<Option<ItemStack>, &'static str> {
        let stack = match stack {
            Some(stack) if stack.count > 0 => {
                Some(crate::sanitize_stack(&self.registry, stack).ok_or("invalid_stack")?)
            }
            _ => None,
        };
        match address {
            SlotAddress::Player(slot) => {
                let Some(player) = self.network_players.get_mut(&id) else {
                    return Err("not_connected");
                };
                let previous = player.inventory[slot];
                player.inventory[slot] = stack;
                Ok(previous)
            }
            SlotAddress::Equipment(index) => {
                let Some(player) = self.network_players.get_mut(&id) else {
                    return Err("not_connected");
                };
                let Some(stack) = stack else {
                    return Ok(player.equipment[index].take());
                };
                if stack.count != 1 {
                    return Err("equipment_single_item");
                }
                let Some(armor) = self.registry.item(stack.item).armor else {
                    return Err("not_armor");
                };
                if crate::armor_slot_index(armor.slot) != index {
                    return Err("wrong_armor_slot");
                }
                Ok(player.equipment[index].replace(stack))
            }
            SlotAddress::Window(index) => {
                let Some(window) = window else {
                    return Err("no_window");
                };
                self.write_container_slot(window, index, stack, allow_output)
            }
        }
    }

    /// Reads one slot of a furnace/chest window.
    fn read_container_slot(&self, window: NetworkWindowRef, index: usize) -> Result<Option<ItemStack>, &'static str> {
        if window.furnace {
            if index >= 3 {
                return Err("invalid_slot");
            }
            let state = self.furnace_at(window.pos).ok_or("container_missing")?;
            Ok(match index {
                0 => state.input,
                1 => state.fuel,
                _ => state.output,
            })
        } else {
            let state = self.chest_at(window.pos).ok_or("container_missing")?;
            Ok(state.slots.get(index).copied().flatten())
        }
    }

    /// Writes one slot of a furnace/chest window.
    fn write_container_slot(
        &mut self,
        window: NetworkWindowRef,
        index: usize,
        stack: Option<ItemStack>,
        allow_output: bool,
    ) -> Result<Option<ItemStack>, &'static str> {
        if window.furnace {
            if index >= 3 {
                return Err("invalid_slot");
            }
            if index == 2 && !allow_output {
                return Err("output_take_only");
            }
            let state = self.furnace_at_mut(window.pos).ok_or("container_missing")?;
            let slot = match index {
                0 => &mut state.input,
                1 => &mut state.fuel,
                _ => &mut state.output,
            };
            let previous = *slot;
            *slot = stack;
            self.mark_player_dirty();
            Ok(previous)
        } else {
            let state = self.chest_at_mut(window.pos).ok_or("container_missing")?;
            let slot = state.slots.get_mut(index).ok_or("invalid_slot")?;
            let previous = *slot;
            *slot = stack;
            self.mark_player_dirty();
            Ok(previous)
        }
    }
}

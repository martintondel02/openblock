//! The authoritative host shared by single-player, LAN and dedicated servers.
//!
//! The same [`Host`] drives the client's integrated server (via the client
//! crate) and the headless `openblock-server` binary. It owns the world, the
//! generator and the player state, advances a fixed 20 Hz tick, streams chunks
//! around the player and persists the world.
//!
//! Networking (`docs/spec/18-networking-multiplayer.md`) attaches to this host
//! at MS-06; v0.0.1 has no sockets.

#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ob_content::{BlockId, ItemId, Registry};
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_sim::{
    BlockAction, MAX_HEALTH, MAX_HUNGER, Player, PlayerInput, REACH, TICK_DT, advance_vitals, apply_block_actions,
    apply_damage, apply_fall_damage, eat, reset_vitals, spawn_position, step_player,
};
use ob_world::{ChunkColumn, World};
use ob_worldgen::Generator;
use rayon::prelude::*;

/// Number of slots in the player's hotbar.
pub const HOTBAR_SLOTS: usize = 9;
/// Maximum number of items in one inventory stack.
pub const MAX_STACK_SIZE: u8 = 64;
/// One-in-this-many chance for a broken leaf block to also drop an apple.
const APPLE_DROP_MODULUS: u64 = 6;

/// The gameplay mode the host simulates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GameMode {
    /// Health, hunger, fall damage, starvation and item consumption apply.
    Survival,
    /// Instant block interaction, full vitals, no item consumption or drops.
    #[default]
    Creative,
}

impl GameMode {
    /// Stable display name, independent of localization.
    pub const fn name(self) -> &'static str {
        match self {
            GameMode::Survival => "survival",
            GameMode::Creative => "creative",
        }
    }
}

/// One inventory stack. `count` is `1..=`[`MAX_STACK_SIZE`]; an empty slot is
/// represented by `None` in the inventory rather than by a zero count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    /// Item type resource id (session-local index).
    pub item: ItemId,
    /// Number of items in the stack.
    pub count: u8,
}

impl ItemStack {
    /// Creates a stack, clamping the count to [`MAX_STACK_SIZE`].
    pub const fn new(item: ItemId, count: u8) -> Self {
        let count = if count > MAX_STACK_SIZE { MAX_STACK_SIZE } else { count };
        Self { item, count }
    }

    /// Whether the stack holds no items.
    pub fn is_empty(self) -> bool {
        self.count == 0
    }
}

/// Result of one break/place interaction, including survival loot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionOutcome {
    /// Whether the world or the player inventory changed.
    pub changed: bool,
    /// The block that was broken, when a break succeeded.
    pub broke: Option<BlockId>,
    /// The block that was placed, when a placement succeeded.
    pub placed: Option<BlockId>,
    /// Items the broken block dropped, whether or not they fitted.
    pub drops: Vec<ItemStack>,
    /// Dropped items that could not enter the inventory.
    pub pickup_overflow: Vec<ItemStack>,
}

/// Configuration for a host instance.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// World seed used when a new world is created.
    pub seed: u64,
    /// World name (directory name under the data directory).
    pub world_name: String,
    /// Data directory root; `None` uses [`ob_core::data_dir`].
    pub data_dir: Option<PathBuf>,
    /// Horizontal streaming radius in chunks (default 6).
    pub render_distance_chunks: i32,
    /// Maximum chunks generated per frame/tick call.
    pub generate_budget: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            world_name: "world".to_string(),
            data_dir: None,
            render_distance_chunks: 6,
            generate_budget: 2,
        }
    }
}

/// Errors from host setup and persistence.
#[derive(Debug)]
pub enum HostError {
    /// Filesystem error while loading or saving.
    Io(io::Error),
    /// The world file could not be decoded.
    Save(ob_world::SaveError),
    /// The save belongs to a newer save format.
    UnsupportedSave(u16),
    /// A player state file was malformed.
    PlayerState(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Io(err) => write!(f, "io error: {err}"),
            HostError::Save(err) => write!(f, "world save error: {err}"),
            HostError::UnsupportedSave(v) => write!(f, "save format {v} is newer than this build"),
            HostError::PlayerState(reason) => write!(f, "invalid player state: {reason}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<io::Error> for HostError {
    fn from(err: io::Error) -> Self {
        HostError::Io(err)
    }
}

impl From<ob_world::SaveError> for HostError {
    fn from(err: ob_world::SaveError) -> Self {
        HostError::Save(err)
    }
}

/// The authoritative game host.
pub struct Host {
    world: World,
    generator: Generator,
    player: Player,
    registry: Arc<Registry>,
    config: HostConfig,
    tick_count: u64,
    unloaded: Vec<ChunkPos>,
    world_dir: PathBuf,
    dirty_player: bool,
    game_mode: GameMode,
    inventory: [Option<ItemStack>; HOTBAR_SLOTS],
    hotbar_selection: usize,
    spawn: WorldVec,
}

impl Host {
    /// Creates a host, loading the saved world, player and inventory if they
    /// exist at the configured path.
    ///
    /// The host always starts in [`GameMode::Creative`] for backwards
    /// compatibility; a survival game calls [`Host::set_game_mode`] after open.
    pub fn open(registry: Arc<Registry>, config: HostConfig) -> Result<Self, HostError> {
        let world_dir = world_dir(&config);
        let world_path = world_dir.join("world.obs");
        let mut host;
        if world_path.exists() {
            let bytes = std::fs::read(&world_path)?;
            let world = ob_world::from_bytes(Arc::clone(&registry), &bytes)?;
            let generator = Generator::new(world.seed());
            let (player, saved_ticks) = load_player(&world_dir, &world, &generator);
            host = Self {
                world,
                generator,
                player,
                registry,
                config,
                tick_count: saved_ticks,
                unloaded: Vec::new(),
                world_dir,
                dirty_player: false,
                game_mode: GameMode::default(),
                inventory: [None; HOTBAR_SLOTS],
                hotbar_selection: 0,
                spawn: WorldVec::ZERO,
            };
            host.inventory = load_inventory(&host.world_dir, &host.registry);
            host.ensure_streaming(2);
            // Stream the area first so a saved player near spawn gets an exact
            // surface; find_spawn falls back to generated height elsewhere.
            host.spawn = find_spawn(&host.world, &host.generator);
        } else {
            let generator = Generator::new(config.seed);
            let world = World::new(Arc::clone(&registry), config.seed);
            host = Self {
                world,
                generator,
                player: Player::new(WorldVec::new(0.5, SEA_LEVEL as f64 + 20.0, 0.5)),
                registry,
                config,
                tick_count: 0,
                unloaded: Vec::new(),
                world_dir,
                dirty_player: false,
                game_mode: GameMode::default(),
                inventory: [None; HOTBAR_SLOTS],
                hotbar_selection: 0,
                spawn: WorldVec::ZERO,
            };
            host.ensure_streaming(3);
            let spawn = find_spawn(&host.world, &host.generator);
            host.spawn = spawn;
            host.player = Player::new(spawn);
            host.save()?;
        }
        host.ensure_streaming(2);
        Ok(host)
    }

    /// The world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The world, mutably (commands/debug tools).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// The player.
    pub fn player(&self) -> &Player {
        &self.player
    }

    /// The player, mutably.
    pub fn player_mut(&mut self) -> &mut Player {
        &mut self.player
    }

    /// The content registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Number of completed ticks.
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Host configuration.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Streaming radius in chunks.
    pub fn render_distance_chunks(&self) -> i32 {
        self.config.render_distance_chunks
    }

    /// Changes the streaming radius at runtime.
    ///
    /// Clamped to 1..=32 chunks. Newly needed columns are generated by the next
    /// [`Host::ensure_streaming`] call; columns beyond the new radius are unloaded
    /// then too. Returns whether the radius changed.
    pub fn set_render_distance_chunks(&mut self, chunks: i32) -> bool {
        let chunks = chunks.clamp(1, 32);
        if chunks == self.config.render_distance_chunks {
            return false;
        }
        self.config.render_distance_chunks = chunks;
        true
    }

    /// The active gameplay mode.
    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    /// Changes the gameplay mode.
    ///
    /// Switching to [`GameMode::Creative`] immediately restores full vitals.
    /// The mode is session state and is not persisted; the client selects it
    /// after [`Host::open`].
    pub fn set_game_mode(&mut self, mode: GameMode) {
        self.game_mode = mode;
        if mode == GameMode::Creative {
            reset_vitals(&mut self.player);
        }
        self.mark_player_dirty();
    }

    /// The nine hotbar slots; `None` is an empty slot.
    pub fn inventory(&self) -> &[Option<ItemStack>; HOTBAR_SLOTS] {
        &self.inventory
    }

    /// Index of the selected hotbar slot, `0..`[`HOTBAR_SLOTS`].
    pub fn hotbar_selection(&self) -> usize {
        self.hotbar_selection
    }

    /// Selects a hotbar slot, wrapping out-of-range indices modulo
    /// [`HOTBAR_SLOTS`].
    pub fn set_hotbar_selection(&mut self, slot: usize) {
        self.hotbar_selection = slot % HOTBAR_SLOTS;
        self.mark_player_dirty();
    }

    /// The stack in the selected hotbar slot.
    pub fn selected_stack(&self) -> Option<ItemStack> {
        self.inventory[self.hotbar_selection]
    }

    /// Adds items to matching stacks first, then to empty slots.
    ///
    /// Returns the number of items that did not fit. Stacks never exceed
    /// [`MAX_STACK_SIZE`]. An unknown [`ItemId`] is rejected and returned in
    /// full rather than panicking.
    pub fn give_item(&mut self, item: ItemId, count: u8) -> u8 {
        if count == 0 || item.0 as usize >= self.registry.item_len() {
            return count;
        }
        let mut remaining = count;
        for stack in self.inventory.iter_mut().flatten() {
            if stack.item == item && stack.count < MAX_STACK_SIZE {
                let added = remaining.min(MAX_STACK_SIZE - stack.count);
                stack.count += added;
                remaining -= added;
                if remaining == 0 {
                    break;
                }
            }
        }
        if remaining > 0 {
            for slot in self.inventory.iter_mut() {
                if slot.is_none() {
                    let added = remaining.min(MAX_STACK_SIZE);
                    *slot = Some(ItemStack::new(item, added));
                    remaining -= added;
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
        if remaining < count {
            self.mark_player_dirty();
        }
        remaining
    }

    /// Removes items from the selected hotbar slot in survival mode.
    ///
    /// Returns whether the whole count was consumed. The slot becomes empty
    /// when its count reaches zero.
    pub fn consume_selected(&mut self, count: u8) -> bool {
        if self.game_mode != GameMode::Survival || count == 0 {
            return false;
        }
        let slot = self.hotbar_selection;
        let Some(stack) = self.inventory[slot] else {
            return false;
        };
        if stack.count < count {
            return false;
        }
        let left = stack.count - count;
        self.inventory[slot] = if left == 0 {
            None
        } else {
            Some(ItemStack::new(stack.item, left))
        };
        self.mark_player_dirty();
        true
    }

    /// Eats one item from the selected hotbar slot in survival mode.
    ///
    /// Returns whether the item was eaten. Only edible items, living players
    /// and players below full hunger qualify; success restores the item's
    /// hunger points and consumes exactly one item.
    pub fn eat_selected(&mut self) -> bool {
        if self.game_mode != GameMode::Survival {
            return false;
        }
        let Some(stack) = self.selected_stack() else {
            return false;
        };
        let Some(food) = self.registry.item(stack.item).food else {
            return false;
        };
        if !eat(&mut self.player, food) {
            return false;
        }
        let consumed = self.consume_selected(1);
        debug_assert!(consumed, "an edible selected stack must be consumable");
        self.mark_player_dirty();
        true
    }

    /// Whether the player is dead. Creative players are revived by the next
    /// tick, and switching to creative revives immediately.
    pub fn is_dead(&self) -> bool {
        self.player.dead
    }

    /// Applies survival damage. Creative players are immune.
    pub fn damage(&mut self, amount: f32) {
        if self.game_mode != GameMode::Survival {
            return;
        }
        let dealt = apply_damage(&mut self.player, amount);
        if dealt > 0.0 {
            self.mark_player_dirty();
        }
    }

    /// Respawns at the spawn point found when the host opened.
    ///
    /// Vitals are reset, velocity is cleared and the player is marked dirty.
    /// The inventory, look direction and game mode are kept, and chunks around
    /// the spawn are streamed immediately so the player does not fall through
    /// an unloaded world.
    pub fn respawn(&mut self) {
        self.player.pos = self.spawn;
        self.player.velocity = WorldVec::ZERO;
        self.player.on_ground = false;
        reset_vitals(&mut self.player);
        self.ensure_streaming(2);
        self.mark_player_dirty();
    }

    /// Advances one simulation tick with the given player input.
    ///
    /// In survival this applies fall damage and the hunger/starvation/regen
    /// rules; in creative vitals are restored every tick and no damage applies.
    pub fn tick(&mut self, input: &PlayerInput) {
        let events = step_player(&self.world, &mut self.player, input, TICK_DT);
        self.tick_count += 1;
        match self.game_mode {
            GameMode::Survival => {
                if !self.player.dead {
                    if events.landing_impact > 0.0 {
                        apply_fall_damage(&mut self.player, events.landing_impact);
                    }
                    if !self.player.dead {
                        advance_vitals(&mut self.player, &events, TICK_DT);
                    }
                    if self.player.health <= 0.0 {
                        self.player.health = 0.0;
                        self.player.dead = true;
                        self.player.velocity = WorldVec::ZERO;
                    }
                }
            }
            GameMode::Creative => {
                reset_vitals(&mut self.player);
            }
        }
        if self.player.dead {
            self.player.velocity = WorldVec::ZERO;
        }
        self.mark_player_dirty();
    }

    /// Applies a break/place action from the player camera.
    ///
    /// This is the mode-aware entry point: it returns whether anything changed
    /// and discards the loot details. Use [`Host::apply_action_with_outcome`]
    /// when the client needs drops and overflow.
    pub fn apply_action(&mut self, action: &BlockAction) -> bool {
        self.apply_action_with_outcome(action).changed
    }

    /// Applies a break/place action and reports what happened.
    ///
    /// Creative mode delegates to the same instant break/place path as v0.0.6
    /// and never touches the inventory. Survival mode enforces breakability,
    /// awards block drops (with a deterministic rare apple from leaves) and
    /// consumes one matching selected item when placing.
    pub fn apply_action_with_outcome(&mut self, action: &BlockAction) -> ActionOutcome {
        let mut outcome = ActionOutcome::default();

        if self.game_mode == GameMode::Creative {
            outcome.changed = apply_block_actions(&mut self.world, &self.player, action);
            if outcome.changed {
                self.mark_player_dirty();
            }
            return outcome;
        }

        if action.break_block
            && let Some(hit) = self.world.raycast(self.player.eye(), self.player.forward(), REACH)
        {
            let block = self.world.get_block(hit.block);
            if block != BlockId::AIR
                && self.registry.block(block).breakable()
                && self.world.set_block(hit.block, BlockId::AIR)
            {
                outcome.changed = true;
                outcome.broke = Some(block);
                if let Some((item, count)) = self.registry.block_drop(block) {
                    outcome.drops.push(ItemStack::new(item, count));
                }
                let leaves = self.registry.id_or_air("openblock:leaves");
                if block == leaves
                    && apple_drop(self.world.seed(), hit.block)
                    && let Some(apple) = self.registry.item_id("openblock:apple")
                {
                    outcome.drops.push(ItemStack::new(apple, 1));
                }
                for drop in outcome.drops.clone() {
                    let overflow = self.give_item(drop.item, drop.count);
                    if overflow > 0 {
                        outcome.pickup_overflow.push(ItemStack::new(drop.item, overflow));
                    }
                }
            }
        }

        if action.place_block && action.place != BlockId::AIR {
            let matches_selection = self
                .selected_stack()
                .is_some_and(|stack| stack.count > 0 && self.registry.item(stack.item).block == Some(action.place));
            if matches_selection {
                let place_only = BlockAction {
                    break_block: false,
                    place_block: true,
                    place: action.place,
                };
                if apply_block_actions(&mut self.world, &self.player, &place_only) {
                    let consumed = self.consume_selected(1);
                    debug_assert!(consumed, "a matching selected stack must be consumable");
                    outcome.changed = true;
                    outcome.placed = Some(action.place);
                }
            }
        }

        if outcome.changed {
            self.mark_player_dirty();
        }
        outcome
    }

    /// Generates up to `budget` missing chunk columns around the player and
    /// unloads columns beyond the streaming radius. Returns the number of
    /// newly generated chunks.
    pub fn ensure_streaming(&mut self, budget: usize) -> usize {
        let center = self.player.pos;
        let center_chunk = BlockPos::new(
            center.x.floor() as i32,
            center.y.floor() as i32,
            center.z.floor() as i32,
        )
        .chunk();
        let radius = self.config.render_distance_chunks.max(1);

        // Collect the missing positions in the same deterministic square-ring
        // order the serial path used, truncated to the budget. The world is not
        // mutated while collecting, so this list is exactly what the serial
        // loop would have generated.
        let mut positions = Vec::new();
        if budget > 0 {
            'outer: for ring in 0..=radius {
                for dz in -ring..=ring {
                    for dx in -ring..=ring {
                        if dx.abs() != ring && dz.abs() != ring {
                            continue;
                        }
                        let pos = ChunkPos::new(center_chunk.x + dx, center_chunk.z + dz);
                        if self.world.chunk(pos).is_some() {
                            continue;
                        }
                        positions.push(pos);
                        if positions.len() >= budget {
                            break 'outer;
                        }
                    }
                }
            }
        }

        // Generate the whole batch on the global rayon pool (which respects
        // RAYON_NUM_THREADS). Chunk generation is a pure function of the seed
        // and position; sorting by (cx, cz) before insertion makes the final
        // world identical regardless of worker count or completion order.
        let generator = self.generator;
        let registry = &self.registry;
        let mut batch: Vec<(ChunkPos, ChunkColumn)> = positions
            .par_iter()
            .map(|pos| (*pos, generator.generate_chunk(*pos, registry)))
            .collect();
        batch.sort_by_key(|(pos, _)| (pos.x, pos.z));
        let generated = batch.len();
        for (_, chunk) in batch {
            self.world.insert_chunk(chunk);
        }
        // Unload chunks outside radius + 2 (hysteresis).
        // Chebyshev hysteresis must match the square ring generation above;
        // a squared Euclidean test would immediately unload ring corners.
        let unload_radius = radius + 2;
        let mut to_unload = Vec::new();
        for (pos, _) in self.world.chunks() {
            let dx = (pos.x - center_chunk.x).abs();
            let dz = (pos.z - center_chunk.z).abs();
            if dx > unload_radius || dz > unload_radius {
                to_unload.push(*pos);
            }
        }
        for pos in to_unload {
            self.world.remove_chunk(pos);
            self.unloaded.push(pos);
        }
        generated
    }

    /// Takes the list of chunk columns unloaded since the last call (the client
    /// removes their meshes).
    pub fn take_unloaded_chunks(&mut self) -> Vec<ChunkPos> {
        std::mem::take(&mut self.unloaded)
    }

    /// Takes the sections whose meshes are stale.
    pub fn take_dirty_sections(&mut self) -> Vec<SectionPos> {
        self.world.take_dirty_sections()
    }

    /// Number of pending dirty sections (without draining them).
    pub fn dirty_sections(&self) -> usize {
        self.world.dirty_count()
    }

    /// Whether the player moved since the last save (autosave hint).
    pub fn player_dirty(&self) -> bool {
        self.dirty_player
    }

    fn mark_player_dirty(&mut self) {
        self.dirty_player = true;
    }

    /// Saves the world, player state and inventory atomically.
    pub fn save(&mut self) -> Result<(), HostError> {
        std::fs::create_dir_all(&self.world_dir)?;
        let bytes = ob_world::to_bytes(&self.world);
        write_atomic(&self.world_dir.join("world.obs"), &bytes)?;
        write_atomic(
            &self.world_dir.join("player.txt"),
            player_state(&self.player, self.tick_count).as_bytes(),
        )?;
        write_atomic(
            &self.world_dir.join("inventory.txt"),
            inventory_state(&self.registry, &self.inventory).as_bytes(),
        )?;
        self.dirty_player = false;
        Ok(())
    }

    /// Deterministic hash of the simulation state (world blocks in canonical
    /// order, player position and vitals quantised, inventory and tick count).
    /// Used by the smoke test and by cross-platform determinism checks.
    pub fn state_hash(&self) -> u64 {
        let mut hasher = StableHasher::new();
        hasher.write_u64(self.world.seed());
        hasher.write_u64(self.tick_count);
        hasher.write_i64((self.player.pos.x * 1000.0) as i64);
        hasher.write_i64((self.player.pos.y * 1000.0) as i64);
        hasher.write_i64((self.player.pos.z * 1000.0) as i64);
        hasher.write_i32((self.player.health * 1000.0) as i32);
        hasher.write_i32((self.player.hunger * 1000.0) as i32);
        hasher.write_u8(self.player.dead as u8);
        for (slot, stack) in self.inventory.iter().enumerate() {
            let Some(stack) = stack else {
                continue;
            };
            if stack.is_empty() {
                continue;
            }
            hasher.write_u8(slot as u8);
            let name = self.registry.item(stack.item).id;
            hasher.write_u16(name.len() as u16);
            hasher.write(name.as_bytes());
            hasher.write_u8(stack.count);
        }
        let mut chunks: Vec<_> = self.world.chunks().collect();
        chunks.sort_by_key(|(pos, _)| (pos.x, pos.z));
        for (pos, _chunk) in chunks {
            hasher.write_i32(pos.x);
            hasher.write_i32(pos.z);
            let (ox, oz) = pos.origin();
            for y in ob_math::WORLD_MIN_Y..ob_math::WORLD_MIN_Y + (ob_math::N_SECTIONS * 16) as i32 {
                for z in 0..16 {
                    for x in 0..16 {
                        let block = self.world.get_block(BlockPos::new(ox + x, y, oz + z));
                        hasher.write_u16(block.0);
                    }
                }
            }
        }
        hasher.finish()
    }

    /// Approximate heap memory used by the host.
    pub fn memory_bytes(&self) -> usize {
        self.world.heap_bytes()
    }
}

fn world_dir(config: &HostConfig) -> PathBuf {
    config
        .data_dir
        .clone()
        .unwrap_or_else(ob_core::data_dir)
        .join("worlds")
        .join(&config.world_name)
}

fn find_spawn(world: &World, generator: &Generator) -> WorldVec {
    for radius in 0..64i32 {
        for angle_index in 0..16 {
            let angle = angle_index as f64 * std::f64::consts::TAU / 16.0;
            let x = (angle.cos() * radius as f64 * 8.0) as i32;
            let z = (angle.sin() * radius as f64 * 8.0) as i32;
            let height = generator.height_at(x, z);
            if height > SEA_LEVEL + 2 {
                // Prefer the loaded surface (it includes trees and water); when
                // the column is not streamed yet, the generator height is a safe
                // conservative fallback.
                return match world.height_at(x, z) {
                    Some(_) => spawn_position(world, x, z),
                    None => WorldVec::new(x as f64 + 0.5, generator.height_at(x, z) as f64 + 1.0, z as f64 + 0.5),
                };
            }
        }
    }
    WorldVec::new(0.5, generator.height_at(0, 0) as f64 + 2.0, 0.5)
}

/// Serialized player state: ten whitespace-separated fields.
///
/// `x y z yaw pitch flying ticks health hunger dead`
fn player_state(player: &Player, ticks: u64) -> String {
    format!(
        "{:?} {:?} {:?} {:?} {:?} {} {} {:?} {:?} {}\n",
        player.pos.x,
        player.pos.y,
        player.pos.z,
        player.yaw,
        player.pitch,
        player.flying as u8,
        ticks,
        player.health,
        player.hunger,
        player.dead as u8,
    )
}

fn parse_player_state(parts: &[&str]) -> Option<(Player, u64)> {
    if parts.len() != 7 && parts.len() != 10 {
        return None;
    }
    let x = parts[0].parse::<f64>().ok()?;
    let y = parts[1].parse::<f64>().ok()?;
    let z = parts[2].parse::<f64>().ok()?;
    let yaw = parts[3].parse::<f32>().ok()?;
    let pitch = parts[4].parse::<f32>().ok()?;
    let flying = parts[5].parse::<u8>().ok()?;
    let ticks = parts[6].parse::<u64>().ok()?;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() || !yaw.is_finite() || !pitch.is_finite() {
        return None;
    }

    // Old (7-field) saves predate survival and default to full health/hunger.
    let health = if parts.len() == 10 {
        parts[7].parse::<f32>().ok()?
    } else {
        MAX_HEALTH
    };
    let hunger = if parts.len() == 10 {
        parts[8].parse::<f32>().ok()?
    } else {
        MAX_HUNGER
    };
    let dead = if parts.len() == 10 {
        parts[9].parse::<u8>().ok()? != 0
    } else {
        false
    };

    let mut player = Player::new(WorldVec::new(x, y, z));
    player.yaw = yaw;
    player.pitch = pitch;
    player.flying = flying != 0;
    player.health = if health.is_finite() {
        health.clamp(0.0, MAX_HEALTH)
    } else {
        MAX_HEALTH
    };
    player.hunger = if hunger.is_finite() {
        hunger.clamp(0.0, MAX_HUNGER)
    } else {
        MAX_HUNGER
    };
    player.dead = dead || player.health <= 0.0;
    if player.health <= 0.0 {
        player.health = 0.0;
    }
    Some((player, ticks))
}

fn load_player(world_dir: &Path, world: &World, generator: &Generator) -> (Player, u64) {
    let path = world_dir.join("player.txt");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if let Some(state) = parse_player_state(&parts) {
            return state;
        }
        if !parts.is_empty() {
            log::warn!("ignoring malformed player state in {}", path.display());
        }
    }
    let spawn = find_spawn(world, generator);
    (Player::new(spawn), 0)
}

/// Serialized inventory: one `slot item_resource_id count` line per non-empty
/// slot in slot order. Empty lines and empty slots are skipped.
fn inventory_state(registry: &Registry, inventory: &[Option<ItemStack>; HOTBAR_SLOTS]) -> String {
    let mut text = String::new();
    for (slot, stack) in inventory.iter().enumerate() {
        let Some(stack) = stack else {
            continue;
        };
        if stack.is_empty() {
            continue;
        }
        text.push_str(&format!("{slot} {} {}\n", registry.item(stack.item).id, stack.count));
    }
    text
}

/// Loads `inventory.txt`, dropping unknown items and malformed lines with a
/// warning. A missing file (old worlds) is an empty inventory.
fn load_inventory(world_dir: &Path, registry: &Registry) -> [Option<ItemStack>; HOTBAR_SLOTS] {
    let mut inventory = [None; HOTBAR_SLOTS];
    let path = world_dir.join("inventory.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "could not read {}: {err}; starting with an empty inventory",
                    path.display()
                );
            }
            return inventory;
        }
    };

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let mut fields = line.split_whitespace();
        let (Some(slot_text), Some(name), Some(count_text), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            if !line.trim().is_empty() {
                log::warn!("inventory.txt:{line_number}: ignoring malformed line");
            }
            continue;
        };
        let Ok(slot) = slot_text.parse::<usize>() else {
            log::warn!("inventory.txt:{line_number}: invalid slot '{slot_text}'");
            continue;
        };
        let Some(item) = registry.item_id(name) else {
            log::warn!("inventory.txt:{line_number}: unknown item '{name}', dropping it");
            continue;
        };
        let Ok(count) = count_text.parse::<u32>() else {
            log::warn!("inventory.txt:{line_number}: invalid count '{count_text}'");
            continue;
        };
        let slot = slot.min(HOTBAR_SLOTS - 1);
        let count = count.clamp(1, u32::from(MAX_STACK_SIZE)) as u8;
        inventory[slot] = Some(ItemStack::new(item, count));
    }
    inventory
}

/// Deterministic rare apple drop for a broken leaf block.
///
/// A pure hash of the world seed and the block position: the same block always
/// yields the same result, there is no RNG state to persist, and roughly one in
/// [`APPLE_DROP_MODULUS`] leaves drops an apple.
fn apple_drop(seed: u64, pos: BlockPos) -> bool {
    let mut hasher = StableHasher::new();
    hasher.write_u64(seed);
    hasher.write_i32(pos.x);
    hasher.write_i32(pos.y);
    hasher.write_i32(pos.z);
    hasher.write_u8(0xA7);
    let mut hash = hasher.finish();
    // Final avalanche so the modulo uses well-mixed bits.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash.is_multiple_of(APPLE_DROP_MODULUS)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// FNV-1a 64-bit stable hasher for cross-platform state hashes.
struct StableHasher(u64);

impl StableHasher {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    fn write_u8(&mut self, v: u8) {
        self.write(&[v]);
    }
    fn write_u16(&mut self, v: u16) {
        self.write(&v.to_le_bytes());
    }
    fn write_i32(&mut self, v: i32) {
        self.write(&v.to_le_bytes());
    }
    fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    fn write_i64(&mut self, v: i64) {
        self.write(&v.to_le_bytes());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, CHUNK_SIZE, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
    use ob_sim::{BlockAction, MAX_HEALTH, MAX_HUNGER, PlayerInput, REACH};
    use ob_world::ChunkColumn;
    use ob_worldgen::Generator;

    use super::{GameMode, HOTBAR_SLOTS, Host, HostConfig, ItemStack, StableHasher, apple_drop};

    /// Test-only temporary data directory removed on drop (best effort).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir().join(format!(
                "openblock-server-{label}-{}-{nanos}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temporary data directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Stable hash of every block in one chunk column, in canonical order.
    fn chunk_block_hash(chunk: &ChunkColumn) -> u64 {
        let mut hasher = StableHasher::new();
        let (origin_x, origin_z) = chunk.pos().origin();
        for y in WORLD_MIN_Y..WORLD_MAX_Y {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let block = chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z));
                    hasher.write_u16(block.0);
                }
            }
        }
        hasher.finish()
    }

    /// Loaded chunk positions in canonical (cx, cz) order.
    fn chunk_set(host: &Host) -> Vec<(i32, i32)> {
        let mut loaded: Vec<(i32, i32)> = host.world().chunks().map(|(pos, _)| (pos.x, pos.z)).collect();
        loaded.sort_unstable();
        loaded
    }

    /// Per-chunk block hashes keyed by chunk coordinate.
    fn chunk_hashes(host: &Host) -> BTreeMap<(i32, i32), u64> {
        host.world()
            .chunks()
            .map(|(pos, chunk)| ((pos.x, pos.z), chunk_block_hash(chunk)))
            .collect()
    }

    /// Ignored throughput probe for batch chunk generation.
    ///
    /// Run explicitly in release with:
    /// `./dev-cargo test -p ob_server --release --lib -- --ignored --nocapture`
    #[test]
    #[ignore = "throughput measurement; run explicitly with --ignored --nocapture"]
    fn streaming_throughput_256_chunks() {
        const CHUNKS: usize = 256;

        let data_dir = TempDir::new("throughput");
        let registry = Arc::new(Registry::base());
        let config = HostConfig {
            seed: 0x5EED_1234_ABCD_0001,
            world_name: "throughput".to_string(),
            data_dir: Some(data_dir.path().to_path_buf()),
            render_distance_chunks: 16,
            generate_budget: 2,
        };
        let mut host = Host::open(registry, config).expect("host opens on a fresh world");

        let started = Instant::now();
        let generated = host.ensure_streaming(CHUNKS);
        let elapsed = started.elapsed();

        assert_eq!(
            generated, CHUNKS,
            "fresh world must have at least {CHUNKS} missing chunks"
        );
        let millis = elapsed.as_secs_f64() * 1000.0;
        let per_second = CHUNKS as f64 / elapsed.as_secs_f64();
        println!(
            "ensure_streaming({CHUNKS}) on a fresh world: {generated} chunks in {millis:.2} ms \
             ({per_second:.0} chunks/s)"
        );
    }

    /// Two independent hosts fed identical streaming calls must agree on the
    /// inserted chunk set, the block content of every chunk and the overall
    /// state hash. Every stored column is also compared against a direct serial
    /// `Generator::generate_chunk`, so batching cannot change the content.
    #[test]
    fn parallel_streaming_matches_across_hosts_and_serial_generator() {
        const SEED: u64 = 0x5EED_1234_ABCD_0001;

        let config = |data_dir: &Path| HostConfig {
            seed: SEED,
            world_name: "determinism".to_string(),
            data_dir: Some(data_dir.to_path_buf()),
            render_distance_chunks: 7,
            generate_budget: 2,
        };

        let registry = Arc::new(Registry::base());
        let dir_a = TempDir::new("determinism-a");
        let dir_b = TempDir::new("determinism-b");
        let mut host_a = Host::open(Arc::clone(&registry), config(dir_a.path())).expect("host A opens");
        let mut host_b = Host::open(Arc::clone(&registry), config(dir_b.path())).expect("host B opens");

        // Identical call sequence on both hosts. Budget 1 covers the
        // single-chunk batch; the larger budgets exercise multi-chunk batches.
        for budget in [1, 5, 12, 32, 9] {
            let generated_a = host_a.ensure_streaming(budget);
            let generated_b = host_b.ensure_streaming(budget);
            assert_eq!(
                generated_a, generated_b,
                "budget {budget}: hosts generated a different number of chunks"
            );
        }

        let chunks_a = chunk_set(&host_a);
        let chunks_b = chunk_set(&host_b);
        assert!(!chunks_a.is_empty(), "determinism sample loaded no chunks");
        assert_eq!(chunks_a, chunks_b, "hosts loaded different chunk sets");

        let hashes_a = chunk_hashes(&host_a);
        let hashes_b = chunk_hashes(&host_b);
        assert_eq!(hashes_a, hashes_b, "same chunk set has different block hashes");
        assert_eq!(
            host_a.state_hash(),
            host_b.state_hash(),
            "hosts disagree on the overall world state hash"
        );

        // The stored columns must equal serial generation for the same seed.
        let generator = Generator::new(SEED);
        for (pos, chunk) in host_a.world().chunks() {
            let serial = generator.generate_chunk(*pos, &registry);
            assert_eq!(
                chunk_block_hash(chunk),
                chunk_block_hash(&serial),
                "chunk ({}, {}) differs from serial generation",
                pos.x,
                pos.z
            );
        }
    }

    /// Host configuration used by the survival tests: a small streaming radius
    /// keeps world generation cheap and deterministic.
    fn test_config(data_dir: &Path, seed: u64) -> HostConfig {
        HostConfig {
            seed,
            world_name: "test".to_string(),
            data_dir: Some(data_dir.to_path_buf()),
            render_distance_chunks: 2,
            generate_budget: 16,
        }
    }

    /// World directory for [`test_config`].
    fn test_world_dir(dir: &TempDir) -> PathBuf {
        dir.path().join("worlds").join("test")
    }

    fn open_host(label: &str, seed: u64) -> (TempDir, Host) {
        let dir = TempDir::new(label);
        let host =
            Host::open(Arc::new(Registry::base()), test_config(dir.path(), seed)).expect("host opens on a fresh world");
        (dir, host)
    }

    fn survival_host(label: &str, seed: u64) -> (TempDir, Host) {
        let (dir, mut host) = open_host(label, seed);
        host.set_game_mode(GameMode::Survival);
        (dir, host)
    }

    /// Streams until every chunk in the configured radius is present.
    fn converge(host: &mut Host) {
        for _ in 0..200 {
            if host.ensure_streaming(64) == 0 {
                return;
            }
        }
        panic!("streaming did not converge");
    }

    fn break_action() -> BlockAction {
        BlockAction {
            break_block: true,
            place_block: false,
            place: BlockId::AIR,
        }
    }

    fn place_action(block: BlockId) -> BlockAction {
        BlockAction {
            break_block: false,
            place_block: true,
            place: block,
        }
    }

    /// A high, empty target column near spawn, used to test actions without
    /// terrain in the ray path.
    fn high_target(host: &Host) -> BlockPos {
        let spawn = host.player().pos;
        BlockPos::new(spawn.x.floor() as i32, 200, spawn.z.floor() as i32 - 2)
    }

    /// Teleports and aims the player at `target` so the camera ray hits it, then
    /// installs `block` there. The target must be in a loaded chunk.
    fn aim_at(host: &mut Host, target: BlockPos, block: BlockId) {
        for z in [target.z, target.z + 1, target.z + 2] {
            host.world_mut()
                .set_block(BlockPos::new(target.x, target.y, z), BlockId::AIR);
        }
        assert!(
            host.world_mut().set_block(target, block),
            "target ({}, {}, {}) is not in a loaded chunk",
            target.x,
            target.y,
            target.z
        );
        {
            let player = host.player_mut();
            player.pos = WorldVec::new(target.x as f64 + 0.5, target.y as f64 - 1.12, target.z as f64 + 2.5);
            player.velocity = WorldVec::ZERO;
            player.on_ground = false;
            player.yaw = 0.0;
            player.pitch = 0.0;
        }
        let hit = host
            .world()
            .raycast(host.player().eye(), host.player().forward(), REACH)
            .map(|hit| hit.block);
        assert_eq!(hit, Some(target), "test ray did not hit the target block");
    }

    #[test]
    fn game_mode_defaults_names_and_creative_immunity() {
        assert_eq!(GameMode::default(), GameMode::Creative);
        assert_eq!(GameMode::Survival.name(), "survival");
        assert_eq!(GameMode::Creative.name(), "creative");

        let (_dir, mut host) = open_host("game-mode", 3);
        assert_eq!(host.game_mode(), GameMode::Creative);
        host.damage(5.0);
        assert_eq!(host.player().health, MAX_HEALTH, "creative took damage");
        assert!(!host.is_dead());

        host.set_game_mode(GameMode::Survival);
        assert_eq!(host.game_mode(), GameMode::Survival);
        host.damage(5.0);
        assert_eq!(host.player().health, MAX_HEALTH - 5.0);
        host.damage(100.0);
        assert!(host.is_dead());
        assert_eq!(host.player().health, 0.0);

        host.set_game_mode(GameMode::Creative);
        assert_eq!(host.game_mode(), GameMode::Creative);
        assert!(!host.is_dead(), "switching to creative must revive");
        assert_eq!(host.player().health, MAX_HEALTH);
        assert_eq!(host.player().hunger, MAX_HUNGER);

        let apple = host.registry().item_id("openblock:apple").unwrap();
        host.give_item(apple, 1);
        host.player_mut().hunger = 10.0;
        assert!(!host.eat_selected(), "creative must not eat items");
        assert_eq!(host.selected_stack(), Some(ItemStack::new(apple, 1)));
    }

    #[test]
    fn survival_mining_uses_drop_table_and_rejects_unbreakables() {
        let (_dir, mut host) = survival_host("mining", 5);
        let stone = host.registry().id("openblock:stone").unwrap();
        let grass = host.registry().id("openblock:grass").unwrap();
        let bedrock = host.registry().id("openblock:bedrock").unwrap();
        let stone_item = host.registry().item_id("openblock:stone").unwrap();
        let dirt_item = host.registry().item_id("openblock:dirt").unwrap();
        let target = high_target(&host);

        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.broke, Some(stone));
        assert_eq!(outcome.placed, None);
        assert_eq!(outcome.drops, vec![ItemStack::new(stone_item, 1)]);
        assert!(outcome.pickup_overflow.is_empty());
        assert_eq!(host.world().get_block(target), BlockId::AIR);
        assert_eq!(host.inventory()[0], Some(ItemStack::new(stone_item, 1)));

        aim_at(&mut host, target, grass);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.broke, Some(grass));
        assert_eq!(outcome.drops, vec![ItemStack::new(dirt_item, 1)]);
        assert_eq!(host.inventory()[1], Some(ItemStack::new(dirt_item, 1)));

        aim_at(&mut host, target, bedrock);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(!outcome.changed, "bedrock must not break in survival");
        assert_eq!(outcome.broke, None);
        assert!(outcome.drops.is_empty());
        assert_eq!(host.world().get_block(target), bedrock);
    }

    #[test]
    fn survival_placement_consumes_and_rejects_empty_or_mismatched_slots() {
        let (_dir, mut host) = survival_host("placing", 7);
        let stone = host.registry().id("openblock:stone").unwrap();
        let stone_item = host.registry().item_id("openblock:stone").unwrap();
        let dirt_item = host.registry().item_id("openblock:dirt").unwrap();
        let target = high_target(&host);
        let placed_pos = BlockPos::new(target.x, target.y, target.z + 1);

        assert_eq!(host.give_item(stone_item, 3), 0);
        assert_eq!(host.give_item(dirt_item, 1), 0);
        host.set_hotbar_selection(0);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(stone));
        assert!(outcome.changed);
        assert_eq!(outcome.placed, Some(stone));
        assert_eq!(outcome.broke, None);
        assert!(outcome.drops.is_empty());
        assert_eq!(host.selected_stack(), Some(ItemStack::new(stone_item, 2)));
        assert_eq!(host.world().get_block(placed_pos), stone);

        // A mismatched selected item cannot place the requested block.
        host.set_hotbar_selection(1);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(stone));
        assert!(!outcome.changed);
        assert_eq!(outcome.placed, None);
        assert_eq!(host.selected_stack(), Some(ItemStack::new(dirt_item, 1)));
        assert_eq!(host.world().get_block(placed_pos), BlockId::AIR);

        // An empty selected slot cannot place either.
        host.set_hotbar_selection(8);
        aim_at(&mut host, target, stone);
        assert_eq!(host.selected_stack(), None);
        let outcome = host.apply_action_with_outcome(&place_action(stone));
        assert!(!outcome.changed);
        assert_eq!(host.world().get_block(placed_pos), BlockId::AIR);
    }

    #[test]
    fn creative_actions_do_not_use_or_grant_items() {
        let (_dir, mut host) = open_host("creative", 11);
        assert_eq!(host.game_mode(), GameMode::Creative);
        let stone = host.registry().id("openblock:stone").unwrap();
        let bedrock = host.registry().id("openblock:bedrock").unwrap();
        let target = high_target(&host);

        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert!(outcome.drops.is_empty());
        assert!(outcome.pickup_overflow.is_empty());
        assert_eq!(host.world().get_block(target), BlockId::AIR);
        assert!(host.inventory().iter().all(Option::is_none));

        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(stone));
        assert!(outcome.changed, "creative placement must ignore the inventory");
        assert!(outcome.drops.is_empty());
        assert_eq!(
            host.world().get_block(BlockPos::new(target.x, target.y, target.z + 1)),
            stone
        );
        assert!(host.inventory().iter().all(Option::is_none));

        // Creative keeps the historical instant break, even for bedrock.
        aim_at(&mut host, target, bedrock);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(host.world().get_block(target), BlockId::AIR);
    }

    #[test]
    fn full_inventory_reports_overflow_and_loses_pickups() {
        let (_dir, mut host) = survival_host("full", 13);
        let stone = host.registry().id("openblock:stone").unwrap();
        let stone_item = host.registry().item_id("openblock:stone").unwrap();
        let target = high_target(&host);

        assert_eq!(host.give_item(stone_item, 255), 0);
        assert_eq!(host.give_item(stone_item, 255), 0);
        assert_eq!(host.give_item(stone_item, 66), 0);
        assert!(
            host.inventory()
                .iter()
                .all(|slot| matches!(slot, Some(stack) if stack.count == 64))
        );
        assert_eq!(host.inventory().len(), HOTBAR_SLOTS);
        assert_eq!(
            host.give_item(stone_item, 1),
            1,
            "a full inventory must report overflow"
        );

        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.drops, vec![ItemStack::new(stone_item, 1)]);
        assert_eq!(outcome.pickup_overflow, vec![ItemStack::new(stone_item, 1)]);
        assert!(
            host.inventory()
                .iter()
                .all(|slot| matches!(slot, Some(stack) if stack.count == 64))
        );
    }

    #[test]
    fn survival_fall_damage_and_respawn() {
        let (_dir, mut host) = survival_host("fall", 17);
        let spawn = host.player().pos;

        // An eight metre fall is beyond the safe impact speed.
        host.player_mut().pos = WorldVec::new(spawn.x, spawn.y + 8.0, spawn.z);
        host.player_mut().on_ground = false;
        host.player_mut().velocity = WorldVec::ZERO;
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
            if host.player().on_ground {
                break;
            }
        }
        assert!(host.player().on_ground, "player did not land");
        assert!(
            host.player().health < MAX_HEALTH,
            "fall did not reduce health (health {})",
            host.player().health
        );
        assert!(!host.is_dead());

        // A lethal fall eventually kills and can be respawned.
        {
            let player = host.player_mut();
            player.health = MAX_HEALTH;
            player.hunger = MAX_HUNGER;
            player.dead = false;
            player.velocity = WorldVec::ZERO;
            player.on_ground = false;
            player.pos = WorldVec::new(spawn.x, spawn.y + 80.0, spawn.z);
        }
        for _ in 0..400 {
            host.tick(&PlayerInput::default());
            if host.is_dead() {
                break;
            }
        }
        assert!(host.is_dead(), "lethal fall did not kill the player");
        assert_eq!(host.player().health, 0.0);

        let stone_item = host.registry().item_id("openblock:stone").unwrap();
        assert_eq!(host.give_item(stone_item, 5), 0);
        host.respawn();
        assert!(!host.is_dead());
        assert_eq!(host.player().health, MAX_HEALTH);
        assert_eq!(host.player().hunger, MAX_HUNGER);
        assert_eq!(host.player().pos, spawn, "respawn must teleport to the spawn point");
        assert_eq!(host.player().velocity, WorldVec::ZERO);
        assert_eq!(
            host.selected_stack(),
            Some(ItemStack::new(stone_item, 5)),
            "respawn keeps the inventory"
        );
    }

    #[test]
    fn survival_starvation_and_regeneration_cadence() {
        let (_dir, mut host) = survival_host("vitals", 19);
        // Settle onto the ground before measuring timers.
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
            if host.player().on_ground {
                break;
            }
        }
        assert!(host.player().on_ground);

        {
            let player = host.player_mut();
            player.health = MAX_HEALTH;
            player.hunger = 0.0;
            player.starvation_timer = 0.0;
            player.regen_timer = 0.0;
            player.dead = false;
        }
        for _ in 0..80 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.player().health, 19.0, "starvation cadence should be 4 s");
        assert!(!host.is_dead());

        {
            let player = host.player_mut();
            player.health = 10.0;
            player.hunger = MAX_HUNGER;
            player.starvation_timer = 0.0;
            player.regen_timer = 0.0;
        }
        for _ in 0..80 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.player().health, 11.0, "regeneration cadence should be 4 s");

        // Below the regeneration threshold no health is restored.
        {
            let player = host.player_mut();
            player.health = 10.0;
            player.hunger = 17.9;
            player.starvation_timer = 0.0;
            player.regen_timer = 0.0;
        }
        for _ in 0..160 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.player().health, 10.0, "regenerated below the hunger threshold");
    }

    #[test]
    fn eating_selected_apple_restores_hunger_and_consumes_one() {
        let (_dir, mut host) = survival_host("eating", 23);
        let apple = host.registry().item_id("openblock:apple").unwrap();
        let stone = host.registry().item_id("openblock:stone").unwrap();

        assert_eq!(host.give_item(apple, 2), 0);
        host.player_mut().hunger = 10.0;
        assert!(host.eat_selected());
        assert_eq!(host.player().hunger, 16.0);
        assert_eq!(host.selected_stack(), Some(ItemStack::new(apple, 1)));
        assert!(host.eat_selected());
        assert_eq!(host.player().hunger, MAX_HUNGER);
        assert_eq!(host.selected_stack(), None, "the last apple must clear the slot");
        assert!(!host.eat_selected(), "a full player must not consume food");

        assert_eq!(host.give_item(stone, 1), 0);
        host.player_mut().hunger = 10.0;
        assert!(!host.eat_selected(), "non-food items must not be edible");
        assert_eq!(host.selected_stack(), Some(ItemStack::new(stone, 1)));

        host.player_mut().dead = true;
        assert!(!host.eat_selected(), "dead players must not eat");
        assert_eq!(host.selected_stack(), Some(ItemStack::new(stone, 1)));
    }

    #[test]
    fn save_reload_preserves_vitals_inventory_and_state_hash() {
        let dir = TempDir::new("persistence");
        let config = test_config(dir.path(), 29);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        host.set_game_mode(GameMode::Survival);

        let stone = host.registry().item_id("openblock:stone").unwrap();
        let apple = host.registry().item_id("openblock:apple").unwrap();
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        assert_eq!(host.give_item(stone, 10), 0);
        assert_eq!(host.give_item(apple, 2), 0);
        assert_eq!(host.give_item(dirt, 64), 0);
        host.set_hotbar_selection(4);
        {
            let player = host.player_mut();
            player.health = 13.25;
            player.hunger = 7.5;
            player.dead = false;
        }
        converge(&mut host);
        let hash_before = host.state_hash();
        let inventory_before = *host.inventory();
        let health_before = host.player().health;
        let hunger_before = host.player().hunger;
        let position_before = host.player().pos;
        host.save().expect("save succeeds");

        let player_text = std::fs::read_to_string(test_world_dir(&dir).join("player.txt")).expect("player.txt");
        assert_eq!(
            player_text.split_whitespace().count(),
            10,
            "player.txt must use the 10-field format"
        );
        let inventory_text =
            std::fs::read_to_string(test_world_dir(&dir).join("inventory.txt")).expect("inventory.txt");
        assert_eq!(inventory_text.lines().count(), 3);
        drop(host);

        let reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        assert_eq!(reopened.game_mode(), GameMode::Creative, "game mode is session state");
        assert_eq!(reopened.player().health, health_before);
        assert_eq!(reopened.player().hunger, hunger_before);
        assert_eq!(reopened.player().pos, position_before);
        assert_eq!(*reopened.inventory(), inventory_before);
        assert_eq!(reopened.hotbar_selection(), 0, "hotbar selection is not persisted");
        assert_eq!(
            reopened.state_hash(),
            hash_before,
            "state hash changed across save/load"
        );
    }

    #[test]
    fn old_player_file_and_missing_inventory_load_defaults() {
        let dir = TempDir::new("old-save");
        let config = test_config(dir.path(), 31);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        converge(&mut host);
        host.save().expect("save succeeds");

        let world_dir = test_world_dir(&dir);
        std::fs::write(world_dir.join("player.txt"), "12.5 90.0 -3.25 0.25 -0.5 0 1234\n")
            .expect("write old player file");
        std::fs::remove_file(world_dir.join("inventory.txt")).expect("remove inventory file");

        let reopened = Host::open(Arc::clone(&registry), config).expect("old save still loads");
        assert_eq!(reopened.player().pos, WorldVec::new(12.5, 90.0, -3.25));
        assert_eq!(reopened.player().yaw, 0.25);
        assert_eq!(reopened.player().pitch, -0.5);
        assert!(!reopened.player().flying);
        assert_eq!(reopened.tick_count(), 1234);
        assert_eq!(reopened.player().health, MAX_HEALTH);
        assert_eq!(reopened.player().hunger, MAX_HUNGER);
        assert!(!reopened.is_dead());
        assert!(reopened.inventory().iter().all(Option::is_none));
    }

    #[test]
    fn inventory_loader_drops_unknown_items_and_clamps_values() {
        let dir = TempDir::new("inventory-load");
        let config = test_config(dir.path(), 37);
        let registry = Arc::new(Registry::base());
        let host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        drop(host);

        std::fs::write(
            test_world_dir(&dir).join("inventory.txt"),
            "0 openblock:stone 200\n1 openblock:not_an_item 5\n2 openblock:apple 0\n12 openblock:dirt 3\n",
        )
        .expect("write inventory");

        let reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        let stone = reopened.registry().item_id("openblock:stone").unwrap();
        let apple = reopened.registry().item_id("openblock:apple").unwrap();
        let dirt = reopened.registry().item_id("openblock:dirt").unwrap();
        assert_eq!(reopened.inventory()[0], Some(ItemStack::new(stone, 64)));
        assert_eq!(reopened.inventory()[1], None, "unknown item must be dropped");
        assert_eq!(reopened.inventory()[2], Some(ItemStack::new(apple, 1)));
        assert_eq!(reopened.inventory()[8], Some(ItemStack::new(dirt, 3)));
    }

    #[test]
    fn leaf_apple_drops_are_deterministic() {
        const SEED: u64 = 0xA11CE;
        let (_dir_a, mut host_a) = survival_host("apple-a", SEED);
        let leaves = host_a.registry().id("openblock:leaves").unwrap();
        let apple = host_a.registry().item_id("openblock:apple").unwrap();
        let base = high_target(&host_a);

        // The pure roll is close to one in six over a large deterministic sample.
        let hits = (0..1_000)
            .filter(|y| apple_drop(SEED, BlockPos::new(base.x, *y, base.z)))
            .count();
        assert!(
            (100..=250).contains(&hits),
            "apple hash produced {hits} drops in 1000, expected about 166"
        );

        let mut apple_pos = None;
        let mut plain_pos = None;
        for y in 150..250 {
            let pos = BlockPos::new(base.x, y, base.z);
            if apple_drop(SEED, pos) {
                apple_pos.get_or_insert(pos);
            } else {
                plain_pos.get_or_insert(pos);
            }
            if apple_pos.is_some() && plain_pos.is_some() {
                break;
            }
        }
        let apple_pos = apple_pos.expect("sample contains an apple-dropping leaf");
        let plain_pos = plain_pos.expect("sample contains a non-dropping leaf");

        aim_at(&mut host_a, apple_pos, leaves);
        let apple_outcome = host_a.apply_action_with_outcome(&break_action());
        assert!(apple_outcome.changed);
        assert_eq!(apple_outcome.broke, Some(leaves));
        assert_eq!(apple_outcome.drops, vec![ItemStack::new(apple, 1)]);
        assert_eq!(host_a.inventory()[0], Some(ItemStack::new(apple, 1)));

        aim_at(&mut host_a, plain_pos, leaves);
        let plain_outcome = host_a.apply_action_with_outcome(&break_action());
        assert!(plain_outcome.changed);
        assert!(plain_outcome.drops.is_empty(), "leaves have no guaranteed drop");

        // A second host with the same seed agrees for the same block positions.
        let (_dir_b, mut host_b) = survival_host("apple-b", SEED);
        assert_eq!(host_b.world().seed(), SEED);
        aim_at(&mut host_b, apple_pos, leaves);
        assert_eq!(
            host_b.apply_action_with_outcome(&break_action()).drops,
            apple_outcome.drops
        );
        aim_at(&mut host_b, plain_pos, leaves);
        assert_eq!(
            host_b.apply_action_with_outcome(&break_action()).drops,
            plain_outcome.drops
        );
    }
}

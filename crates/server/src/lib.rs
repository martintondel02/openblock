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

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ob_content::{ArmorSlot, BlockId, ItemId, ItemType, RecipeId, Registry};
use ob_math::{Aabb, BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_sim::{
    BlockAction, MAX_HEALTH, MAX_HUNGER, Player, PlayerInput, REACH, TICK_DT, advance_vitals, apply_block_actions,
    apply_damage, eat, fall_damage, reset_vitals, spawn_position, step_player,
};
use ob_world::{ChunkColumn, World};
use ob_worldgen::Generator;
use rayon::prelude::*;

mod block_entities;
mod items;
mod mobs;
pub mod net;
pub mod net_players;

pub use block_entities::{CHEST_SLOTS, ContainerState, FurnaceSlot, FurnaceState};
pub use items::ItemEntity;
pub use mobs::{
    AttackOutcome, DAY_LENGTH_TICKS, HOSTILE_SPAWN_MAX_DISTANCE, HOSTILE_SPAWN_MIN_DISTANCE, MAX_HOSTILE_MOBS,
    MAX_PASSIVE_MOBS, MOB_ATTACK_REACH, MOB_BURN_DAMAGE_PER_SECOND, MOB_SPAWN_ATTEMPTS_PER_CATEGORY,
    MOB_SPAWN_INTERVAL_TICKS, MOB_SPAWN_MIN_DISTANCE, MobEntity, NIGHT_END_TICK, NIGHT_START_TICK,
    PASSIVE_SPAWN_MAX_DISTANCE, PASSIVE_SPAWN_MIN_DISTANCE,
};
pub use net::{ConfigError, DedicatedServer, NetConfig, ServerFileConfig, load_server_config, parse_server_config};
pub use net_players::{
    ENTITY_INTEREST_RADIUS, NETWORK_PLAYERS_FILE, NetworkDamageEvent, NetworkMoveResult, NetworkWindowRef, ServerPlayer,
};

/// Number of slots in the player's hotbar.
pub const HOTBAR_SLOTS: usize = 9;
/// Number of armor slots (helmet, chestplate, leggings, boots).
pub const ARMOR_SLOTS: usize = 4;
/// Armor points beyond this cap give no further damage reduction.
pub const ARMOR_DEFENSE_CAP: u8 = 20;
/// Damage reduction per armor point (4%, so the cap is 80%).
pub const ARMOR_REDUCTION_PER_POINT: f32 = 0.04;
/// World-directory filename for the equipped armor sidecar.
const EQUIPMENT_FILE: &str = "equipment.txt";
/// World-directory filename for the persisted world-spawn point.
const SPAWN_FILE: &str = "spawn.txt";

/// Defensive upper bound on one inventory stack.
///
/// The actual limit is [`ob_content::ItemType::max_stack`] (64 for blocks and
/// sticks, 16 for apples, 1 for tools); this constant is the hard ceiling the
/// server accepts so a malformed pack cannot request an unbounded stack.
pub const MAX_STACK_SIZE: u8 = ob_content::MAX_ITEM_STACK;
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

/// Canonical array index of an [`ArmorSlot`] (`Helmet` 0, `Chestplate` 1,
/// `Leggings` 2, `Boots` 3).
///
/// This is the index into [`Host::equipment`] and into the `equipment.txt`
/// sidecar.
pub const fn armor_slot_index(slot: ArmorSlot) -> usize {
    match slot {
        ArmorSlot::Helmet => 0,
        ArmorSlot::Chestplate => 1,
        ArmorSlot::Leggings => 2,
        ArmorSlot::Boots => 3,
    }
}

/// Fraction of incoming damage absorbed by `defense` armor points.
///
/// Each point absorbs 4% up to [`ARMOR_DEFENSE_CAP`] points (80% total). The
/// remaining fraction is always at least 20%, so armor never grants immunity.
pub fn armor_damage_reduction(defense: u8) -> f32 {
    f32::from(defense.min(ARMOR_DEFENSE_CAP)) * ARMOR_REDUCTION_PER_POINT
}

/// One inventory stack. `count` is `1..=`[`MAX_STACK_SIZE`]; an empty slot is
/// represented by `None` in the inventory rather than by a zero count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    /// Item type resource id (session-local index).
    pub item: ItemId,
    /// Number of items in the stack.
    pub count: u8,
    /// Remaining durability for tools; `None` for every non-tool stack.
    ///
    /// New tools start at their [`ob_content::ToolSpec::durability`]; a stack at
    /// zero is removed from the inventory and reported through
    /// [`ActionOutcome::tool_broke`].
    pub durability: Option<u16>,
}

impl ItemStack {
    /// Creates a content-agnostic stack, clamping the count to
    /// [`MAX_STACK_SIZE`].
    ///
    /// This constructor cannot initialise tool durability because it does not
    /// see the registry; use [`ItemStack::for_item`] (or the host's inventory
    /// APIs) when the item type is known.
    pub const fn new(item: ItemId, count: u8) -> Self {
        let count = if count > MAX_STACK_SIZE { MAX_STACK_SIZE } else { count };
        Self {
            item,
            count,
            durability: None,
        }
    }

    /// Creates a stack from a content item type and its session-local id.
    ///
    /// Clamps the count to the item's `max_stack` (and [`MAX_STACK_SIZE`]) and
    /// initialises durability to the tool's or armor piece's full durability.
    pub fn for_item(item: ItemId, item_type: &ItemType, count: u8) -> Self {
        let max = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let count = count.clamp(1, max);
        Self {
            item,
            count,
            durability: item_type
                .tool
                .map(|spec| spec.durability)
                .or_else(|| item_type.armor.map(|spec| spec.durability)),
        }
    }

    /// Sets the durability component explicitly (load and tests).
    ///
    /// Accepts both `Some(remaining)` and a bare `remaining` value.
    pub fn with_durability(mut self, durability: impl Into<Option<u16>>) -> Self {
        self.durability = durability.into();
        self
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
    ///
    /// Block breaks now spawn [`ItemEntity`]s instead of granting loot
    /// directly, so this is reserved for pickup-overflow diagnostics.
    pub pickup_overflow: Vec<ItemStack>,
    /// The tool whose last durability was consumed and which was therefore
    /// removed from the selected hotbar slot, if any.
    pub tool_broke: Option<ItemId>,
}

/// Result of one crafting attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CraftOutcome {
    /// Whether the output entered the inventory and ingredients were consumed.
    pub crafted: bool,
    /// The produced stack on success.
    pub output: Option<ItemStack>,
    /// A short ASCII hint for the HUD (`crafted 4 planks`, `inventory full`,
    /// `needs a crafting table`, `need 2 more planks`).
    pub message: String,
}

/// Result of applying one damaging hit after armor mitigation.
///
/// Returned by [`Host::damage_with_outcome`]; [`Host::damage`] discards it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DamageOutcome {
    /// Incoming damage before armor.
    pub raw: f32,
    /// Damage passed to the health pool after armor mitigation.
    pub mitigated: f32,
    /// Health actually removed (clamped by the remaining health).
    pub dealt: f32,
    /// Health loss prevented by armor (`raw - mitigated`).
    pub absorbed: f32,
    /// Armor pieces that reached zero durability on this hit, in slot order.
    pub armor_broke: Vec<ArmorSlot>,
}

impl DamageOutcome {
    /// Whether the hit changed the player or the equipment.
    pub fn changed(&self) -> bool {
        self.dealt > 0.0 || !self.armor_broke.is_empty()
    }
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
    items: Vec<ItemEntity>,
    next_item_id: u64,
    mobs: Vec<MobEntity>,
    next_mob_id: u64,
    pickup_events: Vec<ItemStack>,
    furnaces: BTreeMap<BlockPos, FurnaceState>,
    chests: BTreeMap<BlockPos, ContainerState>,
    equipment: [Option<ItemStack>; ARMOR_SLOTS],
    /// Connected network players keyed by connection id.
    network_players: BTreeMap<u64, ServerPlayer>,
    /// Offline network-player records keyed by lower-case name.
    saved_network_players: BTreeMap<String, ServerPlayer>,
    /// Damage events for the net layer to relay (runtime only).
    network_damage_events: Vec<net_players::NetworkDamageEvent>,
    /// Entity removals for the net layer to broadcast (runtime only).
    network_entity_removals: Vec<net_players::RemovedEntity>,
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
            let bytes = ob_world::read_save_file(&world_path)?;
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
                items: Vec::new(),
                next_item_id: 1,
                mobs: Vec::new(),
                next_mob_id: 1,
                pickup_events: Vec::new(),
                furnaces: BTreeMap::new(),
                chests: BTreeMap::new(),
                equipment: [None; ARMOR_SLOTS],
                network_players: BTreeMap::new(),
                saved_network_players: BTreeMap::new(),
                network_damage_events: Vec::new(),
                network_entity_removals: Vec::new(),
            };
            host.inventory = load_inventory(&host.world_dir, &host.registry);
            host.items = items::load_items(&host.world_dir, &host.registry);
            host.next_item_id = items::next_item_id(&host.items);
            host.mobs = mobs::load_mobs(&host.world_dir, &host.registry);
            host.next_mob_id = mobs::next_mob_id(&host.mobs);
            host.ensure_streaming(2);
            // Prefer the spawn stored at world creation. Older worlds compute
            // it now, then persist it on the next save; regenerating from the
            // current loaded surface after a restart would otherwise move the
            // spawn when the original candidate column was not streamed yet.
            host.spawn = match load_spawn(&host.world_dir) {
                Some(spawn) => spawn,
                None => host.resolve_spawn(),
            };
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
                items: Vec::new(),
                next_item_id: 1,
                mobs: Vec::new(),
                next_mob_id: 1,
                pickup_events: Vec::new(),
                furnaces: BTreeMap::new(),
                chests: BTreeMap::new(),
                equipment: [None; ARMOR_SLOTS],
                network_players: BTreeMap::new(),
                saved_network_players: BTreeMap::new(),
                network_damage_events: Vec::new(),
                network_entity_removals: Vec::new(),
            };
            host.ensure_streaming(3);
            // Generate the chosen candidate column before reading its surface,
            // so a tree, boulder or cave entrance in that column is accounted
            // for and the spawn is safe from the first session onward.
            let spawn = host.resolve_spawn();
            host.spawn = spawn;
            host.player = Player::new(spawn);
            host.save()?;
        }
        host.ensure_streaming(2);
        host.equipment = load_equipment(&host.world_dir, &host.registry);
        let (furnaces, chests) = block_entities::load_block_entities(&host.world_dir, &host.registry, &host.world);
        host.furnaces = furnaces;
        host.chests = chests;
        host.saved_network_players = net_players::load_network_players(&host.world_dir, &host.registry);
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

    /// The spawn position found when the host opened.
    ///
    /// Network clients start here; it is independent of the host's internal
    /// single-player [`Player`] and does not move when that player does. The
    /// point is persisted in `spawn.txt` so a restart and every respawn use the
    /// same safe column even when the original candidate was outside the
    /// initially streamed chunks.
    pub fn spawn(&self) -> WorldVec {
        self.spawn
    }

    /// Resolves and generates a safe initial spawn column.
    ///
    /// The search follows the same deterministic ring/angle order as the old
    /// free function, but generates each candidate column before reading its
    /// surface. A candidate is accepted only when its terrain-height block is
    /// solid, nothing (tree, boulder, snow drift, overhang) sits above it, and
    /// the two feet/head cells are clear. This prevents a fresh world from
    /// starting inside a tree and keeps the first save and every later reopen
    /// on the same ground.
    fn resolve_spawn(&mut self) -> WorldVec {
        for radius in 0..64i32 {
            for angle_index in 0..16 {
                let angle = angle_index as f64 * std::f64::consts::TAU / 16.0;
                let x = (angle.cos() * radius as f64 * 8.0) as i32;
                let z = (angle.sin() * radius as f64 * 8.0) as i32;
                let terrain_height = self.generator.height_at(x, z);
                if terrain_height <= SEA_LEVEL + 2 {
                    continue;
                }
                // Generate the candidate column so features and carved cave
                // entrances are visible to the safety checks below.
                self.ensure_streaming_around(
                    WorldVec::new(x as f64 + 0.5, f64::from(terrain_height) + 1.0, z as f64 + 0.5),
                    0,
                    1,
                );
                if self.spawn_column_is_safe(x, terrain_height, z) {
                    return spawn_position(&self.world, x, z);
                }
            }
        }
        WorldVec::new(0.5, self.generator.height_at(0, 0) as f64 + 1.0, 0.5)
    }

    /// Whether the generated column at `terrain_height` is dry, solid and has
    /// two clear cells for the player's feet and head.
    fn spawn_column_is_safe(&self, x: i32, terrain_height: i32, z: i32) -> bool {
        let surface = BlockPos::new(x, terrain_height, z);
        if self.world.chunk(surface.chunk()).is_none() {
            return false;
        }
        if !self.world.is_solid(surface) {
            return false;
        }
        // Any feature above the terrain surface (tree trunk/canopy, boulder,
        // snow drift) raises the heightmap and disqualifies the column.
        if self.world.height_at(x, z) != Some(terrain_height) {
            return false;
        }
        let feet = BlockPos::new(x, terrain_height + 1, z);
        let head = BlockPos::new(x, terrain_height + 2, z);
        self.world.get_block(feet) == BlockId::AIR && self.world.get_block(head) == BlockId::AIR
    }

    /// One loaded chunk column, if it is currently resident.
    ///
    /// Unloaded chunks read as absent; call
    /// [`Host::ensure_streaming_around`] to generate a missing column first.
    pub fn chunk_column(&self, pos: ChunkPos) -> Option<&ChunkColumn> {
        self.world.chunk(pos)
    }

    /// A detached copy of one loaded chunk column, if resident.
    pub fn chunk_snapshot(&self, pos: ChunkPos) -> Option<ChunkColumn> {
        self.world.chunk(pos).cloned()
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
    /// Clamped to [`ob_core::config::MIN_RENDER_DISTANCE`]..=[`ob_core::config::MAX_RENDER_DISTANCE`]
    /// (2..=32). Newly needed columns are generated by the next
    /// [`Host::ensure_streaming`] call; columns beyond the new radius are unloaded
    /// then too. Returns whether the radius changed.
    pub fn set_render_distance_chunks(&mut self, chunks: i32) -> bool {
        let chunks = chunks.clamp(
            ob_core::config::MIN_RENDER_DISTANCE,
            ob_core::config::MAX_RENDER_DISTANCE,
        );
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

    /// The stack in hotbar slot `slot`, or `None` when the slot is empty or
    /// out of range.
    ///
    /// Unlike [`Host::inventory`] the returned reference can be used without
    /// indexing; it is read-only, so callers that need to change the slot use
    /// [`Host::take_inventory_slot`] or [`Host::set_inventory_slot`].
    pub fn inventory_slot(&self, slot: usize) -> Option<&ItemStack> {
        self.inventory.get(slot)?.as_ref().filter(|stack| stack.count > 0)
    }

    /// Removes and returns the exact stack in hotbar slot `slot`.
    ///
    /// The stack keeps its count and remaining tool/armor durability. This is
    /// the mode-independent counterpart of [`Host::consume_selected`], which
    /// only works for the selected slot in survival. Returns `None` for an
    /// empty slot or an index outside `0..`[`HOTBAR_SLOTS`].
    pub fn take_inventory_slot(&mut self, slot: usize) -> Option<ItemStack> {
        let taken = self.inventory.get_mut(slot)?.take();
        if taken.is_some() {
            self.mark_player_dirty();
        }
        taken
    }

    /// Places `stack` into hotbar slot `slot` exactly, without merging.
    ///
    /// * `None` or a zero-count stack clears the slot.
    /// * A known item id has its count clamped to the content
    ///   [`max_stack`](ItemType::max_stack) (capped at [`MAX_STACK_SIZE`]) and
    ///   tool/armor durability clamped to `1..=spec.durability` (a missing
    ///   component means full durability); other items carry no durability.
    /// * An unknown item id or an out-of-range slot leaves the inventory
    ///   unchanged.
    ///
    /// Returns the previous stack that was displaced from the slot (`None`
    /// when it was empty). Because invalid requests are no-ops, an unknown item
    /// id also returns the unchanged previous stack.
    pub fn set_inventory_slot(&mut self, slot: usize, stack: Option<ItemStack>) -> Option<ItemStack> {
        if slot >= HOTBAR_SLOTS {
            return None;
        }
        let previous = self.inventory[slot];
        let next = match stack {
            Some(stack) if stack.count > 0 => match sanitize_stack(&self.registry, stack) {
                Some(stack) => Some(stack),
                // Unknown item id: reject the write, keep the slot intact.
                None => return previous,
            },
            _ => None,
        };
        if next != previous {
            self.inventory[slot] = next;
            self.mark_player_dirty();
        }
        previous
    }

    /// Effective seconds to mine `block` with the selected hotbar stack.
    ///
    /// Uses the content tool rules: hand and wrong/under-tier tools take the
    /// block's raw hardness, a matching tool divides it by the tool speed.
    /// Unbreakable blocks return `0.0`. The client should use this for its
    /// hold-to-mine timer so its prediction matches host validation.
    pub fn mining_seconds(&self, block: BlockId) -> f64 {
        let held = self.selected_stack().map(|stack| stack.item);
        f64::from(self.registry.effective_mining_seconds(block, held))
    }

    /// Whether a `openblock:crafting_table` block is within four blocks of the
    /// player's eye position (Chebyshev distance).
    ///
    /// Unloaded chunks read as air, so a table in a streamed-out chunk does not
    /// count until the chunk is loaded again.
    pub fn near_crafting_table(&self) -> bool {
        let table = self.registry.id_or_air("openblock:crafting_table");
        if table == BlockId::AIR {
            return false;
        }
        let eye = self.player.eye();
        let centre = BlockPos::new(eye.x.floor() as i32, eye.y.floor() as i32, eye.z.floor() as i32);
        for dy in -4..=4 {
            for dz in -4..=4 {
                for dx in -4..=4 {
                    let pos = BlockPos::new(centre.x + dx, centre.y + dy, centre.z + dz);
                    if self.world.get_block(pos) == table {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// How many times each recipe can currently be crafted, in deterministic
    /// recipe-id order.
    ///
    /// Survival counts are limited by the ingredient totals in the nine hotbar
    /// slots; recipes that need a table report `0` until
    /// [`Host::near_crafting_table`] is true. Counts are capped at
    /// [`u8::MAX`]. Creative bypasses ingredients and table placement and
    /// reports `1` for every recipe (a one-at-a-time availability flag), because
    /// "unlimited" has no useful numeric representation for a HUD.
    ///
    /// Output-inventory space is deliberately not part of this count:
    /// [`Host::craft`] performs that check and returns `inventory full` without
    /// consuming anything.
    pub fn available_recipes(&self) -> Vec<(RecipeId, u8)> {
        if self.game_mode == GameMode::Creative {
            return self.registry.recipes().map(|(id, _)| (id, 1)).collect();
        }
        let near_table = self.near_crafting_table();
        let mut counts = Vec::with_capacity(self.registry.recipe_len());
        for (recipe_id, recipe) in self.registry.recipes() {
            if recipe.needs_table && !near_table {
                counts.push((recipe_id, 0));
                continue;
            }
            let mut crafts = u16::from(u8::MAX);
            for (item, needed) in recipe.ingredients {
                let have: u32 = self
                    .inventory
                    .iter()
                    .flatten()
                    .filter(|stack| stack.item == *item)
                    .map(|stack| u32::from(stack.count))
                    .sum();
                crafts = crafts.min((have / u32::from((*needed).max(1))) as u16);
            }
            counts.push((recipe_id, crafts.min(u16::from(u8::MAX)) as u8));
        }
        counts
    }

    /// Crafts one output of `recipe`.
    ///
    /// Survival requires every ingredient in the nine hotbar slots and, for
    /// table recipes, a crafting table within four blocks ([`Host::craft`] uses
    /// [`Host::near_crafting_table`]). Output space is checked against the
    /// post-consumption inventory before anything is committed, so a genuine
    /// lack of space returns `inventory full` and leaves the ingredients
    /// untouched while a hotbar whose ingredient stack frees up still succeeds.
    /// Creative bypasses ingredient and table requirements but still needs one
    /// free output slot.
    ///
    /// Shaped recipes are simplified to ingredient totals in this slice; the base
    /// recipe table is defined in [`Registry::base`].
    pub fn craft(&mut self, recipe: RecipeId) -> CraftOutcome {
        let Some(recipe) = self.registry.recipe(recipe).copied() else {
            return CraftOutcome {
                crafted: false,
                output: None,
                message: "unknown recipe".to_string(),
            };
        };
        if self.game_mode == GameMode::Survival && recipe.needs_table && !self.near_crafting_table() {
            return CraftOutcome {
                crafted: false,
                output: None,
                message: "needs a crafting table".to_string(),
            };
        }

        if self.game_mode == GameMode::Survival {
            for (item, needed) in recipe.ingredients {
                let have: u32 = self
                    .inventory
                    .iter()
                    .flatten()
                    .filter(|stack| stack.item == *item)
                    .map(|stack| u32::from(stack.count))
                    .sum();
                if have < u32::from(*needed) {
                    let missing = u32::from(*needed) - have;
                    return CraftOutcome {
                        crafted: false,
                        output: None,
                        message: format!("need {missing} more {}", self.item_hint_name_plural(*item, missing)),
                    };
                }
            }

            // Check output space against the inventory *after* the ingredients
            // are consumed: a full hotbar can still accept a tool when the
            // ingredients themselves free the slot. Nothing is committed until
            // the check passes.
            let mut after = self.inventory;
            for (item, needed) in recipe.ingredients {
                consume_from_inventory(&mut after, *item, *needed);
            }
            let output = ItemStack::for_item(recipe.output.0, self.registry.item(recipe.output.0), recipe.output.1);
            if !can_add_to_inventory(&self.registry, &after, output.item, output.count) {
                return CraftOutcome {
                    crafted: false,
                    output: None,
                    message: "inventory full".to_string(),
                };
            }
            self.inventory = after;
            let overflow = self.give_item(output.item, output.count);
            debug_assert_eq!(overflow, 0, "the post-consumption space check must be exact");
            self.mark_player_dirty();
            return CraftOutcome {
                crafted: true,
                output: Some(output),
                message: format!(
                    "crafted {} {}",
                    output.count,
                    self.item_hint_name_plural(recipe.output.0, u32::from(output.count))
                ),
            };
        }

        let output = ItemStack::for_item(recipe.output.0, self.registry.item(recipe.output.0), recipe.output.1);
        if !can_add_to_inventory(&self.registry, &self.inventory, output.item, output.count) {
            return CraftOutcome {
                crafted: false,
                output: None,
                message: "inventory full".to_string(),
            };
        }
        let overflow = self.give_item(output.item, output.count);
        debug_assert_eq!(overflow, 0, "creative craft passed the space precheck");
        self.mark_player_dirty();
        CraftOutcome {
            crafted: true,
            output: Some(output),
            message: format!(
                "crafted {} {}",
                output.count,
                self.item_hint_name_plural(recipe.output.0, u32::from(output.count))
            ),
        }
    }

    /// Human-readable suffix of an item resource id, with underscores replaced
    /// by spaces (`openblock:wooden_pickaxe` -> `wooden pickaxe`).
    fn item_hint_name(&self, item: ItemId) -> String {
        let id = self.registry.item(item).id;
        let path = id.split_once(':').map_or(id, |(_, path)| path);
        path.replace('_', " ")
    }

    /// [`Host::item_hint_name`] pluralised for a count (`4 sticks`).
    fn item_hint_name_plural(&self, item: ItemId, count: u32) -> String {
        let name = self.item_hint_name(item);
        if count == 1 || name.ends_with('s') {
            name
        } else {
            format!("{name}s")
        }
    }

    /// Creates a stack of `count` [`item`](ItemId) with content-defined stack
    /// size and full tool durability.
    ///
    /// Returns `None` for an unknown item id.
    pub fn make_stack(&self, item: ItemId, count: u8) -> Option<ItemStack> {
        if item.0 as usize >= self.registry.item_len() || count == 0 {
            return None;
        }
        Some(ItemStack::for_item(item, self.registry.item(item), count))
    }

    /// Adds items to matching stacks first, then to empty slots.
    ///
    /// Returns the number of items that did not fit. Stacks never exceed the
    /// content [`max_stack`](ItemType::max_stack) (capped at
    /// [`MAX_STACK_SIZE`]). An unknown [`ItemId`] is rejected and returned in
    /// full rather than panicking.
    ///
    /// Newly created items start at full tool/armor durability; use
    /// [`Host::give_stack`] when the exact stack (including durability) must be
    /// preserved.
    pub fn give_item(&mut self, item: ItemId, count: u8) -> u8 {
        if count == 0 || item.0 as usize >= self.registry.item_len() {
            return count;
        }
        let item_type = self.registry.item(item);
        let durability = item_type
            .tool
            .map(|spec| spec.durability)
            .or_else(|| item_type.armor.map(|spec| spec.durability));
        let stack = ItemStack {
            item,
            count,
            durability,
        };
        self.give_stack(stack).map_or(0, |leftover| leftover.count)
    }

    /// Adds an existing stack to the hotbar, preserving its count and
    /// durability.
    ///
    /// This is [`Host::insert_inventory_stack`] under the name used by pickup
    /// and item-transfer call sites: matching stacks are topped up first, then
    /// empty slots are filled, spreading a count larger than one max stack
    /// across slots. Tools and armor keep their remaining durability and are
    /// never repaired. Returns the leftover stack (`None` when everything
    /// fitted).
    pub fn give_stack(&mut self, stack: ItemStack) -> Option<ItemStack> {
        self.insert_inventory_stack(stack)
    }

    /// Adds an existing stack to the hotbar, merging into matching stacks
    /// first and then using empty slots.
    ///
    /// The merge key is item plus durability, so damaged tools and armor only
    /// combine with an identical item/durability pair; because their
    /// `max_stack` is one they normally occupy one slot each. Counts fill
    /// matching stacks up to the content `max_stack` and spill across as many
    /// slots as needed. Tool and armor durability is clamped to
    /// `1..=spec.durability` (a missing component means full) and is never
    /// reset to full when a remaining value is present; other items carry no
    /// durability component.
    ///
    /// Returns the leftover stack with its exact durability, or `None` when the
    /// whole stack fitted. An empty stack returns `None`; an unknown item id is
    /// returned unchanged.
    pub fn insert_inventory_stack(&mut self, mut stack: ItemStack) -> Option<ItemStack> {
        if stack.count == 0 {
            return None;
        }
        if stack.item.0 as usize >= self.registry.item_len() {
            return Some(stack);
        }
        let item_type = self.registry.item(stack.item);
        let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let spec_durability = item_type
            .tool
            .map(|spec| spec.durability)
            .or_else(|| item_type.armor.map(|spec| spec.durability));
        if let Some(max) = spec_durability {
            stack.durability = Some(stack.durability.unwrap_or(max).clamp(1, max));
        } else {
            stack.durability = None;
        }
        let original_count = stack.count;
        let mut remaining = stack.count;
        // Matching stacks first. Comparing durability as well as item keeps a
        // damaged tool from merging with a fresh one (and tools/armor have
        // max_stack 1, so they normally never merge at all).
        for slot in self.inventory.iter_mut().flatten() {
            if slot.item == stack.item && slot.durability == stack.durability && slot.count < max_stack {
                let added = remaining.min(max_stack - slot.count);
                slot.count += added;
                remaining -= added;
                if remaining == 0 {
                    break;
                }
            }
        }
        if remaining > 0 {
            for slot in self.inventory.iter_mut() {
                if slot.is_none() {
                    let added = remaining.min(max_stack);
                    *slot = Some(ItemStack { count: added, ..stack });
                    remaining -= added;
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
        if remaining < original_count {
            self.mark_player_dirty();
        }
        if remaining == 0 {
            None
        } else {
            stack.count = remaining;
            Some(stack)
        }
    }

    /// The dropped item entities, in ascending entity-id order.
    ///
    /// The slice is a snapshot owned by the host; clients copy what they need
    /// for this frame. Ids are stable across save/load; a new entity receives
    /// `max(id) + 1`.
    pub fn item_entities(&self) -> &[ItemEntity] {
        &self.items
    }

    /// Drains the queue of stacks picked up since the last call.
    ///
    /// One entry per partial or complete pickup, in the order the entities were
    /// processed (ascending id). Each stack's `count` is the number of items
    /// that actually entered the inventory, and its `item`/`durability` mirror
    /// the collected entity.
    pub fn take_pickup_events(&mut self) -> Vec<ItemStack> {
        std::mem::take(&mut self.pickup_events)
    }

    /// The furnace block entity at `pos`, if one exists.
    ///
    /// "Opening" a furnace for the future UI means checking this accessor (and
    /// holding on to the position); there is no separate open-window state on
    /// the host.
    pub fn furnace_at(&self, pos: BlockPos) -> Option<&FurnaceState> {
        self.furnaces.get(&pos)
    }

    /// The furnace block entity at `pos`, mutably.
    ///
    /// The smelting tick remains authoritative; writing progress or slots here
    /// is a debug/admin path, not part of the normal UI contract.
    pub fn furnace_at_mut(&mut self, pos: BlockPos) -> Option<&mut FurnaceState> {
        self.furnaces.get_mut(&pos)
    }

    /// Every furnace record in ascending `(x, y, z)` position order.
    pub fn furnaces(&self) -> impl Iterator<Item = (BlockPos, &FurnaceState)> + '_ {
        self.furnaces.iter().map(|(pos, state)| (*pos, state))
    }

    /// The furnace slot `slot` at `pos`, if both exist.
    pub fn furnace_slot(&self, pos: BlockPos, slot: FurnaceSlot) -> Option<ItemStack> {
        self.furnaces.get(&pos).and_then(|state| state.slot(slot))
    }

    /// Places `stack` into one furnace slot and returns the stack it replaced.
    ///
    /// * `Input` and `Fuel` are writable; `Output` is take-only and this call is
    ///   ignored for it (use [`Host::take_furnace_output`]).
    /// * A zero-count stack clears the slot and returns its previous content.
    /// * Counts clamp to the content max stack and tool/armor durability clamps
    ///   to its declared range; an unknown item id is ignored.
    /// * Replacing is a swap, never a merge: use
    ///   [`Host::insert_furnace_input`] / [`Host::insert_furnace_fuel`] to top
    ///   up an input or fuel slot.
    ///
    /// Returns `None` for an unknown position, the output slot or a rejected
    /// stack.
    pub fn put_furnace_slot(&mut self, pos: BlockPos, slot: FurnaceSlot, stack: ItemStack) -> Option<ItemStack> {
        if slot == FurnaceSlot::Output {
            return None;
        }
        if stack.count == 0 {
            let replaced = {
                let state = self.furnaces.get_mut(&pos)?;
                state.slot_mut(slot).take()
            };
            if replaced.is_some() {
                self.mark_player_dirty();
            }
            return replaced;
        }
        let normalized = sanitize_stack(&self.registry, stack)?;
        let replaced = {
            let state = self.furnaces.get_mut(&pos)?;
            state.slot_mut(slot).replace(normalized)
        };
        self.mark_player_dirty();
        replaced
    }

    /// Removes and returns one furnace slot's stack.
    ///
    /// All three slots can be taken; taking the output is the normal UI action,
    /// while input/fuel removal lets the player retrieve an item before breaking
    /// the furnace. Returns `None` for an unknown position or an empty slot.
    pub fn take_furnace_slot(&mut self, pos: BlockPos, slot: FurnaceSlot) -> Option<ItemStack> {
        let taken = {
            let state = self.furnaces.get_mut(&pos)?;
            state.slot_mut(slot).take()
        };
        if taken.is_some() {
            self.mark_player_dirty();
        }
        taken
    }

    /// Takes the finished furnace output, if any.
    pub fn take_furnace_output(&mut self, pos: BlockPos) -> Option<ItemStack> {
        self.take_furnace_slot(pos, FurnaceSlot::Output)
    }

    /// Merges up to one stack into the furnace input slot.
    ///
    /// Returns the part that did not fit (`None` when everything fitted), or the
    /// unchanged input stack for an unknown position or invalid item. The input
    /// slot accepts any item; non-smeltable items simply never cook.
    pub fn insert_furnace_input(&mut self, pos: BlockPos, stack: ItemStack) -> Option<ItemStack> {
        self.insert_furnace_stack(pos, FurnaceSlot::Input, stack)
    }

    /// Merges up to one stack into the furnace fuel slot.
    ///
    /// Same contract as [`Host::insert_furnace_input`]. The fuel slot accepts any
    /// item; non-fuel items are never consumed.
    pub fn insert_furnace_fuel(&mut self, pos: BlockPos, stack: ItemStack) -> Option<ItemStack> {
        self.insert_furnace_stack(pos, FurnaceSlot::Fuel, stack)
    }

    fn insert_furnace_stack(&mut self, pos: BlockPos, slot: FurnaceSlot, stack: ItemStack) -> Option<ItemStack> {
        let normalized = match sanitize_stack(&self.registry, stack) {
            Some(normalized) => normalized,
            None => return Some(stack),
        };
        let (leftover, changed) = {
            let Some(state) = self.furnaces.get_mut(&pos) else {
                return Some(stack);
            };
            match slot {
                FurnaceSlot::Output => return Some(stack),
                FurnaceSlot::Input => merge_stack_slot(&self.registry, &mut state.input, normalized),
                FurnaceSlot::Fuel => merge_stack_slot(&self.registry, &mut state.fuel, normalized),
            }
        };
        if changed {
            self.mark_player_dirty();
        }
        leftover
    }

    /// The chest block entity at `pos`, if one exists.
    pub fn chest_at(&self, pos: BlockPos) -> Option<&ContainerState> {
        self.chests.get(&pos)
    }

    /// The chest block entity at `pos`, mutably (debug/admin path).
    pub fn chest_at_mut(&mut self, pos: BlockPos) -> Option<&mut ContainerState> {
        self.chests.get_mut(&pos)
    }

    /// Every chest record in ascending `(x, y, z)` position order.
    pub fn chests(&self) -> impl Iterator<Item = (BlockPos, &ContainerState)> + '_ {
        self.chests.iter().map(|(pos, state)| (*pos, state))
    }

    /// Places `stack` into chest slot `slot` and returns the replaced stack.
    ///
    /// A zero-count stack clears the slot and returns its previous content.
    /// Counts clamp to the content max stack; durability clamps for tools and
    /// armor. Returns `None` for an unknown position, an out-of-range slot or an
    /// invalid item (no change in those cases).
    pub fn put_chest_slot(&mut self, pos: BlockPos, slot: usize, stack: ItemStack) -> Option<ItemStack> {
        if slot >= CHEST_SLOTS {
            return None;
        }
        if stack.count == 0 {
            let replaced = {
                let state = self.chests.get_mut(&pos)?;
                state.slots[slot].take()
            };
            if replaced.is_some() {
                self.mark_player_dirty();
            }
            return replaced;
        }
        let normalized = sanitize_stack(&self.registry, stack)?;
        let replaced = {
            let state = self.chests.get_mut(&pos)?;
            state.slots[slot].replace(normalized)
        };
        self.mark_player_dirty();
        replaced
    }

    /// Removes and returns one chest slot's stack.
    pub fn take_chest_slot(&mut self, pos: BlockPos, slot: usize) -> Option<ItemStack> {
        if slot >= CHEST_SLOTS {
            return None;
        }
        let taken = {
            let state = self.chests.get_mut(&pos)?;
            state.slots[slot].take()
        };
        if taken.is_some() {
            self.mark_player_dirty();
        }
        taken
    }

    /// Spawns one dropped stack at `pos` and returns its entity id.
    ///
    /// Block breaks use this internally; tests and debug tools may use it too.
    /// The initial velocity is a pure function of the world seed, the new
    /// entity id and the block position containing `pos` (no RNG state). The
    /// stack count is clamped to at least one and to the item's stack size.
    pub fn spawn_item_entity(&mut self, pos: WorldVec, stack: ItemStack) -> u64 {
        let id = self.next_item_id.max(1);
        self.next_item_id = id.saturating_add(1);
        let mut stack = stack;
        if stack.count == 0 {
            stack.count = 1;
        }
        if (stack.item.0 as usize) < self.registry.item_len() {
            let max_stack = self.registry.item(stack.item).max_stack.clamp(1, MAX_STACK_SIZE);
            stack.count = stack.count.min(max_stack);
        }
        let velocity = items::spawn_velocity(self.world.seed(), id, pos);
        self.items.push(ItemEntity::new(id, pos, velocity, stack));
        self.mark_player_dirty();
        id
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
            let mut remaining_stack = stack;
            remaining_stack.count = left;
            Some(remaining_stack)
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
    ///
    /// Armor mitigates the hit and wears one durability point per equipped piece;
    /// use [`Host::damage_with_outcome`] when the caller needs the reduction and
    /// broken pieces.
    pub fn damage(&mut self, amount: f32) {
        let _ = self.damage_with_outcome(amount);
    }

    /// Applies survival damage through the armor formula and reports the result.
    ///
    /// The reduction is `min(armor_defense, `[`ARMOR_DEFENSE_CAP`]`) * 4%`
    /// ([`ARMOR_REDUCTION_PER_POINT`]), so a full 20 points absorbs 80% and the
    /// result is never zero for a positive hit. Every equipped armor piece loses
    /// one durability per damaging hit; a piece at zero is removed and reported
    /// in [`DamageOutcome::armor_broke`]. Dead players and creative mode take no
    /// damage and wear no armor. Fall and starvation damage from [`Host::tick`]
    /// go through this same path.
    pub fn damage_with_outcome(&mut self, amount: f32) -> DamageOutcome {
        let mut outcome = DamageOutcome {
            raw: amount,
            ..DamageOutcome::default()
        };
        if self.game_mode != GameMode::Survival || self.player.dead || !amount.is_finite() || amount <= 0.0 {
            return outcome;
        }
        let reduction = armor_damage_reduction(self.armor_defense());
        let mitigated = amount * (1.0 - reduction);
        outcome.absorbed = amount - mitigated;
        outcome.mitigated = mitigated;
        outcome.dealt = apply_damage(&mut self.player, mitigated);
        if outcome.dealt > 0.0 {
            outcome.armor_broke = self.wear_armor();
        }
        self.mark_player_dirty();
        outcome
    }

    /// The player's four equipped armor slots, indexed by [`ArmorSlot`] order
    /// (helmet, chestplate, leggings, boots).
    pub fn equipment(&self) -> &[Option<ItemStack>; ARMOR_SLOTS] {
        &self.equipment
    }

    /// The equipped piece in one armor slot.
    pub fn equipment_slot(&self, slot: ArmorSlot) -> Option<ItemStack> {
        self.equipment[armor_slot_index(slot)]
    }

    /// Equips the armor piece in hotbar slot `inventory_slot`.
    ///
    /// The item must be armor and the slot index must be `0..`[`HOTBAR_SLOTS`].
    /// The piece moves to its [`ArmorSpec::slot`](ob_content::ArmorSpec::slot);
    /// any previously equipped piece is returned to the inventory (the source
    /// slot is free after the move, so a swap normally always fits). Returns
    /// `false` without changing anything when the slot is empty, the item is not
    /// armor, or the old piece cannot fit.
    ///
    /// Equipping is allowed in creative mode; only the defense effect is
    /// survival-only.
    pub fn equip_armor(&mut self, inventory_slot: usize) -> bool {
        if inventory_slot >= HOTBAR_SLOTS {
            return false;
        }
        let Some(stack) = self.inventory[inventory_slot].filter(|stack| stack.count > 0) else {
            return false;
        };
        let Some(armor) = self.registry.item(stack.item).armor else {
            return false;
        };
        let index = armor_slot_index(armor.slot);
        let inventory_before = self.inventory;
        let equipment_before = self.equipment;

        let mut equipped = stack;
        equipped.count = 1;
        equipped.durability = Some(stack.durability.unwrap_or(armor.durability).clamp(1, armor.durability));
        let source_count = self.inventory[inventory_slot].map_or(0, |source| source.count);
        if source_count > 1
            && let Some(source) = self.inventory[inventory_slot].as_mut()
        {
            source.count -= 1;
        } else {
            self.inventory[inventory_slot] = None;
        }
        let previous = self.equipment[index].replace(equipped);
        if let Some(previous) = previous
            && self.insert_inventory_stack(previous).is_some()
        {
            self.inventory = inventory_before;
            self.equipment = equipment_before;
            return false;
        }
        self.mark_player_dirty();
        true
    }

    /// Moves one equipped armor piece back into the inventory.
    ///
    /// Returns `false` when the slot is empty or the inventory has no room; in
    /// that case nothing changes.
    pub fn unequip_armor(&mut self, slot: ArmorSlot) -> bool {
        let index = armor_slot_index(slot);
        let Some(piece) = self.equipment[index] else {
            return false;
        };
        let inventory_before = self.inventory;
        if self.insert_inventory_stack(piece).is_some() {
            self.inventory = inventory_before;
            return false;
        }
        self.equipment[index] = None;
        self.mark_player_dirty();
        true
    }

    /// Sum of the defense points of the equipped armor.
    ///
    /// Returns `0` in creative mode and while the player is dead. Damaged pieces
    /// count until they break.
    pub fn armor_defense(&self) -> u8 {
        if self.game_mode != GameMode::Survival || self.player.dead {
            return 0;
        }
        self.equipment
            .iter()
            .flatten()
            .filter(|stack| (stack.item.0 as usize) < self.registry.item_len())
            .filter_map(|stack| self.registry.item(stack.item).armor.map(|spec| spec.defense))
            .fold(0u8, u8::saturating_add)
    }

    /// Wears every equipped armor piece by one and returns the pieces that
    /// broke, in slot order.
    fn wear_armor(&mut self) -> Vec<ArmorSlot> {
        let mut broken = Vec::new();
        for index in 0..ARMOR_SLOTS {
            let Some(piece) = self.equipment[index] else {
                continue;
            };
            let Some(armor) = self.registry.item(piece.item).armor else {
                continue;
            };
            let remaining = piece.durability.unwrap_or(armor.durability);
            if remaining <= 1 {
                self.equipment[index] = None;
                broken.push(armor.slot);
            } else {
                let mut worn = piece;
                worn.durability = Some(remaining - 1);
                self.equipment[index] = Some(worn);
            }
        }
        broken
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
    /// In survival this applies armor-mitigated fall and starvation damage and
    /// the hunger/starvation/regen rules; in creative vitals are restored every
    /// tick and no damage applies. Every stored furnace also advances one tick,
    /// whether or not its chunk is currently streamed.
    pub fn tick(&mut self, input: &PlayerInput) {
        let events = step_player(&self.world, &mut self.player, input, TICK_DT);
        self.tick_count += 1;
        match self.game_mode {
            GameMode::Survival => {
                if !self.player.dead {
                    if events.landing_impact > 0.0 {
                        self.damage_with_outcome(fall_damage(events.landing_impact));
                    }
                    if !self.player.dead {
                        // `advance_vitals` owns the hunger/starvation timers and
                        // applies starvation damage directly. Snapshot health so
                        // that damage can be re-applied through the armor path;
                        // the health delta is zero on every non-starvation tick.
                        let health_before = self.player.health;
                        advance_vitals(&mut self.player, &events, TICK_DT);
                        let starvation = health_before - self.player.health;
                        if starvation > 0.0 {
                            self.player.health = health_before;
                            self.player.dead = false;
                            self.damage_with_outcome(starvation);
                        }
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
        self.tick_mobs();
        self.tick_furnaces();
        self.tick_items(TICK_DT);
        self.mark_player_dirty();
    }

    /// Advances every stored furnace by one tick.
    ///
    /// All records tick regardless of chunk residency (documented on
    /// [`FurnaceState`]). The BTreeMap iteration order is deterministic and each
    /// furnace is independent, so save/reload cannot change the outcome.
    fn tick_furnaces(&mut self) {
        let registry = &self.registry;
        for state in self.furnaces.values_mut() {
            block_entities::tick_furnace(registry, state);
        }
    }

    /// Advances dropped items, runs survival pickup and despawns stale or
    /// void-fallen entities.
    ///
    /// Physics runs in every game mode (loaded entities keep falling); pickup
    /// only runs for a living survival player.
    fn tick_items(&mut self, dt: f64) {
        for entity in &mut self.items {
            items::step_item(&self.world, entity, dt);
        }

        if self.game_mode == GameMode::Survival && !self.player.dead {
            // Attract every ready item within the pickup radius toward the
            // player, then collect the ones that reach the chest.
            for entity in &mut self.items {
                if entity.age >= items::ITEM_PICKUP_DELAY && items::can_pick_up(&self.player, entity) {
                    items::attract_item(&self.player, entity, dt);
                }
            }
            let player = self.player.clone();
            let mut index = 0;
            while index < self.items.len() {
                let chest = items::player_chest(&player);
                let near = self.items[index].age >= items::ITEM_PICKUP_DELAY
                    && chest.distance(self.items[index].pos) <= items::ITEM_COLLECT_DISTANCE;
                if near {
                    let stack = self.items[index].stack;
                    if stack.count > 0 {
                        let leftover = self.give_stack(stack);
                        let added = stack.count - leftover.map_or(0, |left| left.count);
                        if added > 0 {
                            let mut event = stack;
                            event.count = added;
                            self.pickup_events.push(event);
                            match leftover {
                                None => {
                                    self.items.remove(index);
                                    continue;
                                }
                                Some(left) => self.items[index].stack.count = left.count,
                            }
                        }
                    }
                }
                index += 1;
            }
        }

        self.items
            .retain(|entity| entity.age < items::ITEM_DESPAWN_AGE && entity.pos.y >= items::ITEM_DESPAWN_Y);
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
    /// wears the selected tool by one durability per broken block (removing it
    /// at zero, reported through [`ActionOutcome::tool_broke`]) and consumes one
    /// matching selected block item when placing. Every survival loot stack
    /// (including the deterministic rare apple from leaves) spawns one
    /// [`ItemEntity`] at the broken block's centre instead of entering the
    /// inventory directly; [`ActionOutcome::drops`] still lists those stacks.
    ///
    /// Placing a furnace or chest creates an empty block entity at the filled
    /// cell. Breaking one removes its record: survival appends its slots to
    /// [`ActionOutcome::drops`] (furnace input/fuel/output, chest slots
    /// ascending) and spawns them as item entities; creative discards them along
    /// with the block loot.
    ///
    /// A dead player cannot break or place anything: the call returns an
    /// unchanged outcome. This matches the network intent path, which rejects
    /// dead players before reaching the shared survival helpers.
    pub fn apply_action_with_outcome(&mut self, action: &BlockAction) -> ActionOutcome {
        let mut outcome = ActionOutcome::default();

        if self.player.dead {
            return outcome;
        }

        if self.game_mode == GameMode::Creative {
            let break_target = self.break_target(action);
            let place_target = self.placement_cell(action);
            outcome.changed = apply_block_actions(&mut self.world, &self.player, action);
            if outcome.changed {
                if let Some((pos, block)) = break_target
                    && self.world.get_block(pos) == BlockId::AIR
                {
                    // Creative breaks drop no block loot; container contents are
                    // discarded with the block.
                    self.remove_block_entity(pos, block);
                }
                if let Some(pos) = place_target
                    && self.world.get_block(pos) == action.place
                {
                    self.register_block_entity(pos, action.place);
                }
                self.mark_player_dirty();
            }
            return outcome;
        }

        // Placement is resolved before the world changes, exactly like
        // `apply_block_actions` does internally, so the block entity can be
        // registered at the cell the action actually filled.
        let place_target = self.placement_cell(action);

        if action.break_block
            && let Some(hit) = self.world.raycast(self.player.eye(), self.player.forward(), REACH)
        {
            let broken = self.break_block_at(hit.block);
            merge_action_outcome(&mut outcome, broken);
        }

        if action.place_block
            && action.place != BlockId::AIR
            && let Some(target) = place_target
        {
            let placed = self.place_selected_at(target, action.place);
            merge_action_outcome(&mut outcome, placed);
        }

        if outcome.changed {
            self.mark_player_dirty();
        }
        outcome
    }

    /// Breaks the exact block at `hit` in survival and returns the loot,
    /// container contents and tool wear.
    ///
    /// Shared by the single-player camera action and the exact-position
    /// network intent path. The caller has already resolved/validated the
    /// position; this method still checks the block is present, registered and
    /// breakable before mutating the world.
    fn break_block_at(&mut self, hit: BlockPos) -> ActionOutcome {
        let mut outcome = ActionOutcome::default();
        if !hit.in_world() || self.world.chunk(hit.chunk()).is_none() {
            return outcome;
        }
        let block = self.world.get_block(hit);
        let held = self.selected_stack();
        let held_item = held.map(|stack| stack.item);
        if block == BlockId::AIR || !self.registry.block(block).breakable() || !self.world.set_block(hit, BlockId::AIR)
        {
            return outcome;
        }
        outcome.changed = true;
        outcome.broke = Some(block);
        // A tiered block still breaks with a hand or an under-tier tool, but
        // yields no loot (Minecraft-like).
        if self.registry.can_drop(block, held_item) {
            if let Some((item, count)) = self.registry.block_drop(block) {
                outcome.drops.push(ItemStack::new(item, count));
            }
            let leaves = self.registry.id_or_air("openblock:leaves");
            if block == leaves
                && apple_drop(self.world.seed(), hit)
                && let Some(apple) = self.registry.item_id("openblock:apple")
            {
                outcome.drops.push(ItemStack::new(apple, 1));
            }
        }
        // Furnace and chest contents drop regardless of the tool tier and join
        // the same entity-spawning path as block loot.
        outcome.drops.extend(self.take_block_entity_contents(hit, block));
        // Dropped loot becomes authoritative item entities at the block
        // centre; the inventory is only touched by pickup in `tick`, never by
        // the break itself.
        let centre = block_centre(hit);
        for drop in &outcome.drops {
            self.spawn_item_entity(centre, *drop);
        }
        // Any selected tool loses one durability per broken block, whether or
        // not its kind matched. At zero the stack is removed and the break is
        // reported.
        if let Some(stack) = held
            && let Some(spec) = self.registry.item(stack.item).tool
        {
            let remaining = stack.durability.unwrap_or(spec.durability);
            if remaining <= 1 {
                self.inventory[self.hotbar_selection] = None;
                outcome.tool_broke = Some(stack.item);
            } else {
                self.inventory[self.hotbar_selection] = Some(stack.with_durability(Some(remaining - 1)));
            }
        }
        self.mark_player_dirty();
        outcome
    }

    /// Places `block` into the exact air cell `target`, consuming one matching
    /// selected item in survival.
    ///
    /// Shared by the single-player camera action and the exact-position
    /// network intent path. Adjacency/raycast validation is the caller's
    /// responsibility; this method validates the world cell, the selected
    /// item, the player overlap and block registration.
    fn place_selected_at(&mut self, target: BlockPos, block: BlockId) -> ActionOutcome {
        let mut outcome = ActionOutcome::default();
        if block == BlockId::AIR
            || !target.in_world()
            || self.world.chunk(target.chunk()).is_none()
            || self.world.get_block(target) != BlockId::AIR
            || Aabb::block(target).intersects(&self.player.aabb())
        {
            return outcome;
        }
        let matches_selection = self
            .selected_stack()
            .is_some_and(|stack| stack.count > 0 && self.registry.item(stack.item).block == Some(block));
        if !matches_selection || self.registry.block(block).fluid {
            return outcome;
        }
        if !self.world.set_block(target, block) {
            return outcome;
        }
        let consumed = self.consume_selected(1);
        debug_assert!(consumed, "a matching selected stack must be consumable");
        outcome.changed = true;
        outcome.placed = Some(block);
        self.register_block_entity(target, block);
        self.mark_player_dirty();
        outcome
    }

    /// The block a break action would remove, resolved before the action.
    fn break_target(&self, action: &BlockAction) -> Option<(BlockPos, BlockId)> {
        if !action.break_block {
            return None;
        }
        let hit = self.world.raycast(self.player.eye(), self.player.forward(), REACH)?;
        let block = self.world.get_block(hit.block);
        (block != BlockId::AIR).then_some((hit.block, block))
    }

    /// The air cell a placement would fill, resolved before the action.
    fn placement_cell(&self, action: &BlockAction) -> Option<BlockPos> {
        if !action.place_block || action.place == BlockId::AIR {
            return None;
        }
        let hit = self.world.raycast(self.player.eye(), self.player.forward(), REACH)?;
        if !self.world.is_solid(hit.block) {
            return None;
        }
        let target = hit.block.offset(hit.normal.x, hit.normal.y, hit.normal.z);
        (self.world.get_block(target) == BlockId::AIR).then_some(target)
    }

    /// Creates empty furnace/chest state for a freshly placed block.
    fn register_block_entity(&mut self, pos: BlockPos, block: BlockId) {
        if block == self.registry.id_or_air("openblock:furnace") {
            if self.furnaces.insert(pos, FurnaceState::default()).is_some() {
                log::warn!(
                    "placing a furnace at ({}, {}, {}) replaced a stale record",
                    pos.x,
                    pos.y,
                    pos.z
                );
            }
        } else if block == self.registry.id_or_air("openblock:chest")
            && self.chests.insert(pos, ContainerState::default()).is_some()
        {
            log::warn!(
                "placing a chest at ({}, {}, {}) replaced a stale record",
                pos.x,
                pos.y,
                pos.z
            );
        }
    }

    /// Removes the block entity for a broken block and returns its contents in
    /// canonical slot order (furnace input/fuel/output, chest slots ascending).
    fn take_block_entity_contents(&mut self, pos: BlockPos, block: BlockId) -> Vec<ItemStack> {
        if block == self.registry.id_or_air("openblock:furnace") {
            self.furnaces
                .remove(&pos)
                .map_or_else(Vec::new, |state| state.contents())
        } else if block == self.registry.id_or_air("openblock:chest") {
            self.chests.remove(&pos).map_or_else(Vec::new, |state| state.contents())
        } else {
            Vec::new()
        }
    }

    /// Removes a block entity and discards its contents (creative/remote edits).
    fn remove_block_entity(&mut self, pos: BlockPos, block: BlockId) {
        let _ = self.take_block_entity_contents(pos, block);
    }

    /// Applies an authoritative block edit received from a multiplayer client.
    ///
    /// Unlike [`Host::apply_action`] this does **not** raycast or consult the
    /// internal player's reach. The caller (the network layer) validates the
    /// distance from the requesting client; this method validates the block,
    /// the world bounds and the target column:
    ///
    /// * `block == AIR` breaks an existing block and requires it to be
    ///   registered and [`breakable`](ob_content::BlockType::breakable);
    /// * any other block is placed only into air and must be registered and
    ///   non-fluid.
    ///
    /// No loot, inventory, tool wear or player statistics are affected: this is
    /// the world-authority path for the interim multiplayer protocol. Returns
    /// `true` only when the world actually changed. The column must already be
    /// loaded; otherwise the edit is rejected rather than silently lost.
    ///
    /// Block entity bookkeeping follows the edit: placing a furnace or chest
    /// creates an empty record and breaking one removes its record. As with
    /// block loot, a remote break discards the container contents (this path has
    /// no loot authority); the interim protocol has no container messages yet.
    pub fn apply_remote_edit(&mut self, pos: BlockPos, block: BlockId) -> bool {
        if !pos.in_world() || block.0 as usize >= self.registry.len() {
            return false;
        }
        let current = self.world.get_block(pos);
        if current == block {
            return false;
        }
        if block == BlockId::AIR {
            if current == BlockId::AIR || !self.registry.block(current).breakable() {
                return false;
            }
        } else if current != BlockId::AIR || self.registry.block(block).fluid {
            return false;
        }
        if self.world.chunk(pos.chunk()).is_none() {
            return false;
        }
        if !self.world.set_block(pos, block) {
            return false;
        }
        if block == BlockId::AIR {
            self.remove_block_entity(pos, current);
        } else {
            self.register_block_entity(pos, block);
        }
        true
    }

    /// Generates up to `budget` missing chunk columns within `radius_chunks` of
    /// `center` and returns how many were generated.
    ///
    /// Columns are visited in the same deterministic square-ring order used by
    /// [`Host::ensure_streaming`] (ascending ring, `dz` then `dx` inside each
    /// ring). This variant does **not** unload anything: it is intended for
    /// multi-client streaming, where the resident set is the union of every
    /// client's interest area. A radius of `0` generates exactly the chunk
    /// containing `center`.
    pub fn ensure_streaming_around(&mut self, center: WorldVec, radius_chunks: i32, budget: usize) -> usize {
        if budget == 0 || !center.is_finite() {
            return 0;
        }
        let center_chunk = BlockPos::new(
            center.x.floor() as i32,
            center.y.floor() as i32,
            center.z.floor() as i32,
        )
        .chunk();
        let radius = radius_chunks.clamp(0, 32);

        let mut positions = Vec::new();
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
        generated
    }

    /// Unloads every resident column that is farther than `radius_chunks` from
    /// every centre in `centers`, using Chebyshev distance, and returns the
    /// number removed.
    ///
    /// A two-chunk hysteresis is the caller's choice: single-player streaming
    /// passes `render_distance_chunks + 2`. No-op for an empty `centers` list.
    /// Removed columns are queued for [`Host::take_unloaded_chunks`].
    pub fn unload_chunks_outside(&mut self, centers: &[WorldVec], radius_chunks: i32) -> usize {
        if centers.is_empty() {
            return 0;
        }
        let radius = radius_chunks.max(0);
        let center_chunks: Vec<ChunkPos> = centers
            .iter()
            .filter(|center| center.is_finite())
            .map(|center| {
                BlockPos::new(
                    center.x.floor() as i32,
                    center.y.floor() as i32,
                    center.z.floor() as i32,
                )
                .chunk()
            })
            .collect();
        if center_chunks.is_empty() {
            return 0;
        }
        let mut to_unload = Vec::new();
        for (pos, _) in self.world.chunks() {
            let keep = center_chunks
                .iter()
                .any(|center| (pos.x - center.x).abs() <= radius && (pos.z - center.z).abs() <= radius);
            if !keep {
                to_unload.push(*pos);
            }
        }
        let removed = to_unload.len();
        for pos in to_unload {
            self.world.remove_chunk(pos);
            self.unloaded.push(pos);
        }
        removed
    }

    /// Generates up to `budget` missing chunk columns around the player and
    /// unloads columns beyond the streaming radius. Returns the number of
    /// newly generated chunks.
    pub fn ensure_streaming(&mut self, budget: usize) -> usize {
        let center = self.player.pos;
        let radius = self.config.render_distance_chunks.max(1);
        let generated = self.ensure_streaming_around(center, radius, budget);
        // Unload chunks outside radius + 2 (hysteresis).
        self.unload_chunks_outside(&[center], radius + 2);
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

    /// Saves the world, player state, spawn point, inventory, dropped items,
    /// block entities and equipped armor.
    ///
    /// Stale furnace/chest records are pruned first (with a warning), so a block
    /// removed through a direct world edit does not survive in the sidecar.
    ///
    /// # Commit protocol
    ///
    /// Every payload is serialised first, then written to a `.tmp` file and
    /// `fsync`ed, and only after **all** temporary files exist are they renamed
    /// over the targets. Renames put the sidecars first and `world.obs` last,
    /// so a failure while staging or a rename failure leaves the previous world
    /// snapshot in place; the next start can still open the last committed
    /// world. The individual replacements are atomic; the multi-file group is
    /// not transactional (a crash between two renames can mix one new sidecar
    /// with the previous world), which is why the format still expects the
    /// journal from spec 17 §3.5 for a future region-file save path.
    ///
    /// The encoded world is checked against the loader limits
    /// ([`ob_world::MAX_SAVE_PAYLOAD_BYTES`] / [`ob_world::MAX_SAVE_FILE_BYTES`])
    /// before anything is written, so a long-running server cannot create a
    /// snapshot it will refuse to open on the next start.
    pub fn save(&mut self) -> Result<(), HostError> {
        std::fs::create_dir_all(&self.world_dir)?;
        block_entities::prune_block_entities(&self.world, &self.registry, &mut self.furnaces, &mut self.chests);

        // Serialise everything before touching the directory. The world is
        // last so the renames below commit sidecars before the snapshot they
        // describe.
        let files: [(&str, Vec<u8>); 9] = [
            ("player.txt", player_state(&self.player, self.tick_count).into_bytes()),
            (SPAWN_FILE, spawn_state(&self.spawn).into_bytes()),
            (
                "inventory.txt",
                inventory_state(&self.registry, &self.inventory).into_bytes(),
            ),
            (
                "items.txt",
                items::items_state(&self.registry, &self.items).into_bytes(),
            ),
            (
                mobs::MOBS_FILE,
                mobs::mobs_state(&self.registry, &self.mobs).into_bytes(),
            ),
            (
                block_entities::BLOCK_ENTITIES_FILE,
                block_entities::block_entities_state(&self.registry, &self.furnaces, &self.chests).into_bytes(),
            ),
            (
                EQUIPMENT_FILE,
                equipment_state(&self.registry, &self.equipment).into_bytes(),
            ),
            (
                net_players::NETWORK_PLAYERS_FILE,
                net_players::network_players_state(&self.registry, &self.network_players, &self.saved_network_players)
                    .into_bytes(),
            ),
            ("world.obs", ob_world::to_bytes_checked(&self.world)?),
        ];

        // Reject a target path that is a directory before staging anything: a
        // rename over a directory would otherwise leave the group half written.
        for (name, _) in &files {
            let target = self.world_dir.join(name);
            if target.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("save target {} is a directory", target.display()),
                )
                .into());
            }
        }

        let mut staged: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::with_capacity(files.len());
        for (name, bytes) in &files {
            let target = self.world_dir.join(name);
            let temporary = target.with_extension("tmp");
            if let Err(error) = write_staged(&temporary, bytes) {
                let _ = std::fs::remove_file(&temporary);
                for (temporary, _) in &staged {
                    let _ = std::fs::remove_file(temporary);
                }
                return Err(error.into());
            }
            staged.push((temporary, target));
        }

        for (index, (temporary, target)) in staged.iter().enumerate() {
            if let Err(error) = std::fs::rename(temporary, target) {
                for (temporary, _) in &staged[index..] {
                    let _ = std::fs::remove_file(temporary);
                }
                return Err(error.into());
            }
        }
        // Best effort: make the renames durable where the platform supports
        // syncing a directory handle. Individual files were already fsynced.
        if let Ok(directory) = std::fs::File::open(&self.world_dir) {
            let _ = directory.sync_all();
        }
        self.dirty_player = false;
        Ok(())
    }

    /// Deterministic hash of the simulation state: world blocks in canonical
    /// order, player position, vitals, look direction and flight state
    /// quantised, the persisted spawn point, inventory, dropped item entities
    /// sorted by id, mobs sorted by id (id, kind, quantised position, yaw,
    /// health and age), furnace and chest block entities sorted by position,
    /// equipped armor in slot order, and tick count.
    ///
    /// Every persisted single-player field participates, so a save/reload must
    /// reproduce the hash exactly. Used by the smoke test and cross-platform
    /// determinism checks. (Network-player runtime state has its own sidecar and
    /// round-trip tests and is intentionally excluded; light is deterministic
    /// from the block set and generation and is not mutated by 1.0 gameplay.)
    pub fn state_hash(&self) -> u64 {
        let mut hasher = StableHasher::new();
        hasher.write_u64(self.world.seed());
        hasher.write_u64(self.tick_count);
        hasher.write_i64((self.player.pos.x * 1000.0) as i64);
        hasher.write_i64((self.player.pos.y * 1000.0) as i64);
        hasher.write_i64((self.player.pos.z * 1000.0) as i64);
        // Look direction and flight are persisted in player.txt and affect
        // raycasts, attacks and placement; normalise exactly like save/load.
        hasher.write_i64(quantize(f64::from(net_players::normalise_angle(self.player.yaw))));
        hasher.write_i64(quantize(f64::from(net_players::clamp_pitch(self.player.pitch))));
        hasher.write_u8(self.player.flying as u8);
        hasher.write_i32((self.player.health * 1000.0) as i32);
        hasher.write_i32((self.player.hunger * 1000.0) as i32);
        hasher.write_u8(self.player.dead as u8);
        // The spawn point is persisted and respawn-visible; changing it must
        // change the simulation hash.
        hasher.write_u8(0xF4);
        hasher.write_i64(quantize(self.spawn.x));
        hasher.write_i64(quantize(self.spawn.y));
        hasher.write_i64(quantize(self.spawn.z));
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
            // Durability is part of the authoritative stack state: a repaired or
            // worn tool must change the simulation hash.
            match stack.durability {
                Some(remaining) => {
                    hasher.write_u8(1);
                    hasher.write_u16(remaining);
                }
                None => hasher.write_u8(0),
            }
        }
        let mut entities: Vec<&ItemEntity> = self.items.iter().collect();
        entities.sort_by_key(|entity| entity.id);
        hasher.write_u64(entities.len() as u64);
        for entity in entities {
            hasher.write_u64(entity.id);
            hasher.write_i64(quantize(entity.pos.x));
            hasher.write_i64(quantize(entity.pos.y));
            hasher.write_i64(quantize(entity.pos.z));
            hasher.write_i64(quantize(entity.velocity.x));
            hasher.write_i64(quantize(entity.velocity.y));
            hasher.write_i64(quantize(entity.velocity.z));
            if (entity.stack.item.0 as usize) < self.registry.item_len() {
                hasher.write_u8(1);
                let name = self.registry.item(entity.stack.item).id;
                hasher.write_u16(name.len() as u16);
                hasher.write(name.as_bytes());
            } else {
                // Defensive: an invalid stack cannot be saved, but hashing it
                // must not panic. Encode the raw id plus an "unknown" marker.
                hasher.write_u8(0);
                hasher.write_u16(entity.stack.item.0);
            }
            hasher.write_u8(entity.stack.count);
            match entity.stack.durability {
                Some(remaining) => {
                    hasher.write_u8(1);
                    hasher.write_u16(remaining);
                }
                None => hasher.write_u8(0),
            }
            hasher.write_i64(quantize(entity.age));
        }
        // Mobs hash only persisted fields, sorted by id. Transient AI and
        // combat timers are intentionally excluded so a save/reload matches.
        mobs::hash_mobs(&mut hasher, &self.registry, &self.mobs);
        // Block entities are hashed in ascending (x, y, z) order. A section
        // marker keeps the encoding unambiguous; empty furnaces/chests still
        // contribute their position so an empty container survives a reload.
        hasher.write_u8(0xF1);
        hasher.write_u64(self.furnaces.len() as u64);
        for (pos, state) in &self.furnaces {
            hasher.write_i32(pos.x);
            hasher.write_i32(pos.y);
            hasher.write_i32(pos.z);
            hasher.write_u16(state.burn_remaining);
            hasher.write_u16(state.burn_total);
            hasher.write_u16(state.cook_progress);
            for slot in FurnaceSlot::ALL {
                hash_optional_stack(&mut hasher, &self.registry, &state.slot(slot));
            }
        }
        hasher.write_u8(0xF2);
        hasher.write_u64(self.chests.len() as u64);
        for (pos, state) in &self.chests {
            hasher.write_i32(pos.x);
            hasher.write_i32(pos.y);
            hasher.write_i32(pos.z);
            for slot in &state.slots {
                hash_optional_stack(&mut hasher, &self.registry, slot);
            }
        }
        // Equipped armor, slot by slot.
        hasher.write_u8(0xF3);
        for slot in &self.equipment {
            hash_optional_stack(&mut hasher, &self.registry, slot);
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

/// Merges one extracted action outcome into an accumulated one.
fn merge_action_outcome(target: &mut ActionOutcome, source: ActionOutcome) {
    target.changed |= source.changed;
    target.broke = source.broke.or(target.broke);
    target.placed = source.placed.or(target.placed);
    target.drops.extend(source.drops);
    target.pickup_overflow.extend(source.pickup_overflow);
    target.tool_broke = source.tool_broke.or(target.tool_broke);
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

/// Maximum size of one world-directory sidecar accepted from disk (8 MiB).
///
/// A world directory is hostile input ([17 §3.10]); without a cap a multi-gigabyte
/// `items.txt` or `network_players.txt` would be read into memory before any
/// validation ran.
pub(crate) const MAX_SIDECAR_BYTES: u64 = 8 * 1024 * 1024;

/// Reads one UTF-8 sidecar with a hard size cap.
///
/// Missing files still return [`io::ErrorKind::NotFound`] so callers keep their
/// "old save, use defaults" path. An oversized file returns
/// [`io::ErrorKind::InvalidData`] after reading at most one byte past the cap;
/// the metadata check avoids reading a huge sparse or regular file at all.
pub(crate) fn read_sidecar(path: &Path) -> io::Result<String> {
    // A world directory is untrusted input: check that the path is a regular
    // file before opening it, because opening a FIFO or device can block
    // forever (and `read_to_end` on a device can stream gigabytes). The second
    // check on the opened handle narrows the metadata/open race.
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sidecar {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sidecar exceeds the {MAX_SIDECAR_BYTES}-byte limit"),
        ));
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sidecar {} is not a regular file", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SIDECAR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sidecar exceeds the {MAX_SIDECAR_BYTES}-byte limit"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Serialized world spawn: three whitespace-separated finite coordinates.
fn spawn_state(spawn: &WorldVec) -> String {
    format!("{:?} {:?} {:?}\n", spawn.x, spawn.y, spawn.z)
}

/// Loads `spawn.txt`, ignoring malformed or out-of-range coordinates.
///
/// A missing file is the normal path for worlds created before the spawn
/// sidecar existed; the caller resolves and persists a spawn in that case.
fn load_spawn(world_dir: &Path) -> Option<WorldVec> {
    let path = world_dir.join(SPAWN_FILE);
    let text = match read_sidecar(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                log::warn!("could not read {}: {err}; recomputing the spawn", path.display());
            }
            return None;
        }
    };
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 3 {
        log::warn!("{}: ignoring malformed spawn line", path.display());
        return None;
    }
    let x = fields[0].parse::<f64>().ok()?;
    let y = fields[1].parse::<f64>().ok()?;
    let z = fields[2].parse::<f64>().ok()?;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        log::warn!("{}: ignoring non-finite spawn", path.display());
        return None;
    }
    let y_min = f64::from(ob_math::WORLD_MIN_Y) - 32.0;
    let y_max = f64::from(ob_math::WORLD_MAX_Y) + 256.0;
    if x.abs() > 30_000_000.0 || z.abs() > 30_000_000.0 || !(y_min..=y_max).contains(&y) {
        log::warn!("{}: ignoring out-of-range spawn", path.display());
        return None;
    }
    Some(WorldVec::new(x, y, z))
}

/// Serialized player state: ten whitespace-separated fields.
///
/// `x y z yaw pitch flying ticks health hunger dead`
///
/// Look angles are stored in their canonical range (yaw `-PI..=PI`, pitch
/// `-PI/2..=PI/2`) so `save -> load -> save` is byte-stable and the state hash
/// can cover exactly the persisted values.
fn player_state(player: &Player, ticks: u64) -> String {
    let yaw = net_players::normalise_angle(player.yaw);
    let pitch = net_players::clamp_pitch(player.pitch);
    format!(
        "{:?} {:?} {:?} {:?} {:?} {} {} {:?} {:?} {}\n",
        player.pos.x,
        player.pos.y,
        player.pos.z,
        yaw,
        pitch,
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

    // A sidecar is untrusted input: a finite but absurd coordinate must not
    // place the player outside every streamable chunk or poison the state hash.
    let x = x.clamp(-30_000_000.0, 30_000_000.0);
    let y = y.clamp(
        f64::from(ob_math::WORLD_MIN_Y) - 32.0,
        f64::from(ob_math::WORLD_MAX_Y) + 256.0,
    );
    let z = z.clamp(-30_000_000.0, 30_000_000.0);
    let mut player = Player::new(WorldVec::new(x, y, z));
    player.yaw = net_players::normalise_angle(yaw);
    player.pitch = net_players::clamp_pitch(pitch);
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
    if let Ok(text) = read_sidecar(&path) {
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

/// Serialized inventory: one `slot item_resource_id count [durability]` line per
/// non-empty slot in slot order.
///
/// The fourth field is only written for tools. Empty lines and empty slots are
/// skipped, and old three-field lines still load (tools start at full
/// durability).
fn inventory_state(registry: &Registry, inventory: &[Option<ItemStack>; HOTBAR_SLOTS]) -> String {
    let mut text = String::new();
    for (slot, stack) in inventory.iter().enumerate() {
        let Some(stack) = stack else {
            continue;
        };
        if stack.is_empty() {
            continue;
        }
        let name = registry.item(stack.item).id;
        match stack.durability {
            Some(remaining) => text.push_str(&format!("{slot} {name} {} {remaining}\n", stack.count)),
            None => text.push_str(&format!("{slot} {name} {}\n", stack.count)),
        }
    }
    text
}

/// Loads `inventory.txt`, dropping unknown items and malformed lines with a
/// warning. A missing file (old worlds) is an empty inventory.
///
/// Accepts both the v0.0.7 `slot item count` format and the durability-aware
/// `slot item count durability` format. Counts clamp to the content
/// `max_stack`; tool durability clamps to `1..=spec.durability` and defaults to
/// full for old lines.
fn load_inventory(world_dir: &Path, registry: &Registry) -> [Option<ItemStack>; HOTBAR_SLOTS] {
    let mut inventory = [None; HOTBAR_SLOTS];
    let path = world_dir.join("inventory.txt");
    let text = match read_sidecar(&path) {
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
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if !(3..=4).contains(&fields.len()) {
            log::warn!("inventory.txt:{line_number}: ignoring malformed line");
            continue;
        }
        let Ok(slot) = fields[0].parse::<usize>() else {
            log::warn!("inventory.txt:{line_number}: invalid slot '{}'", fields[0]);
            continue;
        };
        let Some(item) = registry.item_id(fields[1]) else {
            log::warn!("inventory.txt:{line_number}: unknown item '{}', dropping it", fields[1]);
            continue;
        };
        let Ok(count) = fields[2].parse::<u32>() else {
            log::warn!("inventory.txt:{line_number}: invalid count '{}'", fields[2]);
            continue;
        };
        let item_type = registry.item(item);
        let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let count = count.clamp(1, u32::from(max_stack)) as u8;
        let mut stack = ItemStack::for_item(item, item_type, count);
        let durability_spec = item_type
            .tool
            .map(|spec| spec.durability)
            .or_else(|| item_type.armor.map(|spec| spec.durability));
        if let Some(max_durability) = durability_spec {
            let durability = match fields.get(3) {
                Some(text) => match text.parse::<u32>() {
                    Ok(value) => value.clamp(1, u32::from(max_durability)) as u16,
                    Err(_) => {
                        log::warn!("inventory.txt:{line_number}: invalid durability '{text}'");
                        continue;
                    }
                },
                // Older lines predate durability: tools/armor start undamaged.
                None => max_durability,
            };
            stack.durability = Some(durability);
        }
        let slot = slot.min(HOTBAR_SLOTS - 1);
        inventory[slot] = Some(stack);
    }
    inventory
}

/// Serialized equipped armor: one `<slot> <item> <durability>` line per piece
/// in slot order (helmet, chestplate, leggings, boots).
///
/// The count is implicit (armor stacks to one) and empty slots are skipped.
fn equipment_state(registry: &Registry, equipment: &[Option<ItemStack>; ARMOR_SLOTS]) -> String {
    let mut text = String::new();
    for slot in ARMOR_SLOT_ORDER {
        let index = armor_slot_index(slot);
        let Some(stack) = equipment[index].filter(|stack| stack.count > 0) else {
            continue;
        };
        let Some(armor) = registry.item(stack.item).armor else {
            log::warn!("equipment: skipping non-armor item in the {} slot", slot.name());
            continue;
        };
        if armor.slot != slot {
            log::warn!("equipment: skipping {} in the wrong slot", registry.item(stack.item).id);
            continue;
        }
        let name = registry.item(stack.item).id;
        let durability = stack.durability.unwrap_or(armor.durability).clamp(1, armor.durability);
        text.push_str(&format!("{} {name} {durability}\n", slot.name()));
    }
    text
}

/// Loads `equipment.txt`.
///
/// A missing file (a save from before this slice) means no armor is equipped.
/// Malformed lines, unknown items, mismatched slots and duplicates are skipped
/// with a warning; durability is clamped to the armor's range and defaults to
/// full when the field is absent.
fn load_equipment(world_dir: &Path, registry: &Registry) -> [Option<ItemStack>; ARMOR_SLOTS] {
    let mut equipment = [None; ARMOR_SLOTS];
    let path = world_dir.join(EQUIPMENT_FILE);
    let text = match read_sidecar(&path) {
        Ok(text) => text,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                log::warn!("could not read {}: {err}; no armor equipped", path.display());
            }
            return equipment;
        }
    };

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if !(2..=3).contains(&fields.len()) {
            log::warn!("equipment.txt:{line_number}: ignoring malformed line");
            continue;
        }
        let Some(slot) = armor_slot_from_name(fields[0]) else {
            log::warn!("equipment.txt:{line_number}: unknown armor slot '{}'", fields[0]);
            continue;
        };
        let Some(item) = registry.item_id(fields[1]) else {
            log::warn!("equipment.txt:{line_number}: unknown item '{}', dropping it", fields[1]);
            continue;
        };
        let item_type = registry.item(item);
        let Some(armor) = item_type.armor else {
            log::warn!("equipment.txt:{line_number}: '{}' is not armor", fields[1]);
            continue;
        };
        if armor.slot != slot {
            log::warn!(
                "equipment.txt:{line_number}: '{}' belongs in the {} slot, not {}",
                fields[1],
                armor.slot.name(),
                slot.name()
            );
            continue;
        }
        let durability = match fields.get(2) {
            Some(text) => match text.parse::<u32>() {
                Ok(value) => value.clamp(1, u32::from(armor.durability)) as u16,
                Err(_) => {
                    log::warn!("equipment.txt:{line_number}: invalid durability '{text}'");
                    continue;
                }
            },
            None => armor.durability,
        };
        let slot_index = armor_slot_index(slot);
        if equipment[slot_index].is_some() {
            log::warn!("equipment.txt:{line_number}: duplicate {} slot", slot.name());
            continue;
        }
        equipment[slot_index] = Some(ItemStack {
            item,
            count: 1,
            durability: Some(durability),
        });
    }
    equipment
}

/// Canonical armor slot order used by saves and state hashing.
const ARMOR_SLOT_ORDER: [ArmorSlot; ARMOR_SLOTS] = [
    ArmorSlot::Helmet,
    ArmorSlot::Chestplate,
    ArmorSlot::Leggings,
    ArmorSlot::Boots,
];

fn armor_slot_from_name(name: &str) -> Option<ArmorSlot> {
    ARMOR_SLOT_ORDER.into_iter().find(|slot| slot.name() == name)
}

/// Whether `count` more of `item` fit in a nine-slot inventory.
///
/// Capacity comes from matching non-empty stacks plus empty slots; tools never
/// merge into a damaged stack and are capped at one per slot by their own
/// `max_stack`.
fn can_add_to_inventory(
    registry: &Registry,
    inventory: &[Option<ItemStack>; HOTBAR_SLOTS],
    item: ItemId,
    count: u8,
) -> bool {
    let item_type = registry.item(item);
    let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
    let durability = item_type
        .tool
        .map(|spec| spec.durability)
        .or_else(|| item_type.armor.map(|spec| spec.durability));
    let mut capacity = 0u32;
    for stack in inventory.iter().flatten() {
        if stack.item == item && stack.durability == durability && stack.count < max_stack {
            capacity += u32::from(max_stack - stack.count);
        }
    }
    let empty_slots = inventory.iter().filter(|slot| slot.is_none()).count() as u32;
    capacity += empty_slots * u32::from(max_stack);
    capacity >= u32::from(count)
}

/// Removes `count` of `item` from a nine-slot inventory in slot order.
///
/// Callers must have checked availability first; this is a partial helper for
/// crafting and does not report failure.
fn consume_from_inventory(inventory: &mut [Option<ItemStack>; HOTBAR_SLOTS], item: ItemId, count: u8) {
    let mut remaining = u32::from(count);
    for slot in inventory.iter_mut() {
        if remaining == 0 {
            break;
        }
        let Some(stack) = slot else {
            continue;
        };
        if stack.item != item {
            continue;
        }
        let taken = remaining.min(u32::from(stack.count));
        let left = u32::from(stack.count) - taken;
        remaining -= taken;
        *slot = if left == 0 {
            None
        } else {
            let mut left_stack = *stack;
            left_stack.count = left as u8;
            Some(left_stack)
        };
    }
}

/// Clamps a caller-provided stack to the content limits for storage in a block
/// entity.
///
/// Returns `None` for an unknown item id or an empty stack. Tool and armor
/// durability is normalised to `1..=spec.durability` (missing durability means
/// full); other items carry no durability component.
fn sanitize_stack(registry: &Registry, mut stack: ItemStack) -> Option<ItemStack> {
    if stack.count == 0 || stack.item.0 as usize >= registry.item_len() {
        return None;
    }
    let item_type = registry.item(stack.item);
    stack.count = stack.count.min(item_type.max_stack.clamp(1, MAX_STACK_SIZE));
    let spec_durability = item_type
        .tool
        .map(|spec| spec.durability)
        .or_else(|| item_type.armor.map(|spec| spec.durability));
    stack.durability = spec_durability.map(|max| stack.durability.unwrap_or(max).clamp(1, max));
    Some(stack)
}

/// Merges `incoming` into one block-entity slot.
///
/// Returns the leftover stack (if any) and whether the slot changed. The caller
/// has already sanitised `incoming`.
fn merge_stack_slot(
    registry: &Registry,
    slot: &mut Option<ItemStack>,
    mut incoming: ItemStack,
) -> (Option<ItemStack>, bool) {
    match slot {
        None => {
            *slot = Some(incoming);
            (None, true)
        }
        Some(existing) if existing.item == incoming.item && existing.durability == incoming.durability => {
            let max_stack = registry.item(incoming.item).max_stack.clamp(1, MAX_STACK_SIZE);
            let room = max_stack.saturating_sub(existing.count);
            let moved = incoming.count.min(room);
            existing.count += moved;
            incoming.count -= moved;
            if incoming.count == 0 {
                (None, moved > 0)
            } else {
                (Some(incoming), moved > 0)
            }
        }
        Some(_) => (Some(incoming), false),
    }
}

/// Base/centre position of an item entity spawned by breaking `block`.
fn block_centre(block: BlockPos) -> WorldVec {
    WorldVec::new(block.x as f64 + 0.5, block.y as f64 + 0.25, block.z as f64 + 0.5)
}

/// Quantises a float to four decimal places for the state hash.
fn quantize(value: f64) -> i64 {
    (value * 10_000.0).round() as i64
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

/// Writes one staged save file and flushes it to stable storage.
fn write_staged(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Hashes one optional stack for the block-entity and equipment sections.
///
/// The encoding matches the inventory fields (resource name, count, optional
/// durability) with an explicit empty/invalid marker so slot boundaries are
/// unambiguous.
fn hash_optional_stack(hasher: &mut StableHasher, registry: &Registry, stack: &Option<ItemStack>) {
    match stack {
        Some(stack) if !stack.is_empty() && (stack.item.0 as usize) < registry.item_len() => {
            hasher.write_u8(1);
            let name = registry.item(stack.item).id;
            hasher.write_u16(name.len() as u16);
            hasher.write(name.as_bytes());
            hasher.write_u8(stack.count);
            match stack.durability {
                Some(remaining) => {
                    hasher.write_u8(1);
                    hasher.write_u16(remaining);
                }
                None => hasher.write_u8(0),
            }
        }
        Some(stack) => {
            // Defensive: invalid stacks cannot be saved, but hashing must not
            // panic. Encode the raw id plus an "unknown" marker.
            hasher.write_u8(2);
            hasher.write_u16(stack.item.0);
            hasher.write_u8(stack.count);
        }
        None => hasher.write_u8(0),
    }
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
    use std::collections::{BTreeMap, HashSet};

    use crate::ItemEntity;
    use crate::items;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use ob_content::{ArmorSlot, BlockId, ItemId, MobKind, RecipeId, Registry};
    use ob_math::{BlockPos, CHUNK_SIZE, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
    use ob_sim::{BlockAction, MAX_HEALTH, MAX_HUNGER, MobAi, PlayerInput, REACH};
    use ob_world::ChunkColumn;
    use ob_worldgen::Generator;

    use super::{
        ARMOR_SLOTS, CHEST_SLOTS, ContainerState, CraftOutcome, DAY_LENGTH_TICKS, FurnaceSlot, FurnaceState, GameMode,
        HOTBAR_SLOTS, Host, HostConfig, ItemStack, MAX_HOSTILE_MOBS, MAX_PASSIVE_MOBS, MOB_ATTACK_REACH, StableHasher,
        apple_drop, armor_damage_reduction, armor_slot_index, block_centre,
    };

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

    /// Ticks until the player rests on the ground (up to ten seconds).
    fn settle_player(host: &mut Host) {
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
            if host.player().on_ground {
                return;
            }
        }
        panic!("player did not settle on the ground");
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

    /// Reflected CRC32C used to re-sign hand-built snapshot fixtures.
    fn crc32c(bytes: &[u8]) -> u32 {
        const POLY: u32 = 0x82F6_3B78;
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ POLY } else { crc >> 1 };
            }
        }
        !crc
    }

    /// Recomputes and rewrites the trailing snapshot checksum.
    fn resign_snapshot(bytes: &mut [u8]) {
        let payload_len = bytes.len() - 4;
        let checksum = crc32c(&bytes[..payload_len]);
        bytes[payload_len..].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Total number of `item` across the nine hotbar slots.
    fn item_count(host: &Host, item: ItemId) -> u32 {
        host.inventory()
            .iter()
            .flatten()
            .filter(|stack| stack.item == item)
            .map(|stack| u32::from(stack.count))
            .sum()
    }

    /// Resolves a base recipe id.
    fn recipe_id(host: &Host, name: &str) -> RecipeId {
        host.registry()
            .recipe_id(name)
            .unwrap_or_else(|| panic!("missing recipe {name}"))
    }

    /// Crafts one unit of the named base recipe.
    fn craft(host: &mut Host, name: &str) -> CraftOutcome {
        let id = recipe_id(host, name);
        host.craft(id)
    }

    /// Places a crafting table next to the player and returns its position.
    fn place_table_near_player(host: &mut Host) -> BlockPos {
        let table = host.registry().id("openblock:crafting_table").expect("table block");
        let spawn = host.player().pos;
        let pos = BlockPos::new(
            spawn.x.floor() as i32 + 2,
            spawn.y.floor() as i32,
            spawn.z.floor() as i32,
        );
        assert!(host.world_mut().set_block(pos, table), "table position is loaded");
        pos
    }

    /// A loaded high-altitude position used for directly installed block
    /// entities, offset along X from the spawn column.
    fn block_entity_pos(host: &Host, offset_x: i32, y: i32) -> BlockPos {
        let spawn = host.player().pos;
        BlockPos::new(spawn.x.floor() as i32 + offset_x, y, spawn.z.floor() as i32)
    }

    /// Installs a furnace block and its record directly (bypasses placement).
    fn install_furnace(host: &mut Host, pos: BlockPos, state: FurnaceState) {
        let furnace = host.registry().id("openblock:furnace").expect("furnace block");
        assert!(host.world_mut().set_block(pos, furnace), "furnace position is loaded");
        host.furnaces.insert(pos, state);
    }

    /// Installs a chest block and its record directly (bypasses placement).
    fn install_chest(host: &mut Host, pos: BlockPos, state: ContainerState) {
        let chest = host.registry().id("openblock:chest").expect("chest block");
        assert!(host.world_mut().set_block(pos, chest), "chest position is loaded");
        host.chests.insert(pos, state);
    }

    /// Gives `count` of `item` and selects the slot it landed in.
    fn give_and_select(host: &mut Host, item: ItemId, count: u8) {
        assert_eq!(host.give_item(item, count), 0, "inventory must have room");
        let slot = host
            .inventory()
            .iter()
            .position(|slot| matches!(slot, Some(stack) if stack.item == item))
            .unwrap_or_else(|| panic!("item {} not found in inventory", host.registry().item(item).id));
        host.set_hotbar_selection(slot);
    }

    /// Finds the hotbar slot holding `item`.
    fn item_slot(host: &Host, item: ItemId) -> usize {
        host.inventory()
            .iter()
            .position(|slot| matches!(slot, Some(stack) if stack.item == item))
            .unwrap_or_else(|| panic!("item {} not found in inventory", host.registry().item(item).id))
    }

    /// A stack of the named item with content default durability.
    fn named_stack(host: &Host, name: &str, count: u8) -> ItemStack {
        let item = host
            .registry()
            .item_id(name)
            .unwrap_or_else(|| panic!("missing item {name}"));
        ItemStack::for_item(item, host.registry().item(item), count)
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
        let dirt_item = host.registry().item_id("openblock:dirt").unwrap();
        let target = high_target(&host);

        // Stone needs a pickaxe: breaking by hand succeeds but drops nothing.
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.broke, Some(stone));
        assert_eq!(outcome.placed, None);
        assert!(outcome.drops.is_empty(), "stone must not drop without a pickaxe");
        assert!(outcome.pickup_overflow.is_empty());
        assert_eq!(host.world().get_block(target), BlockId::AIR);
        assert!(host.inventory().iter().all(Option::is_none));
        assert!(host.item_entities().is_empty(), "no drop means no item entity");

        // Tier-0 blocks still drop by hand, but the loot spawns as an entity
        // at the broken block's centre instead of entering the inventory.
        aim_at(&mut host, target, grass);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.broke, Some(grass));
        assert_eq!(outcome.drops, vec![ItemStack::new(dirt_item, 1)]);
        assert!(outcome.pickup_overflow.is_empty());
        assert!(host.inventory().iter().all(Option::is_none));
        assert_eq!(host.item_entities().len(), 1);
        let entity = &host.item_entities()[0];
        assert_eq!(entity.id, 1);
        assert_eq!(entity.pos, block_centre(target));
        assert_eq!(entity.stack, ItemStack::new(dirt_item, 1));
        assert_eq!(entity.age, 0.0);

        aim_at(&mut host, target, bedrock);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(!outcome.changed, "bedrock must not break in survival");
        assert_eq!(outcome.broke, None);
        assert!(outcome.drops.is_empty());
        assert_eq!(host.item_entities().len(), 1, "no bedrock entity");
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
        assert!(host.item_entities().is_empty(), "creative breaks spawn no entities");

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
    fn full_inventory_break_spawns_entity_and_no_overflow() {
        let (_dir, mut host) = survival_host("full", 13);
        let dirt = host.registry().id("openblock:dirt").unwrap();
        let stone_item = host.registry().item_id("openblock:stone").unwrap();
        let dirt_item = host.registry().item_id("openblock:dirt").unwrap();
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

        // Break no longer grants directly, so a full inventory does not lose
        // loot: the entity exists and `pickup_overflow` stays empty.
        aim_at(&mut host, target, dirt);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.drops, vec![ItemStack::new(dirt_item, 1)]);
        assert!(outcome.pickup_overflow.is_empty());
        assert_eq!(host.item_entities().len(), 1);
        assert_eq!(host.item_entities()[0].stack, ItemStack::new(dirt_item, 1));
        assert!(
            host.inventory()
                .iter()
                .all(|slot| matches!(slot, Some(stack) if stack.count == 64)),
            "the full inventory is unchanged by the break"
        );
    }

    #[test]
    fn crafting_basic_recipes_consume_and_produce() {
        let (_dir, mut host) = survival_host("craft-basic", 41);
        let log = host.registry().item_id("openblock:log").unwrap();
        let planks = host.registry().item_id("openblock:planks").unwrap();
        let sticks = host.registry().item_id("openblock:stick").unwrap();
        let table = host.registry().item_id("openblock:crafting_table").unwrap();

        // Missing ingredients report the shortfall without changing anything.
        let outcome = craft(&mut host, "openblock:planks");
        assert!(!outcome.crafted);
        assert_eq!(outcome.output, None);
        assert_eq!(outcome.message, "need 1 more log");
        assert!(host.inventory().iter().all(Option::is_none));

        assert_eq!(host.give_item(log, 1), 0);
        let outcome = craft(&mut host, "openblock:planks");
        assert!(outcome.crafted);
        assert_eq!(
            outcome.output,
            Some(ItemStack::for_item(planks, host.registry().item(planks), 4))
        );
        assert_eq!(outcome.message, "crafted 4 planks");
        assert_eq!(item_count(&host, log), 0);
        assert_eq!(item_count(&host, planks), 4);
        assert_eq!(host.inventory()[0], Some(ItemStack::new(planks, 4)));

        let outcome = craft(&mut host, "openblock:sticks");
        assert!(outcome.crafted, "{}", outcome.message);
        assert_eq!(outcome.output.unwrap().count, 4);
        assert_eq!(outcome.message, "crafted 4 sticks");
        assert_eq!(item_count(&host, planks), 2);
        assert_eq!(item_count(&host, sticks), 4);

        // Four planks are needed for a table; only two remain.
        let outcome = craft(&mut host, "openblock:crafting_table");
        assert!(!outcome.crafted);
        assert_eq!(outcome.message, "need 2 more planks");
        assert_eq!(item_count(&host, planks), 2);

        assert_eq!(host.give_item(planks, 2), 0);
        let outcome = craft(&mut host, "openblock:crafting_table");
        assert!(outcome.crafted);
        assert_eq!(
            outcome.output,
            Some(ItemStack::for_item(table, host.registry().item(table), 1))
        );
        assert_eq!(outcome.message, "crafted 1 crafting table");
        assert_eq!(item_count(&host, planks), 0);
        assert_eq!(item_count(&host, table), 1);
    }

    #[test]
    fn table_recipes_require_a_nearby_table() {
        let (_dir, mut host) = survival_host("craft-table", 43);
        let planks = host.registry().item_id("openblock:planks").unwrap();
        let sticks = host.registry().item_id("openblock:stick").unwrap();
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();

        assert!(!host.near_crafting_table(), "fresh survival spawn has no table");
        assert_eq!(host.give_item(planks, 3), 0);
        assert_eq!(host.give_item(sticks, 2), 0);

        let counts = host.available_recipes();
        let wood_pick = recipe_id(&host, "openblock:wooden_pickaxe");
        assert_eq!(
            counts.iter().find(|(id, _)| *id == wood_pick).map(|(_, count)| *count),
            Some(0),
            "table recipes report zero without a table"
        );
        // Non-table recipes still report their ingredient-limited count.
        assert_eq!(
            counts
                .iter()
                .find(|(id, _)| *id == recipe_id(&host, "openblock:sticks"))
                .map(|(_, count)| *count),
            Some(1)
        );

        let outcome = craft(&mut host, "openblock:wooden_pickaxe");
        assert!(!outcome.crafted);
        assert_eq!(outcome.message, "needs a crafting table");
        assert_eq!(item_count(&host, planks), 3);
        assert_eq!(item_count(&host, sticks), 2);

        place_table_near_player(&mut host);
        assert!(host.near_crafting_table());
        let counts = host.available_recipes();
        assert_eq!(
            counts.iter().find(|(id, _)| *id == wood_pick).map(|(_, count)| *count),
            Some(1)
        );

        let outcome = craft(&mut host, "openblock:wooden_pickaxe");
        assert!(outcome.crafted, "{}", outcome.message);
        let output = outcome.output.expect("tool output");
        assert_eq!(output.item, pickaxe);
        assert_eq!(output.count, 1);
        assert_eq!(output.durability, Some(59));
        assert_eq!(outcome.message, "crafted 1 wooden pickaxe");
        assert_eq!(item_count(&host, planks), 0);
        assert_eq!(item_count(&host, sticks), 0);
        assert_eq!(item_count(&host, pickaxe), 1);
    }

    #[test]
    fn all_wooden_and_stone_tool_recipes_craft() {
        let (_dir, mut host) = survival_host("craft-tools", 47);
        place_table_near_player(&mut host);
        let planks = host.registry().item_id("openblock:planks").unwrap();
        let sticks = host.registry().item_id("openblock:stick").unwrap();
        let cobble = host.registry().item_id("openblock:cobblestone").unwrap();

        // The wood set consumes 3+3+1+2 planks and 2+2+2+1 sticks exactly.
        assert_eq!(host.give_item(planks, 9), 0);
        assert_eq!(host.give_item(sticks, 7), 0);
        for (name, durability) in [
            ("openblock:wooden_pickaxe", 59),
            ("openblock:wooden_axe", 59),
            ("openblock:wooden_shovel", 59),
            ("openblock:wooden_sword", 59),
        ] {
            let item = host.registry().item_id(name).unwrap();
            let outcome = craft(&mut host, name);
            assert!(outcome.crafted, "{} failed: {}", name, outcome.message);
            let output = outcome.output.expect("tool output");
            assert_eq!(output.item, item);
            assert_eq!(output.count, 1);
            assert_eq!(output.durability, Some(durability));
            assert_eq!(item_count(&host, item), 1);
        }
        assert_eq!(item_count(&host, planks), 0);
        assert_eq!(item_count(&host, sticks), 0);

        // Give each stone tool exactly its ingredients so the nine-slot hotbar
        // always has room for the finished tool.
        for (name, cobble_count, stick_count, durability) in [
            ("openblock:stone_pickaxe", 3, 2, 131),
            ("openblock:stone_axe", 3, 2, 131),
            ("openblock:stone_shovel", 1, 2, 131),
            ("openblock:stone_sword", 2, 1, 131),
        ] {
            assert_eq!(host.give_item(cobble, cobble_count), 0);
            assert_eq!(host.give_item(sticks, stick_count), 0);
            let item = host.registry().item_id(name).unwrap();
            let outcome = craft(&mut host, name);
            assert!(outcome.crafted, "{} failed: {}", name, outcome.message);
            let output = outcome.output.expect("tool output");
            assert_eq!(output.item, item);
            assert_eq!(output.count, 1);
            assert_eq!(output.durability, Some(durability));
            assert_eq!(item_count(&host, item), 1);
        }
        assert_eq!(item_count(&host, cobble), 0);
        assert_eq!(item_count(&host, sticks), 0);
    }

    #[test]
    fn crafting_fails_without_consuming_when_inventory_is_full() {
        let (_dir, mut host) = survival_host("craft-full", 53);
        let stone = host.registry().item_id("openblock:stone").unwrap();
        let planks = host.registry().item_id("openblock:planks").unwrap();
        let sticks = host.registry().item_id("openblock:stick").unwrap();

        // Eight full stone stacks leave one slot holding three planks. The
        // sticks recipe needs only two planks, so consuming them frees no slot;
        // the output cannot fit and nothing may be consumed.
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(stone, 2), 0);
        assert_eq!(host.give_item(planks, 3), 0);
        let before = *host.inventory();

        let outcome = craft(&mut host, "openblock:sticks");
        assert!(!outcome.crafted);
        assert_eq!(outcome.output, None);
        assert_eq!(outcome.message, "inventory full");
        assert_eq!(*host.inventory(), before, "a failed craft consumes nothing");
        assert_eq!(item_count(&host, planks), 3);
        assert_eq!(item_count(&host, sticks), 0);
    }

    #[test]
    fn creative_crafting_bypasses_ingredients_and_tables() {
        let (_dir, mut host) = open_host("craft-creative", 59);
        assert_eq!(host.game_mode(), GameMode::Creative);
        let planks = host.registry().item_id("openblock:planks").unwrap();
        assert!(host.inventory().iter().all(Option::is_none));
        assert_eq!(host.available_recipes().len(), host.registry().recipe_len());
        assert!(
            host.available_recipes().iter().all(|(_, count)| *count == 1),
            "creative reports all recipes as available"
        );

        let outcome = craft(&mut host, "openblock:stone_pickaxe");
        assert!(outcome.crafted, "{}", outcome.message);
        assert_eq!(item_count(&host, planks), 0);
        assert_eq!(
            item_count(&host, host.registry().item_id("openblock:stone_pickaxe").unwrap()),
            1
        );
    }

    #[test]
    fn crafted_table_places_and_enables_nearby_crafting() {
        let (_dir, mut host) = survival_host("craft-place-table", 61);
        let table = host.registry().id("openblock:crafting_table").unwrap();
        let table_item = host.registry().item_id("openblock:crafting_table").unwrap();
        let target = high_target(&host);

        assert_eq!(host.give_item(table_item, 1), 0);
        host.set_hotbar_selection(0);
        // The ray needs a solid hit block to place against; use the target
        // cell itself as the support.
        let stone = host.registry().id("openblock:stone").unwrap();
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(table));
        assert!(outcome.changed);
        assert_eq!(outcome.placed, Some(table));
        assert_eq!(host.selected_stack(), None);
        assert!(host.near_crafting_table());
        assert_eq!(
            host.world().get_block(BlockPos::new(target.x, target.y, target.z + 1)),
            table
        );
    }

    #[test]
    fn tools_speed_mining_drop_loot_and_break_at_zero() {
        let (_dir, mut host) = survival_host("mining-tools", 67);
        let stone = host.registry().id("openblock:stone").unwrap();
        let log = host.registry().id("openblock:log").unwrap();
        let dirt = host.registry().id("openblock:dirt").unwrap();
        let bedrock = host.registry().id("openblock:bedrock").unwrap();
        let cobble = host.registry().item_id("openblock:cobblestone").unwrap();
        let wood_pick = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let stone_pick = host.registry().item_id("openblock:stone_pickaxe").unwrap();
        let wood_axe = host.registry().item_id("openblock:wooden_axe").unwrap();
        let wood_shovel = host.registry().item_id("openblock:wooden_shovel").unwrap();

        // Hand, wood and stone pickaxes: faster with each tier.
        assert_eq!(host.mining_seconds(stone), 1.5);
        assert_eq!(host.give_item(wood_pick, 1), 0);
        host.set_hotbar_selection(0);
        assert_eq!(host.selected_stack().unwrap().durability, Some(59));
        assert_eq!(host.mining_seconds(stone), 0.75);
        assert_eq!(host.give_item(stone_pick, 1), 0);
        host.set_hotbar_selection(1);
        assert_eq!(host.mining_seconds(stone), 0.375);

        // A wooden pickaxe harvests cobblestone and loses one durability.
        host.set_hotbar_selection(0);
        let target = high_target(&host);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.drops, vec![ItemStack::new(cobble, 1)]);
        assert_eq!(outcome.tool_broke, None);
        let tool = host.selected_stack().expect("pickaxe still present");
        assert_eq!(tool.item, wood_pick);
        assert_eq!(tool.durability, Some(58));
        assert_eq!(item_count(&host, cobble), 0, "loot waits on the ground");
        assert_eq!(host.item_entities().len(), 1);
        assert_eq!(host.item_entities()[0].stack, ItemStack::new(cobble, 1));

        // The last durability point breaks the tool and clears the slot.
        host.inventory[0] = Some(ItemStack::new(wood_pick, 1).with_durability(Some(1)));
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(outcome.drops, vec![ItemStack::new(cobble, 1)]);
        assert_eq!(outcome.tool_broke, Some(wood_pick));
        assert_eq!(host.selected_stack(), None);
        assert_eq!(host.item_entities().len(), 2, "one entity per break");
        assert!(
            host.item_entities()
                .iter()
                .all(|entity| entity.stack == ItemStack::new(cobble, 1))
        );

        // Any tool loses durability even against a block it cannot harvest.
        assert_eq!(host.give_item(wood_axe, 1), 0);
        let axe_slot = host
            .inventory()
            .iter()
            .position(|slot| matches!(slot, Some(stack) if stack.item == wood_axe))
            .expect("axe slot");
        host.set_hotbar_selection(axe_slot);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert!(outcome.drops.is_empty(), "an axe cannot harvest stone");
        assert_eq!(host.selected_stack().unwrap().durability, Some(58));

        // Axe and shovel preferences: faster than hand on their blocks.
        assert_eq!(host.mining_seconds(log), 1.0);
        assert_eq!(host.mining_seconds(dirt), 0.5);
        assert_eq!(host.give_item(wood_shovel, 1), 0);
        let shovel_slot = host
            .inventory()
            .iter()
            .position(|slot| matches!(slot, Some(stack) if stack.item == wood_shovel))
            .expect("shovel slot");
        host.set_hotbar_selection(shovel_slot);
        assert_eq!(host.mining_seconds(dirt), 0.25);
        assert_eq!(host.mining_seconds(log), 2.0, "a shovel does not speed up logs");

        // Unbreakable blocks have no mining duration and never drop.
        assert_eq!(host.mining_seconds(bedrock), 0.0);
        aim_at(&mut host, target, bedrock);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(!outcome.changed);
        assert!(outcome.drops.is_empty());
    }

    #[test]
    fn crafting_and_durability_change_the_state_hash() {
        let (_dir, mut host) = survival_host("hash-craft", 71);
        let log = host.registry().item_id("openblock:log").unwrap();
        let wood_pick = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let stone = host.registry().id("openblock:stone").unwrap();
        let target = high_target(&host);

        assert_eq!(host.give_item(log, 2), 0);
        let before = host.state_hash();
        let outcome = craft(&mut host, "openblock:planks");
        assert!(outcome.crafted);
        assert_ne!(host.state_hash(), before, "crafting must change the state hash");

        assert_eq!(host.give_item(wood_pick, 1), 0);
        let slot = host
            .inventory()
            .iter()
            .position(|slot| matches!(slot, Some(stack) if stack.item == wood_pick))
            .unwrap();
        host.set_hotbar_selection(slot);
        let before = host.state_hash();
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert!(outcome.tool_broke.is_none());
        assert_ne!(host.state_hash(), before, "worn durability must change the state hash");
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
    fn give_item_uses_content_stack_sizes_and_full_tool_durability() {
        let (_dir, mut host) = survival_host("stack-sizes", 25);
        let apple = host.registry().item_id("openblock:apple").unwrap();
        let wood_pick = host.registry().item_id("openblock:wooden_pickaxe").unwrap();

        assert_eq!(host.give_item(apple, 20), 0);
        let apple_counts: Vec<u8> = host
            .inventory()
            .iter()
            .flatten()
            .filter(|stack| stack.item == apple)
            .map(|stack| stack.count)
            .collect();
        assert_eq!(apple_counts, vec![16, 4], "apples stack to 16");
        assert_eq!(item_count(&host, apple), 20);

        let stack = host.make_stack(wood_pick, 1).expect("tool stack");
        assert_eq!(stack.durability, Some(59));
        assert_eq!(stack.with_durability(7).durability, Some(7));
        assert_eq!(host.give_item(wood_pick, 2), 0);
        let tool_counts: Vec<u8> = host
            .inventory()
            .iter()
            .flatten()
            .filter(|stack| stack.item == wood_pick)
            .map(|stack| stack.count)
            .collect();
        assert_eq!(tool_counts, vec![1, 1], "tools never stack");
        assert!(
            host.inventory()
                .iter()
                .flatten()
                .filter(|stack| stack.item == wood_pick)
                .all(|stack| stack.durability == Some(59))
        );
    }

    #[test]
    fn inventory_slot_take_set_validate_and_preserve_durability() {
        let (_dir, mut host) = survival_host("inventory-slot-api", 31);
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let apple = host.registry().item_id("openblock:apple").unwrap();
        let damaged = host.make_stack(pickaxe, 1).unwrap().with_durability(7);

        assert_eq!(host.inventory_slot(0), None);
        assert_eq!(host.inventory_slot(HOTBAR_SLOTS), None);
        assert_eq!(host.take_inventory_slot(0), None, "an empty slot has nothing to take");
        assert_eq!(
            host.take_inventory_slot(HOTBAR_SLOTS),
            None,
            "out-of-range take is a no-op"
        );

        assert_eq!(host.set_inventory_slot(0, Some(damaged)), None);
        assert_eq!(host.inventory_slot(0), Some(&damaged));
        assert_eq!(host.inventory()[0], Some(damaged));

        // Setting a slot is an exact swap, never a merge.
        let other = host.make_stack(pickaxe, 1).unwrap().with_durability(3);
        assert_eq!(host.set_inventory_slot(0, Some(other)), Some(damaged));
        assert_eq!(host.inventory_slot(0), Some(&other));

        // A zero count (or None) clears the slot and reports the old stack.
        assert_eq!(host.set_inventory_slot(0, Some(ItemStack::new(apple, 0))), Some(other));
        assert_eq!(host.inventory_slot(0), None);
        assert_eq!(host.set_inventory_slot(0, None), None);

        // Invalid item ids are rejected without disturbing the slot.
        assert_eq!(host.set_inventory_slot(1, Some(damaged)), None);
        let invalid = ItemStack::new(ItemId(9999), 64);
        assert_eq!(
            host.set_inventory_slot(1, Some(invalid)),
            Some(damaged),
            "a rejected write still reports the previous stack"
        );
        assert_eq!(host.inventory_slot(1), Some(&damaged));

        // Slot bounds are hard: a write past the hotbar does nothing.
        assert_eq!(host.set_inventory_slot(HOTBAR_SLOTS, Some(damaged)), None);
        assert_eq!(host.inventory_slot(HOTBAR_SLOTS), None);

        // Counts clamp to the item's max stack, durability to the tool spec.
        let big_apple = ItemStack {
            item: apple,
            count: 200,
            durability: None,
        };
        assert_eq!(host.set_inventory_slot(2, Some(big_apple)), None);
        assert_eq!(host.inventory_slot(2).map(|stack| stack.count), Some(16));
        let over_durable = host.make_stack(pickaxe, 1).unwrap().with_durability(400);
        assert_eq!(host.set_inventory_slot(3, Some(over_durable)), None);
        let full_tool = host.make_stack(pickaxe, 1).unwrap();
        assert_eq!(host.inventory_slot(3), Some(&full_tool));

        // Taking preserves the exact stack (count and durability included).
        assert_eq!(host.take_inventory_slot(1), Some(damaged));
        assert_eq!(host.inventory_slot(1), None);
        assert_eq!(host.take_inventory_slot(1), None, "a second take finds an empty slot");
        let apples = ItemStack::new(apple, 5);
        assert_eq!(host.set_inventory_slot(4, Some(apples)), None);
        assert_eq!(host.take_inventory_slot(4), Some(apples));
    }

    #[test]
    fn insert_inventory_stack_merges_spills_and_preserves_tool_durability() {
        let (_dir, mut host) = survival_host("inventory-insert", 33);
        let stone = host.registry().item_id("openblock:stone").unwrap();
        let apples = host.registry().item_id("openblock:apple").unwrap();
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();

        // A matching stack is topped up before an empty slot is used.
        assert_eq!(host.set_inventory_slot(3, Some(ItemStack::new(stone, 10))), None);
        assert_eq!(host.insert_inventory_stack(ItemStack::new(stone, 5)), None);
        assert_eq!(host.inventory_slot(3).map(|stack| stack.count), Some(15));
        assert_eq!(
            host.inventory_slot(0),
            None,
            "the merge must use the matching stack first"
        );

        // A count larger than the remaining room splits: fill to max, spill out.
        assert_eq!(host.insert_inventory_stack(ItemStack::new(stone, 60)), None);
        assert_eq!(host.inventory_slot(3).map(|stack| stack.count), Some(64));
        assert_eq!(host.inventory_slot(0), Some(&ItemStack::new(stone, 11)));

        // Apple stacks are 16, so a larger insertion spans several slots.
        assert_eq!(host.set_inventory_slot(8, Some(ItemStack::new(apples, 15))), None);
        assert_eq!(host.insert_inventory_stack(ItemStack::new(apples, 20)), None);
        assert_eq!(host.inventory_slot(8).map(|stack| stack.count), Some(16));
        let apple_total: u32 = host
            .inventory()
            .iter()
            .flatten()
            .filter(|stack| stack.item == apples)
            .map(|stack| u32::from(stack.count))
            .sum();
        assert_eq!(apple_total, 35, "the merge must not lose items");

        // A damaged tool goes to an empty slot and keeps its durability; it
        // never merges into the fresh tool sitting elsewhere.
        for slot in 0..HOTBAR_SLOTS {
            let _ = host.take_inventory_slot(slot);
        }
        let damaged = host.make_stack(pickaxe, 1).unwrap().with_durability(7);
        let fresh = host.make_stack(pickaxe, 1).unwrap();
        assert_eq!(host.set_inventory_slot(1, Some(damaged)), None);
        assert_eq!(host.insert_inventory_stack(fresh), None);
        let damaged_slot = host
            .inventory()
            .iter()
            .position(|slot| slot.is_some_and(|stack| stack.item == pickaxe && stack.durability == Some(7)))
            .expect("damaged tool still present");
        let fresh_slot = host
            .inventory()
            .iter()
            .position(|slot| slot.is_some_and(|stack| stack.item == pickaxe && stack.durability == Some(59)))
            .expect("fresh tool still present");
        assert_ne!(fresh_slot, damaged_slot, "tools must never merge");
        assert_eq!(host.inventory()[fresh_slot], Some(fresh));

        // A full inventory returns the leftover with the exact durability.
        for slot in 0..HOTBAR_SLOTS {
            let _ = host.set_inventory_slot(slot, Some(ItemStack::new(stone, 64)));
        }
        assert!(host.inventory().iter().all(|slot| slot.is_some()));
        let leftover_tool = host.make_stack(pickaxe, 1).unwrap().with_durability(5);
        assert_eq!(host.insert_inventory_stack(leftover_tool), Some(leftover_tool));
        let leftover_apples = ItemStack::new(apples, 3);
        assert_eq!(host.insert_inventory_stack(leftover_apples), Some(leftover_apples));
        assert_eq!(
            host.insert_inventory_stack(ItemStack::new(ItemId(9999), 1)),
            Some(ItemStack::new(ItemId(9999), 1)),
            "unknown items are returned unchanged"
        );
    }

    #[test]
    fn pickup_preserves_damaged_tool_durability_from_entity() {
        let (_dir, mut host) = survival_host("pickup-durability", 139);
        settle_player(&mut host);
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let damaged = host.make_stack(pickaxe, 1).unwrap().with_durability(7);
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), damaged);

        let mut picked = false;
        for _ in 0..80 {
            host.tick(&PlayerInput::default());
            if host.item_entities().is_empty() {
                picked = true;
                break;
            }
        }
        assert!(picked, "damaged tool entity was not collected");
        assert_eq!(
            host.inventory_slot(0),
            Some(&damaged),
            "pickup must not repair the tool"
        );
        assert_eq!(host.take_pickup_events(), vec![damaged]);
    }

    #[test]
    fn saved_item_entity_pickup_preserves_damaged_tool_durability() {
        let dir = TempDir::new("pickup-durability-save");
        let config = test_config(dir.path(), 149);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        host.set_game_mode(GameMode::Survival);
        settle_player(&mut host);
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let damaged = host.make_stack(pickaxe, 1).unwrap().with_durability(7);
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), damaged);
        host.save().expect("save succeeds");
        drop(host);

        let mut reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        reopened.set_game_mode(GameMode::Survival);
        assert_eq!(reopened.item_entities().len(), 1);
        assert_eq!(
            reopened.item_entities()[0].stack,
            damaged,
            "items.txt must retain durability"
        );
        let mut picked = false;
        for _ in 0..80 {
            reopened.tick(&PlayerInput::default());
            if reopened.item_entities().is_empty() {
                picked = true;
                break;
            }
        }
        assert!(picked, "reloaded damaged tool entity was not collected");
        assert_eq!(reopened.inventory_slot(0), Some(&damaged));
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
    fn sidecar_reads_are_size_capped_and_utf8_checked() {
        let dir = TempDir::new("sidecar-cap");
        let oversized = dir.path().join("items.txt");
        let file = std::fs::File::create(&oversized).expect("create oversized sidecar");
        file.set_len(super::MAX_SIDECAR_BYTES + 1).expect("extend sidecar");
        drop(file);
        let error = super::read_sidecar(&oversized).expect_err("oversized sidecar must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("limit"), "{error}");

        let non_utf8 = dir.path().join("mobs.txt");
        std::fs::write(&non_utf8, [0xffu8, 0xfe, 0x00]).expect("write non-utf8 sidecar");
        let error = super::read_sidecar(&non_utf8).expect_err("non-utf8 sidecar must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let missing = dir.path().join("inventory.txt");
        let error = super::read_sidecar(&missing).expect_err("missing sidecar must be NotFound");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let directory = dir.path().join("network_players.txt");
        std::fs::create_dir(&directory).expect("create directory in place of a sidecar");
        let error = super::read_sidecar(&directory).expect_err("a directory is not a sidecar");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
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
    fn state_hash_includes_persisted_look_and_flight_and_round_trips_them() {
        let dir = TempDir::new("hash-look");
        let config = test_config(dir.path(), 42);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        converge(&mut host);
        let baseline = host.state_hash();

        host.player_mut().yaw = 0.75;
        assert_ne!(baseline, host.state_hash(), "yaw must participate in the hash");
        host.player_mut().yaw = 0.0;
        host.player_mut().pitch = -0.5;
        assert_ne!(baseline, host.state_hash(), "pitch must participate in the hash");
        host.player_mut().pitch = 0.0;
        host.player_mut().flying = true;
        assert_ne!(baseline, host.state_hash(), "flying must participate in the hash");

        // Store out-of-range angles to prove save/load canonicalises the same
        // values the hash covers.
        host.player_mut().yaw = std::f32::consts::PI + 0.5;
        host.player_mut().pitch = 2.0;
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");
        drop(host);

        let reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        let yaw = reopened.player().yaw;
        let pitch = reopened.player().pitch;
        assert!((yaw - crate::net_players::normalise_angle(std::f32::consts::PI + 0.5)).abs() < 1e-6);
        assert!(pitch <= std::f32::consts::FRAC_PI_2);
        assert!(reopened.player().flying);
        assert_eq!(reopened.state_hash(), hash_before, "look/flight hash must round-trip");
    }

    #[test]
    fn host_open_accepts_a_v3_world_with_a_pre_crafting_fingerprint() {
        let dir = TempDir::new("v3-host");
        let config = test_config(dir.path(), 79);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        converge(&mut host);
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");
        drop(host);

        // Rewrite the world as a v3 snapshot with the v0.0.7 fingerprint, as if
        // it had been saved before planks/cobblestone/crafting_table existed.
        let mut blocks: Vec<_> = registry.iter().map(|(_, block)| block.clone()).collect();
        blocks.truncate(12);
        let old_registry = Registry::from_blocks(blocks).expect("old registry is valid");
        let old_fingerprint = ob_world::registry_fingerprint(&old_registry);
        let world_path = test_world_dir(&dir).join("world.obs");
        let mut bytes = std::fs::read(&world_path).expect("world.obs");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 4);
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        bytes[14..22].copy_from_slice(&old_fingerprint.to_le_bytes());
        resign_snapshot(&mut bytes);
        std::fs::write(&world_path, &bytes).expect("write v3 fixture");

        let mut reopened = Host::open(Arc::clone(&registry), config).expect("v3 world still opens");
        assert!(reopened.world().chunk_count() > 0);
        assert_eq!(reopened.state_hash(), hash_before, "v3 ids must load unchanged");

        // The next save upgrades to the strict v4 format.
        reopened.save().expect("re-save succeeds");
        let upgraded = std::fs::read(&world_path).expect("upgraded world.obs");
        assert_eq!(u16::from_le_bytes([upgraded[4], upgraded[5]]), 4);
        assert_eq!(
            u64::from_le_bytes([
                upgraded[14],
                upgraded[15],
                upgraded[16],
                upgraded[17],
                upgraded[18],
                upgraded[19],
                upgraded[20],
                upgraded[21]
            ]),
            ob_world::registry_fingerprint(&registry),
            "v4 saves record the current fingerprint"
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
        std::fs::remove_file(world_dir.join("items.txt")).expect("remove items file");

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
        assert!(reopened.item_entities().is_empty(), "a missing items.txt is empty");
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
    fn inventory_durability_round_trips_and_legacy_lines_load_full() {
        let dir = TempDir::new("inventory-durability");
        let config = test_config(dir.path(), 73);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        host.set_game_mode(GameMode::Survival);
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        assert_eq!(host.give_item(pickaxe, 1), 0);
        host.inventory[0].as_mut().expect("pickaxe slot").durability = Some(17);
        converge(&mut host);
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");

        let text = std::fs::read_to_string(test_world_dir(&dir).join("inventory.txt")).expect("inventory.txt");
        assert_eq!(text, "0 openblock:wooden_pickaxe 1 17\n");
        assert_eq!(text.split_whitespace().count(), 4);
        drop(host);

        let reopened = Host::open(Arc::clone(&registry), config.clone()).expect("host reopens");
        assert_eq!(reopened.inventory()[0].expect("pickaxe slot").durability, Some(17));
        assert_eq!(reopened.state_hash(), hash_before, "durability must round-trip");
        drop(reopened);

        // v0.0.7 three-field lines load tools at full durability; the fourth
        // field clamps to the spec and counts clamp to the content max_stack.
        std::fs::write(
            test_world_dir(&dir).join("inventory.txt"),
            "0 openblock:wooden_pickaxe 1\n\
             1 openblock:wooden_pickaxe 4 500\n\
             2 openblock:apple 99\n\
             3 openblock:stone 64 7\n",
        )
        .expect("write legacy inventory");
        let reopened = Host::open(Arc::clone(&registry), config).expect("host reopens legacy inventory");
        let apple = reopened.registry().item_id("openblock:apple").unwrap();
        let stone = reopened.registry().item_id("openblock:stone").unwrap();
        let slot0 = reopened.inventory()[0].expect("full tool");
        assert_eq!(slot0.count, 1);
        assert_eq!(slot0.durability, Some(59));
        let slot1 = reopened.inventory()[1].expect("clamped tool");
        assert_eq!(slot1.count, 1);
        assert_eq!(slot1.durability, Some(59));
        assert_eq!(reopened.inventory()[2], Some(ItemStack::new(apple, 16)));
        assert_eq!(reopened.inventory()[3], Some(ItemStack::new(stone, 64)));
        assert_eq!(reopened.inventory()[3].unwrap().durability, None);
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
        assert!(host_a.inventory().iter().all(Option::is_none));
        assert_eq!(host_a.item_entities().len(), 1, "one apple entity");
        let apple_entity = host_a.item_entities()[0].clone();
        assert_eq!(apple_entity.pos, block_centre(apple_pos));
        assert_eq!(apple_entity.stack, ItemStack::new(apple, 1));

        aim_at(&mut host_a, plain_pos, leaves);
        let plain_outcome = host_a.apply_action_with_outcome(&break_action());
        assert!(plain_outcome.changed);
        assert!(plain_outcome.drops.is_empty(), "leaves have no guaranteed drop");
        assert_eq!(host_a.item_entities().len(), 1, "plain leaves spawn no entity");

        // A second host with the same seed agrees for the same block positions,
        // including the deterministic spawn impulse and entity id.
        let (_dir_b, mut host_b) = survival_host("apple-b", SEED);
        assert_eq!(host_b.world().seed(), SEED);
        aim_at(&mut host_b, apple_pos, leaves);
        assert_eq!(
            host_b.apply_action_with_outcome(&break_action()).drops,
            apple_outcome.drops
        );
        assert_eq!(host_b.item_entities(), &[apple_entity]);
        aim_at(&mut host_b, plain_pos, leaves);
        assert_eq!(
            host_b.apply_action_with_outcome(&break_action()).drops,
            plain_outcome.drops
        );
    }

    #[test]
    fn item_spawn_velocity_is_deterministic_and_bounded() {
        const SEED: u64 = 0x1E3_5EED;
        let (_dir_a, mut host_a) = open_host("spawn-velocity-a", SEED);
        let (_dir_b, mut host_b) = open_host("spawn-velocity-b", SEED);
        let dirt = host_a.registry().item_id("openblock:dirt").unwrap();
        let pos = WorldVec::new(3.5, 200.25, -7.5);
        let stack = ItemStack::new(dirt, 3);

        assert_eq!(host_a.spawn_item_entity(pos, stack), 1);
        assert_eq!(host_b.spawn_item_entity(pos, stack), 1);
        assert_eq!(
            host_a.item_entities(),
            host_b.item_entities(),
            "same seed, id and block position must produce the same impulse"
        );
        let entity = &host_a.item_entities()[0];
        assert_eq!(entity.pos, pos, "spawn keeps the requested centre");
        assert_eq!(entity.stack, stack);
        assert_eq!(entity.age, 0.0);
        assert!(entity.velocity.x.abs() <= 1.5);
        assert!(entity.velocity.z.abs() <= 1.5);
        assert!(
            entity.velocity.x.hypot(entity.velocity.z) <= 1.5 + 1e-12,
            "horizontal impulse exceeds 1.5 m/s"
        );
        assert!(
            (2.5..4.0).contains(&entity.velocity.y),
            "upward impulse {} outside 2.5..4",
            entity.velocity.y
        );

        let (_dir_c, mut host_c) = open_host("spawn-velocity-c", SEED ^ 0xFFFF);
        host_c.spawn_item_entity(pos, stack);
        assert_ne!(
            host_c.item_entities()[0].velocity,
            entity.velocity,
            "the impulse must depend on the world seed"
        );
    }

    #[test]
    fn item_physics_settles_on_a_flat_platform_and_stays() {
        let (_dir, mut host) = open_host("item-physics", 0xF1A7);
        converge(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let stone = host.registry().id("openblock:stone").unwrap();
        let spawn = host.player().pos;
        let base = BlockPos::new(spawn.x.floor() as i32 + 4, 150, spawn.z.floor() as i32);

        // A 3x3 stone platform high above the terrain.
        for dx in -1..=1 {
            for dz in -1..=1 {
                assert!(
                    host.world_mut()
                        .set_block(BlockPos::new(base.x + dx, base.y, base.z + dz), stone),
                    "platform position must be loaded"
                );
            }
        }
        let start = WorldVec::new(base.x as f64 + 0.5, base.y as f64 + 3.0, base.z as f64 + 0.5);
        host.spawn_item_entity(start, ItemStack::new(dirt, 1));
        let expected_y = base.y as f64 + 1.0 + 0.1;
        let mut landed = false;
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
            let entity = &host.item_entities()[0];
            if entity.velocity == WorldVec::ZERO && (entity.pos.y - expected_y).abs() < 1e-9 {
                landed = true;
                break;
            }
            assert!(
                entity.pos.y > base.y as f64 + 1.0,
                "item fell through the platform at {:?}",
                entity.pos
            );
        }
        assert!(landed, "item never settled on the platform");
        let settled = host.item_entities()[0].pos;
        assert!(
            (settled.y - expected_y).abs() < 1e-9,
            "item rests at {:?}, expected y {expected_y}",
            settled
        );
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
        }
        let entity = &host.item_entities()[0];
        assert_eq!(entity.pos, settled, "settled item drifted");
        assert_eq!(entity.velocity, WorldVec::ZERO);
        assert!(entity.age > 0.5);
        assert_eq!(entity.stack, ItemStack::new(dirt, 1));
    }

    #[test]
    fn pickup_after_delay_enters_inventory_emits_event_and_removes_entity() {
        let (_dir, mut host) = survival_host("pickup", 103);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        let pos = WorldVec::new(feet.x, feet.y + 0.1, feet.z);
        assert_eq!(host.spawn_item_entity(pos, ItemStack::new(dirt, 2)), 1);

        for _ in 0..9 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1, "pickup must wait for the delay");
        assert!(host.take_pickup_events().is_empty());
        assert_eq!(item_count(&host, dirt), 0);

        let mut picked = false;
        for _ in 0..3 {
            host.tick(&PlayerInput::default());
            if host.item_entities().is_empty() {
                picked = true;
                break;
            }
        }
        assert!(picked, "entity was not collected after the delay");
        assert_eq!(item_count(&host, dirt), 2);
        assert_eq!(host.take_pickup_events(), vec![ItemStack::new(dirt, 2)]);
        assert!(host.take_pickup_events().is_empty(), "the queue drains");
    }

    #[test]
    fn pickup_radius_attracts_items_from_a_few_blocks_away() {
        let (_dir, mut host) = survival_host("pickup-radius", 127);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        let start = WorldVec::new(feet.x + 2.2, feet.y + 0.1, feet.z);
        host.spawn_item_entity(start, ItemStack::new(dirt, 1));

        // The pickup delay still applies: no collection during the first ticks.
        for _ in 0..9 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1, "delay must still gate pickup");
        let before = host.item_entities()[0].pos.distance(items::player_chest(host.player()));

        // Once allowed, the item is attracted toward the player and collected.
        let mut collected = false;
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
            if host.item_entities().is_empty() {
                collected = true;
                break;
            }
        }
        assert!(collected, "item inside the pickup radius was not collected");
        assert_eq!(item_count(&host, dirt), 1);
        assert_eq!(host.take_pickup_events(), vec![ItemStack::new(dirt, 1)]);
        assert!(before <= items::ITEM_PICKUP_RADIUS + 1e-9);
    }

    #[test]
    fn items_outside_the_pickup_radius_are_left_alone() {
        let (_dir, mut host) = survival_host("pickup-radius-out", 131);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        let far = WorldVec::new(feet.x + 5.0, feet.y + 0.1, feet.z);
        host.spawn_item_entity(far, ItemStack::new(dirt, 1));

        for _ in 0..40 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1, "far item must not be collected");
        assert_eq!(item_count(&host, dirt), 0);
        assert!(host.take_pickup_events().is_empty());

        // The boundary check is a true radius around the player's chest.
        let player = host.player();
        let stack = ItemStack::new(dirt, 1);
        let inside = ItemEntity {
            id: 99,
            pos: items::player_chest(player) + WorldVec::new(2.9, 0.0, 0.0),
            velocity: WorldVec::ZERO,
            stack,
            age: 1.0,
        };
        let outside = ItemEntity {
            pos: items::player_chest(player) + WorldVec::new(0.0, 0.0, 3.1),
            ..inside.clone()
        };
        assert!(items::can_pick_up(player, &inside));
        assert!(!items::can_pick_up(player, &outside));
    }

    #[test]
    fn full_inventory_keeps_entity_and_emits_no_event() {
        let (_dir, mut host) = survival_host("pickup-full", 107);
        settle_player(&mut host);
        let stone = host.registry().item_id("openblock:stone").unwrap();
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(stone, 66), 0);
        let before = *host.inventory();
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), ItemStack::new(dirt, 1));

        for _ in 0..40 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1);
        assert_eq!(host.item_entities()[0].stack, ItemStack::new(dirt, 1));
        assert_eq!(*host.inventory(), before, "a full inventory is unchanged");
        assert!(
            host.take_pickup_events().is_empty(),
            "no items entered, so there is no pickup event"
        );
    }

    #[test]
    fn partial_pickup_reduces_entity_stack_and_reports_added_count() {
        let (_dir, mut host) = survival_host("pickup-partial", 109);
        settle_player(&mut host);
        let stone = host.registry().item_id("openblock:stone").unwrap();
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(stone, 255), 0);
        assert_eq!(host.give_item(dirt, 61), 0);
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), ItemStack::new(dirt, 5));

        let mut events = Vec::new();
        for _ in 0..15 {
            host.tick(&PlayerInput::default());
            events.extend(host.take_pickup_events());
        }
        assert_eq!(events, vec![ItemStack::new(dirt, 3)], "only three fitted");
        assert_eq!(item_count(&host, dirt), 64);
        assert_eq!(host.item_entities().len(), 1, "the remainder stays");
        assert_eq!(host.item_entities()[0].stack, ItemStack::new(dirt, 2));
    }

    #[test]
    fn creative_pickup_is_disabled() {
        let (_dir, mut host) = open_host("pickup-creative", 113);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), ItemStack::new(dirt, 1));
        for _ in 0..40 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1);
        assert!(host.inventory().iter().all(Option::is_none));
        assert!(host.take_pickup_events().is_empty());
    }

    #[test]
    fn dead_survival_player_does_not_pick_up() {
        let (_dir, mut host) = survival_host("pickup-dead", 117);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        host.player_mut().dead = true;
        host.player_mut().health = 0.0;
        host.spawn_item_entity(WorldVec::new(feet.x, feet.y + 0.1, feet.z), ItemStack::new(dirt, 1));
        for _ in 0..40 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.item_entities().len(), 1, "dead players cannot collect items");
        assert!(host.inventory().iter().all(Option::is_none));
        assert!(host.take_pickup_events().is_empty());
    }

    #[test]
    fn items_despawn_below_the_void_and_after_five_minutes() {
        let (_dir, mut host) = open_host("despawn-items", 127);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        host.spawn_item_entity(WorldVec::new(feet.x + 3.0, -70.0, feet.z), ItemStack::new(dirt, 1));
        host.tick(&PlayerInput::default());
        assert!(host.item_entities().is_empty(), "void items despawn");

        let high = WorldVec::new(feet.x + 3.0, feet.y + 5.0, feet.z);
        assert_eq!(host.spawn_item_entity(high, ItemStack::new(dirt, 1)), 2);
        host.items[0].age = 300.0 - 0.01;
        host.tick(&PlayerInput::default());
        assert!(host.item_entities().is_empty(), "aged items despawn");
    }

    #[test]
    fn state_hash_includes_item_entity_state_sorted_by_id() {
        let (_dir, mut host) = open_host("hash-items", 131);
        settle_player(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let feet = host.player().pos;
        let pos = WorldVec::new(feet.x + 3.0, feet.y + 5.0, feet.z);
        let before = host.state_hash();
        assert_eq!(host.spawn_item_entity(pos, ItemStack::new(dirt, 1)), 1);
        let hash = host.state_hash();
        assert_ne!(hash, before, "spawning an entity changes the hash");

        host.items[0].pos.x += 1.0;
        assert_ne!(host.state_hash(), hash, "position is hashed");
        host.items[0].pos.x -= 1.0;
        assert_eq!(host.state_hash(), hash, "restoring the position restores the hash");

        host.items[0].velocity.y -= 1.0;
        assert_ne!(host.state_hash(), hash, "velocity is hashed");
        host.items[0].velocity.y += 1.0;

        host.items[0].age += 1.0;
        assert_ne!(host.state_hash(), hash, "age is hashed");
        host.items[0].age -= 1.0;
        assert_eq!(host.state_hash(), hash, "restoring the state restores the hash");

        host.spawn_item_entity(pos, ItemStack::new(dirt, 2));
        let ordered = host.state_hash();
        host.items.reverse();
        assert_eq!(host.state_hash(), ordered, "the hash sorts entities by id");
    }

    #[test]
    fn save_reload_preserves_item_entities_ids_and_state_hash() {
        let dir = TempDir::new("items-persistence");
        let config = test_config(dir.path(), 137);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        host.set_game_mode(GameMode::Survival);
        converge(&mut host);
        let dirt = host.registry().item_id("openblock:dirt").unwrap();
        let apple = host.registry().item_id("openblock:apple").unwrap();
        let pickaxe = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let spawn = host.player().pos;
        let base = BlockPos::new(spawn.x.floor() as i32 + 2, 180, spawn.z.floor() as i32);
        let centre = WorldVec::new(base.x as f64 + 0.5, base.y as f64 + 0.25, base.z as f64 + 0.5);

        assert_eq!(host.spawn_item_entity(centre, ItemStack::new(dirt, 2)), 1);
        assert_eq!(host.spawn_item_entity(centre, ItemStack::new(apple, 1)), 2);
        assert_eq!(
            host.spawn_item_entity(centre, ItemStack::new(pickaxe, 1).with_durability(Some(17))),
            3
        );
        for _ in 0..3 {
            host.tick(&PlayerInput::default());
        }
        let entities_before = host.item_entities().to_vec();
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");

        let items_text = std::fs::read_to_string(test_world_dir(&dir).join("items.txt")).expect("items.txt");
        assert_eq!(items_text.lines().count(), 3);
        for line in items_text.lines() {
            assert_eq!(
                line.split_whitespace().count(),
                11,
                "items.txt lines carry eleven fields: {line}"
            );
        }
        let ids: Vec<u64> = items_text
            .lines()
            .map(|line| line.split_whitespace().next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3], "lines are sorted by entity id");
        assert!(items_text.contains("openblock:apple"));
        assert!(items_text.contains("openblock:wooden_pickaxe"));
        let tool_fields: Vec<&str> = items_text.lines().nth(2).unwrap().split_whitespace().collect();
        assert_eq!(tool_fields[9], "17", "tool durability is stored");
        drop(host);

        let mut reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        assert_eq!(reopened.item_entities(), entities_before.as_slice());
        assert_eq!(
            reopened.state_hash(),
            hash_before,
            "item entities must round-trip through the state hash"
        );
        assert_eq!(
            reopened.spawn_item_entity(centre, ItemStack::new(dirt, 1)),
            4,
            "next_item_id is max + 1 after load"
        );
    }

    #[test]
    fn items_loader_skips_malformed_lines_and_clamps_values() {
        let dir = TempDir::new("items-load");
        let config = test_config(dir.path(), 139);
        let registry = Arc::new(Registry::base());
        let host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        drop(host);

        std::fs::write(
            test_world_dir(&dir).join("items.txt"),
            "5 1.5 80.25 -2.5 0.0 0.0 0.0 openblock:dirt 2 - 0.5\n\
             5 2.5 80.25 -2.5 0.0 0.0 0.0 openblock:dirt 1 - 0.0\n\
             6 1.5 80.25\n\
             7 NaN 80.25 -2.5 0.0 0.0 0.0 openblock:dirt 1 - 0.0\n\
             8 1.5 80.25 -2.5 0.0 0.0 0.0 openblock:not_an_item 1 - 0.0\n\
             9 1.5 80.25 -2.5 0.0 0.0 0.0 openblock:dirt 999 - -1.0\n\
             10 1.5 -1000 1000000000.0 0.0 0.0 0.0 openblock:apple 0 - 500\n",
        )
        .expect("write items.txt");

        let mut reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        let dirt = reopened.registry().item_id("openblock:dirt").unwrap();
        let entities = reopened.item_entities();
        assert_eq!(entities.len(), 3, "malformed, unknown and duplicate lines are skipped");
        assert_eq!(entities[0].id, 5);
        assert_eq!(entities[0].pos, WorldVec::new(1.5, 80.25, -2.5));
        assert_eq!(entities[0].velocity, WorldVec::ZERO);
        assert_eq!(entities[0].stack, ItemStack::new(dirt, 2));
        assert_eq!(entities[0].age, 0.5);
        assert_eq!(entities[1].id, 9);
        assert_eq!(entities[1].stack.count, 64, "count clamps to max_stack");
        assert_eq!(entities[1].age, 0.0, "negative age clamps to zero");
        assert_eq!(entities[2].id, 10);
        assert_eq!(entities[2].stack.count, 1, "zero count clamps to one");
        assert_eq!(entities[2].pos.x, 1.5);
        assert_eq!(entities[2].pos.y, -64.0);
        assert_eq!(entities[2].pos.z, 30_000_000.0);
        assert_eq!(entities[2].age, 300.0);
        assert_eq!(
            reopened.spawn_item_entity(WorldVec::new(0.5, 80.0, 0.5), ItemStack::new(dirt, 1)),
            11,
            "next_item_id is max + 1 after a load"
        );
    }

    #[test]
    fn furnace_and_chest_place_as_empty_records_and_break_with_contents() {
        let (_dir, mut host) = survival_host("block-entities-place", 401);
        converge(&mut host);
        let stone = host.registry().id("openblock:stone").unwrap();
        let furnace = host.registry().id("openblock:furnace").unwrap();
        let chest = host.registry().id("openblock:chest").unwrap();
        let furnace_item = host.registry().item_id("openblock:furnace").unwrap();
        let chest_item = host.registry().item_id("openblock:chest").unwrap();
        let raw_iron = named_stack(&host, "openblock:raw_iron", 2);
        let coal = named_stack(&host, "openblock:coal", 1);
        let ingot = named_stack(&host, "openblock:iron_ingot", 1);
        let dirt = named_stack(&host, "openblock:dirt", 5);
        let sticks = named_stack(&host, "openblock:stick", 3);

        // Place a furnace with the selected block item.
        let target = high_target(&host);
        give_and_select(&mut host, furnace_item, 1);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(furnace));
        assert!(outcome.changed);
        assert_eq!(outcome.placed, Some(furnace));
        let furnace_pos = BlockPos::new(target.x, target.y, target.z + 1);
        assert_eq!(host.world().get_block(furnace_pos), furnace);
        assert_eq!(
            host.furnace_at(furnace_pos),
            Some(&FurnaceState::default()),
            "placing a furnace creates an empty record"
        );

        // Fill it through the block-entity API, then break it.
        assert_eq!(host.put_furnace_slot(furnace_pos, FurnaceSlot::Input, raw_iron), None);
        assert_eq!(host.put_furnace_slot(furnace_pos, FurnaceSlot::Fuel, coal), None);
        host.furnace_at_mut(furnace_pos).unwrap().output = Some(ingot);
        aim_at(&mut host, furnace_pos, furnace);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.broke, Some(furnace));
        assert_eq!(
            outcome.drops,
            vec![ItemStack::new(furnace_item, 1), raw_iron, coal, ingot],
            "block loot comes first, then furnace contents input/fuel/output"
        );
        assert!(host.furnace_at(furnace_pos).is_none(), "the record is removed");
        assert_eq!(host.world().get_block(furnace_pos), BlockId::AIR);
        let entity_stacks: Vec<ItemStack> = host.item_entities().iter().map(|entity| entity.stack).collect();
        assert_eq!(entity_stacks, outcome.drops);
        assert!(
            host.item_entities()
                .iter()
                .all(|entity| entity.pos == block_centre(furnace_pos))
        );

        // Place and break a chest with contents.
        let target = BlockPos::new(target.x + 3, target.y, target.z);
        give_and_select(&mut host, chest_item, 1);
        aim_at(&mut host, target, stone);
        let outcome = host.apply_action_with_outcome(&place_action(chest));
        assert!(outcome.changed);
        let chest_pos = BlockPos::new(target.x, target.y, target.z + 1);
        assert_eq!(host.chest_at(chest_pos), Some(&ContainerState::default()));
        assert_eq!(host.put_chest_slot(chest_pos, 0, dirt), None);
        assert_eq!(host.put_chest_slot(chest_pos, 26, sticks), None);
        assert_eq!(host.take_chest_slot(chest_pos, 0), Some(dirt));
        assert_eq!(host.put_chest_slot(chest_pos, 0, dirt), None);
        aim_at(&mut host, chest_pos, chest);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.broke, Some(chest));
        assert_eq!(outcome.drops, vec![ItemStack::new(chest_item, 1), dirt, sticks]);
        assert!(host.chest_at(chest_pos).is_none());
        let entity_stacks: Vec<ItemStack> = host.item_entities().iter().map(|entity| entity.stack).collect();
        assert_eq!(entity_stacks.len(), 7);
        assert_eq!(&entity_stacks[4..], outcome.drops.as_slice());

        // Creative breaks discard container contents and drop no loot.
        let (_dir2, mut creative) = open_host("block-entities-creative", 403);
        converge(&mut creative);
        let pos = block_entity_pos(&creative, 0, 150);
        let mut state = ContainerState::default();
        state.slots[0] = Some(dirt);
        install_chest(&mut creative, pos, state);
        aim_at(&mut creative, pos, chest);
        let outcome = creative.apply_action_with_outcome(&break_action());
        assert!(outcome.changed);
        assert_eq!(creative.world().get_block(pos), BlockId::AIR);
        assert!(outcome.drops.is_empty());
        assert!(creative.chest_at(pos).is_none());
        assert!(creative.item_entities().is_empty());
    }

    #[test]
    fn furnace_smelts_consumes_fuel_and_pauses() {
        let (_dir, mut host) = open_host("furnace-smelt", 405);
        converge(&mut host);
        let pos = block_entity_pos(&host, 0, 150);
        let raw_iron = named_stack(&host, "openblock:raw_iron", 3);
        let coal = named_stack(&host, "openblock:coal", 1);
        install_furnace(
            &mut host,
            pos,
            FurnaceState {
                input: Some(raw_iron),
                fuel: Some(coal),
                ..FurnaceState::default()
            },
        );
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).expect("furnace record").clone();
        assert_eq!(state.output, Some(named_stack(&host, "openblock:iron_ingot", 1)));
        assert_eq!(state.input.unwrap().count, 2);
        assert_eq!(state.fuel, None, "one coal is one fuel item");
        assert_eq!(state.burn_total, 1_600);
        assert_eq!(state.burn_remaining, 1_400);
        assert_eq!(state.cook_progress, 0);

        // Full output: no cooking and no new fuel is lit.
        let pos = block_entity_pos(&host, 1, 150);
        let blocked_state = FurnaceState {
            input: Some(named_stack(&host, "openblock:raw_iron", 2)),
            fuel: Some(named_stack(&host, "openblock:coal", 1)),
            output: Some(named_stack(&host, "openblock:iron_ingot", 64)),
            ..FurnaceState::default()
        };
        install_furnace(&mut host, pos, blocked_state);
        for _ in 0..50 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).expect("blocked furnace").clone();
        assert_eq!(state.cook_progress, 0);
        assert_eq!(state.burn_remaining, 0);
        assert_eq!(state.fuel.unwrap().count, 1, "fuel waits while output is full");

        // Freeing the output resumes cooking immediately.
        assert_eq!(host.take_furnace_output(pos).unwrap().count, 64);
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).expect("resumed furnace").clone();
        assert_eq!(state.output.unwrap().count, 1);
        assert_eq!(state.input.unwrap().count, 1);
        assert_eq!(state.fuel, None);
        assert_eq!(state.burn_remaining, 1_400);

        // Removing the input pauses progress but the lit fuel keeps burning.
        assert_eq!(host.take_furnace_slot(pos, FurnaceSlot::Input).unwrap().count, 1);
        let progress = host.furnace_at(pos).unwrap().cook_progress;
        for _ in 0..100 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).unwrap();
        assert_eq!(state.cook_progress, progress, "progress is held, not reset");
        assert_eq!(state.burn_remaining, 1_300, "a lit furnace consumes its burn");

        // Reinserting input resumes and completes the held smelt.
        assert_eq!(
            host.insert_furnace_input(pos, named_stack(&host, "openblock:raw_iron", 1)),
            None
        );
        for _ in 0..(200 - u32::from(progress)) {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).unwrap();
        assert_eq!(state.output.unwrap().count, 2);
        assert_eq!(state.input, None);
        assert_eq!(state.cook_progress, 0);

        // Input that has no recipe never lights fuel.
        let pos = block_entity_pos(&host, 2, 150);
        let idle_state = FurnaceState {
            input: Some(named_stack(&host, "openblock:dirt", 4)),
            fuel: Some(named_stack(&host, "openblock:coal", 2)),
            ..FurnaceState::default()
        };
        install_furnace(&mut host, pos, idle_state);
        for _ in 0..50 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).unwrap();
        assert_eq!(state.fuel.unwrap().count, 2);
        assert_eq!(state.burn_remaining, 0);
        assert_eq!(state.cook_progress, 0);

        // Exactly one item is consumed from a larger fuel stack per burn.
        let pos = block_entity_pos(&host, 3, 150);
        let stacked_state = FurnaceState {
            input: Some(named_stack(&host, "openblock:raw_iron", 1)),
            fuel: Some(named_stack(&host, "openblock:coal", 2)),
            ..FurnaceState::default()
        };
        install_furnace(&mut host, pos, stacked_state);
        for _ in 0..200 {
            host.tick(&PlayerInput::default());
        }
        let state = host.furnace_at(pos).unwrap();
        assert_eq!(state.fuel.unwrap().count, 1);
        assert_eq!(state.burn_remaining, 1_400);
        assert_eq!(state.output.unwrap().count, 1);
    }

    #[test]
    fn furnace_slot_api_clamps_merges_and_rejects_output_writes() {
        let (_dir, mut host) = open_host("furnace-api", 407);
        converge(&mut host);
        let pos = block_entity_pos(&host, 0, 150);
        install_furnace(&mut host, pos, FurnaceState::default());
        let raw_iron = named_stack(&host, "openblock:raw_iron", 64);
        let coal = named_stack(&host, "openblock:coal", 1);
        let iron = named_stack(&host, "openblock:iron_ingot", 1);

        assert_eq!(host.put_furnace_slot(pos, FurnaceSlot::Output, iron), None);
        assert!(host.furnace_at(pos).unwrap().output.is_none(), "output is take-only");

        assert_eq!(host.put_furnace_slot(pos, FurnaceSlot::Input, raw_iron), None);
        assert_eq!(host.furnace_at(pos).unwrap().input, Some(raw_iron));
        let ten = named_stack(&host, "openblock:raw_iron", 10);
        assert_eq!(
            host.insert_furnace_input(pos, ten),
            Some(ten),
            "a full input slot rejects the whole insertion"
        );
        assert_eq!(host.furnace_at(pos).unwrap().input.unwrap().count, 64);

        let mut oversized = raw_iron;
        oversized.count = 200;
        assert_eq!(
            host.put_furnace_slot(pos, FurnaceSlot::Input, oversized),
            Some(raw_iron)
        );
        assert_eq!(host.furnace_at(pos).unwrap().input.unwrap().count, 64, "count clamps");

        assert_eq!(host.put_furnace_slot(pos, FurnaceSlot::Input, coal), Some(raw_iron));
        assert_eq!(
            host.put_furnace_slot(pos, FurnaceSlot::Input, ItemStack::new(coal.item, 0)),
            Some(coal)
        );
        assert_eq!(host.furnace_at(pos).unwrap().input, None, "zero count clears the slot");

        assert_eq!(host.insert_furnace_fuel(pos, coal), None);
        let more = named_stack(&host, "openblock:coal", 5);
        assert_eq!(host.furnace_at(pos).unwrap().fuel.unwrap().count, 1);
        assert_eq!(host.insert_furnace_fuel(pos, more), None);
        assert_eq!(host.furnace_at(pos).unwrap().fuel.unwrap().count, 6);
        assert_eq!(host.take_furnace_output(pos), None);
        assert_eq!(host.furnace_slot(pos, FurnaceSlot::Fuel).unwrap().count, 6);
    }

    #[test]
    fn chest_slot_api_clamps_and_max_stacks() {
        let (_dir, mut host) = open_host("chest-api", 409);
        converge(&mut host);
        let pos = block_entity_pos(&host, 0, 150);
        install_chest(&mut host, pos, ContainerState::default());
        let dirt = named_stack(&host, "openblock:dirt", 64);
        let mut oversized = dirt;
        oversized.count = 200;

        assert_eq!(host.put_chest_slot(pos, 0, oversized), None);
        assert_eq!(host.chest_at(pos).unwrap().slots[0].unwrap().count, 64);
        assert_eq!(host.put_chest_slot(pos, 0, dirt), Some(dirt), "put swaps, never merges");
        assert_eq!(
            host.put_chest_slot(pos, 26, named_stack(&host, "openblock:stick", 3)),
            None
        );
        assert_eq!(host.take_chest_slot(pos, 0), Some(dirt));
        assert_eq!(host.take_chest_slot(pos, 0), None);
        assert!(!host.chest_at(pos).unwrap().is_empty());
        assert_eq!(
            host.chest_at(pos).unwrap().contents(),
            vec![named_stack(&host, "openblock:stick", 3)]
        );
        assert_eq!(host.put_chest_slot(pos, CHEST_SLOTS, dirt), None);
        assert_eq!(host.take_chest_slot(pos, CHEST_SLOTS), None);
        assert_eq!(host.put_chest_slot(pos, 0, ItemStack::new(dirt.item, 0)), None);
        assert_eq!(host.chest_at(pos).unwrap().slots[0], None);
    }

    #[test]
    fn block_entities_and_equipment_save_reload_and_hash() {
        let dir = TempDir::new("block-entities-save");
        let config = test_config(dir.path(), 411);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        host.set_game_mode(GameMode::Survival);
        converge(&mut host);

        let furnace_pos = block_entity_pos(&host, 0, 150);
        let chest_pos = block_entity_pos(&host, 1, 151);
        let mut chest = ContainerState::default();
        chest.slots[0] = Some(named_stack(&host, "openblock:dirt", 10));
        chest.slots[26] =
            Some(ItemStack::new(named_stack(&host, "openblock:wooden_pickaxe", 1).item, 1).with_durability(Some(12)));
        let furnace_state = FurnaceState {
            input: Some(named_stack(&host, "openblock:raw_iron", 2)),
            fuel: Some(named_stack(&host, "openblock:coal", 3)),
            output: Some(named_stack(&host, "openblock:iron_ingot", 1)),
            burn_remaining: 777,
            burn_total: 1_600,
            cook_progress: 123,
        };
        install_furnace(&mut host, furnace_pos, furnace_state.clone());
        let lower_furnace = block_entity_pos(&host, -1, 150);
        install_furnace(&mut host, lower_furnace, FurnaceState::default());
        install_chest(&mut host, chest_pos, chest.clone());
        let helmet = named_stack(&host, "openblock:iron_helmet", 1);
        assert_eq!(host.give_item(helmet.item, 1), 0);
        assert!(host.equip_armor(item_slot(&host, helmet.item)));
        assert_eq!(host.armor_defense(), 2);
        let equipment_before = *host.equipment();
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");

        let block_text =
            std::fs::read_to_string(test_world_dir(&dir).join("block_entities.txt")).expect("block_entities.txt");
        let lines: Vec<&str> = block_text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with(&format!(
                "furnace {} {} {}",
                lower_furnace.x, lower_furnace.y, lower_furnace.z
            )),
            "furnace records are sorted by (x, y, z): {}",
            lines[0]
        );
        assert!(lines[1].starts_with(&format!("furnace {} {}", furnace_pos.x, furnace_pos.y)));
        assert!(lines[1].contains("777 1600 123"));
        assert!(lines[2].starts_with("chest "), "chests are written after furnaces");
        assert!(lines[2].contains("0:openblock:dirt,10"));
        assert!(lines[2].contains("26:openblock:wooden_pickaxe,1,12"));
        let equipment_text =
            std::fs::read_to_string(test_world_dir(&dir).join("equipment.txt")).expect("equipment.txt");
        assert_eq!(equipment_text, "helmet openblock:iron_helmet 165\n");
        drop(host);

        let mut reopened = Host::open(Arc::clone(&registry), config).expect("host reopens");
        assert_eq!(reopened.furnace_at(furnace_pos), Some(&furnace_state));
        assert_eq!(reopened.furnace_at(lower_furnace), Some(&FurnaceState::default()));
        assert_eq!(reopened.chest_at(chest_pos), Some(&chest));
        assert_eq!(*reopened.equipment(), equipment_before);
        assert_eq!(
            reopened.state_hash(),
            hash_before,
            "block entities and armor must round-trip through the state hash"
        );

        // Hash coverage: ticking progress changes it; restoring it restores.
        let hash = reopened.state_hash();
        reopened.furnace_at_mut(furnace_pos).unwrap().cook_progress += 1;
        assert_ne!(reopened.state_hash(), hash);
        reopened.furnace_at_mut(furnace_pos).unwrap().cook_progress -= 1;
        assert_eq!(reopened.state_hash(), hash);
        reopened.chest_at_mut(chest_pos).unwrap().slots[1] = Some(named_stack(&reopened, "openblock:stick", 2));
        assert_ne!(reopened.state_hash(), hash);

        // The reloaded mid-smelt state continues from exactly where it stopped:
        // 123 progress + 77 ticks completes the smelt.
        for _ in 0..77 {
            reopened.tick(&PlayerInput::default());
        }
        let continued = reopened.furnace_at(furnace_pos).expect("furnace");
        assert_eq!(continued.cook_progress, 0);
        assert_eq!(continued.output.unwrap().count, 2);
        assert_eq!(continued.input.unwrap().count, 1);
        assert_eq!(continued.burn_remaining, 700);
    }

    #[test]
    fn block_entity_loader_skips_malformed_and_prunes_stale_records() {
        let dir = TempDir::new("block-entities-load");
        let config = test_config(dir.path(), 413);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        converge(&mut host);
        let good = block_entity_pos(&host, 0, 150);
        let stale = block_entity_pos(&host, 1, 150);
        let chest_pos = block_entity_pos(&host, 2, 150);
        let furnace_block = host.registry().id("openblock:furnace").unwrap();
        let chest_block = host.registry().id("openblock:chest").unwrap();
        assert!(host.world_mut().set_block(good, furnace_block));
        assert!(host.world_mut().set_block(chest_pos, chest_block));
        host.save().expect("saves the blocks without records");
        drop(host);

        let text = format!(
            "furnace {} {} {} 120 1600 7 openblock:raw_iron,2 openblock:coal,1 openblock:iron_ingot,1\n\
             furnace {} {} {} 10 100 1 - - -\n\
             furnace {} {} {} 0 0 0 - - -\n\
             chest {} {} {} 0:openblock:dirt,10 1:openblock:not_an_item,2 2:openblock:stick,3\n\
             chest {} {} {} 3:openblock:stone,1\n\
             furnace 1 2\n\
             bogus 1 2 3\n",
            good.x,
            good.y,
            good.z,
            stale.x,
            stale.y,
            stale.z,
            good.x,
            good.y,
            good.z,
            chest_pos.x,
            chest_pos.y,
            chest_pos.z,
            chest_pos.x,
            chest_pos.y,
            chest_pos.z
        );
        std::fs::write(test_world_dir(&dir).join("block_entities.txt"), text).expect("write sidecar");

        let reopened = Host::open(Arc::clone(&registry), config.clone()).expect("host reopens");
        let furnace = reopened.furnace_at(good).expect("valid furnace loads");
        assert_eq!(furnace.burn_remaining, 120);
        assert_eq!(furnace.burn_total, 1_600);
        assert_eq!(furnace.cook_progress, 7);
        assert_eq!(furnace.input.unwrap().count, 2);
        assert_eq!(furnace.fuel.unwrap().count, 1);
        assert!(furnace.output.is_some());
        assert!(reopened.furnace_at(stale).is_none(), "stale record is pruned");
        assert_eq!(reopened.furnaces().count(), 1);
        let chest = reopened.chest_at(chest_pos).expect("valid chest loads");
        assert_eq!(chest.slots[0].unwrap().count, 10);
        assert_eq!(chest.slots[1], None, "unknown item tokens are skipped");
        assert_eq!(chest.slots[2].unwrap().count, 3);
        assert_eq!(reopened.chests().count(), 1);

        // A missing sidecar behaves like a pre-slice save.
        std::fs::remove_file(test_world_dir(&dir).join("block_entities.txt")).unwrap();
        drop(reopened);
        let reopened = Host::open(Arc::clone(&registry), config).expect("host reopens without sidecar");
        assert_eq!(reopened.furnaces().count(), 0);
        assert_eq!(reopened.chests().count(), 0);
        assert!(reopened.state_hash() != 0);
    }

    #[test]
    fn equipment_equip_swap_unequip_and_defense() {
        let (_dir, mut host) = survival_host("equipment", 415);
        let helmet = named_stack(&host, "openblock:iron_helmet", 1);
        let chest = named_stack(&host, "openblock:iron_chestplate", 1);
        let legs = named_stack(&host, "openblock:iron_leggings", 1);
        let boots = named_stack(&host, "openblock:iron_boots", 1);
        let leather_helmet = named_stack(&host, "openblock:leather_helmet", 1);
        for stack in [helmet, chest, legs, boots, leather_helmet] {
            assert_eq!(host.give_item(stack.item, 1), 0);
        }
        assert_eq!(host.armor_defense(), 0);
        let mut failed = false;
        for slot in 0..HOTBAR_SLOTS {
            if host.inventory()[slot].is_some() {
                continue;
            }
            if !host.equip_armor(slot) {
                failed = true;
            }
        }
        assert!(failed, "empty inventory slots cannot be equipped");

        for stack in [helmet, chest, legs, boots] {
            assert!(
                host.equip_armor(item_slot(&host, stack.item)),
                "equip {}",
                host.registry().item(stack.item).id
            );
        }
        assert_eq!(host.armor_defense(), 15, "2 + 6 + 5 + 2");
        assert_eq!(host.equipment()[0], Some(helmet));
        assert_eq!(host.equipment()[1], Some(chest));
        assert_eq!(host.equipment()[2], Some(legs));
        assert_eq!(host.equipment()[3], Some(boots));
        assert_eq!(host.equipment_slot(ArmorSlot::Chestplate), Some(chest));
        assert_eq!(armor_slot_index(ArmorSlot::Boots), 3);

        // Swapping a helmet returns the old one to the freed slot.
        assert!(host.equip_armor(item_slot(&host, leather_helmet.item)));
        assert_eq!(host.equipment()[0], Some(leather_helmet));
        assert_eq!(
            item_count(&host, helmet.item),
            1,
            "the old helmet returns to the inventory"
        );
        assert_eq!(
            host.inventory()
                .iter()
                .flatten()
                .find(|stack| stack.item == helmet.item)
                .and_then(|stack| stack.durability),
            Some(165)
        );
        assert_eq!(host.armor_defense(), 14);

        assert!(host.unequip_armor(ArmorSlot::Boots));
        assert_eq!(host.equipment()[3], None);
        assert_eq!(item_count(&host, boots.item), 1);
        assert!(!host.unequip_armor(ArmorSlot::Boots));

        // Armor is inert in creative mode but may still be equipped.
        host.set_game_mode(GameMode::Creative);
        assert_eq!(host.armor_defense(), 0);
        assert!(host.unequip_armor(ArmorSlot::Chestplate));
        assert_eq!(host.armor_defense(), 0);
        assert!(host.equip_armor(item_slot(&host, chest.item)));

        // A full inventory keeps an unequip from happening.
        host.set_game_mode(GameMode::Survival);
        let stone = host.registry().item_id("openblock:stone").unwrap();
        for slot in host.inventory.iter_mut() {
            if slot.is_none() {
                *slot = Some(ItemStack::new(stone, 64));
            }
        }
        assert_eq!(host.inventory().iter().flatten().count(), HOTBAR_SLOTS);
        assert!(!host.unequip_armor(ArmorSlot::Chestplate));
        assert_eq!(host.equipment()[1], Some(chest), "failed unequip changes nothing");
    }

    #[test]
    fn armor_mitigates_damage_wears_and_breaks() {
        let (_dir, mut host) = survival_host("armor-damage", 417);
        assert_eq!(armor_damage_reduction(0), 0.0);
        assert!((armor_damage_reduction(15) - 0.6).abs() < f32::EPSILON);
        assert!((armor_damage_reduction(21) - 0.8).abs() < f32::EPSILON);

        let helmet = named_stack(&host, "openblock:iron_helmet", 1);
        let chest = named_stack(&host, "openblock:iron_chestplate", 1);
        let legs = named_stack(&host, "openblock:iron_leggings", 1);
        let boots = named_stack(&host, "openblock:iron_boots", 1);
        for stack in [helmet, chest, legs, boots] {
            assert_eq!(host.give_item(stack.item, 1), 0);
            assert!(host.equip_armor(item_slot(&host, stack.item)));
        }
        assert_eq!(host.armor_defense(), 15);
        host.player_mut().health = MAX_HEALTH;
        let outcome = host.damage_with_outcome(10.0);
        assert_eq!(outcome.raw, 10.0);
        assert!((outcome.mitigated - 4.0).abs() < 1e-5, "{outcome:?}");
        assert!((outcome.dealt - 4.0).abs() < 1e-5);
        assert!((outcome.absorbed - 6.0).abs() < 1e-5);
        assert!((host.player().health - (MAX_HEALTH - 4.0)).abs() < 1e-5);
        assert!(outcome.armor_broke.is_empty());
        for (index, stack) in host.equipment().iter().enumerate() {
            let stack = stack.expect("still equipped");
            let full = [165u16, 240, 225, 195][index];
            assert_eq!(stack.durability, Some(full - 1));
        }

        // A piece at one durability breaks and stops contributing defense.
        host.equipment[0] = Some(helmet.with_durability(Some(1)));
        let outcome = host.damage_with_outcome(1.0);
        assert_eq!(outcome.armor_broke, vec![ArmorSlot::Helmet]);
        assert_eq!(host.equipment()[0], None);
        assert_eq!(host.armor_defense(), 13);
        assert_eq!(host.equipment()[1].unwrap().durability, Some(238));

        // Creative and dead players take no damage and wear nothing.
        host.set_game_mode(GameMode::Creative);
        let before = *host.equipment();
        let outcome = host.damage_with_outcome(5.0);
        assert_eq!(outcome.dealt, 0.0);
        assert!(outcome.armor_broke.is_empty());
        assert_eq!(*host.equipment(), before);
        host.set_game_mode(GameMode::Survival);
        host.player_mut().dead = true;
        host.player_mut().health = 0.0;
        let outcome = host.damage_with_outcome(5.0);
        assert_eq!(outcome.dealt, 0.0);
        assert_eq!(*host.equipment(), before);

        // Without armor the same hit removes its full amount.
        host.player_mut().dead = false;
        host.player_mut().health = MAX_HEALTH;
        host.equipment = [None; ARMOR_SLOTS];
        host.damage(5.0);
        assert_eq!(host.player().health, MAX_HEALTH - 5.0);
    }

    #[test]
    fn armor_reduces_fall_and_starvation_damage() {
        let (_dir, mut host) = survival_host("armor-vitals", 419);
        settle_player(&mut host);
        let spawn = host.player().pos;

        let fall_loss = |host: &mut Host, spawn: WorldVec| -> f32 {
            {
                let player = host.player_mut();
                player.pos = WorldVec::new(spawn.x, spawn.y + 8.0, spawn.z);
                player.velocity = WorldVec::ZERO;
                player.on_ground = false;
                player.health = MAX_HEALTH;
                player.hunger = MAX_HUNGER;
                player.dead = false;
            }
            for _ in 0..200 {
                host.tick(&PlayerInput::default());
                if host.player().on_ground {
                    break;
                }
            }
            assert!(host.player().on_ground, "fall did not land");
            MAX_HEALTH - host.player().health
        };

        let unarmored = fall_loss(&mut host, spawn);
        assert!(unarmored > 0.0, "the test fall must hurt");

        // Starvation: one point every four seconds with hunger at zero.
        {
            let player = host.player_mut();
            player.health = MAX_HEALTH;
            player.hunger = 0.0;
            player.starvation_timer = 0.0;
            player.dead = false;
            player.pos = spawn;
            player.velocity = WorldVec::ZERO;
            player.on_ground = true;
        }
        for _ in 0..100 {
            host.tick(&PlayerInput::default());
        }
        let unarmored_starve = MAX_HEALTH - host.player().health;
        assert!((unarmored_starve - 1.0).abs() < 1e-4, "{unarmored_starve}");

        let chest = named_stack(&host, "openblock:iron_chestplate", 1);
        assert_eq!(host.give_item(chest.item, 1), 0);
        assert!(host.equip_armor(item_slot(&host, chest.item)));
        assert_eq!(host.armor_defense(), 6);

        let armored = fall_loss(&mut host, spawn);
        assert!(armored < unarmored, "armor must reduce fall damage");
        assert!(
            (armored - unarmored * 0.76).abs() < 0.02,
            "fall reduction: {armored} vs {unarmored}"
        );

        {
            let player = host.player_mut();
            player.health = MAX_HEALTH;
            player.hunger = 0.0;
            player.starvation_timer = 0.0;
            player.dead = false;
            player.pos = spawn;
            player.velocity = WorldVec::ZERO;
            player.on_ground = true;
        }
        let durability_before = host.equipment()[1].unwrap().durability.unwrap();
        for _ in 0..100 {
            host.tick(&PlayerInput::default());
        }
        let armored_starve = MAX_HEALTH - host.player().health;
        assert!(
            (armored_starve - 0.76).abs() < 1e-3,
            "starvation reduction: {armored_starve}"
        );
        assert_eq!(
            host.equipment()[1].unwrap().durability,
            Some(durability_before - 1),
            "starvation wear uses the armor path"
        );
    }

    #[test]
    fn equipment_persists_and_loader_handles_malformed_lines() {
        let dir = TempDir::new("equipment-persist");
        let config = test_config(dir.path(), 421);
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(Arc::clone(&registry), config.clone()).expect("host opens");
        converge(&mut host);
        let helmet = named_stack(&host, "openblock:iron_helmet", 1);
        let boots = named_stack(&host, "openblock:iron_boots", 1);
        assert_eq!(host.give_item(helmet.item, 1), 0);
        assert!(host.equip_armor(item_slot(&host, helmet.item)));
        assert_eq!(host.give_item(boots.item, 1), 0);
        assert!(host.equip_armor(item_slot(&host, boots.item)));
        host.equipment[0] = Some(helmet.with_durability(Some(150)));
        let spare = named_stack(&host, "openblock:iron_chestplate", 1).with_durability(Some(77));
        host.inventory[8] = Some(spare);
        let equipment_before = *host.equipment();
        let hash_before = host.state_hash();
        host.save().expect("save");
        let text = std::fs::read_to_string(test_world_dir(&dir).join("equipment.txt")).unwrap();
        assert_eq!(
            text,
            "helmet openblock:iron_helmet 150\nboots openblock:iron_boots 195\n"
        );
        let inventory_text = std::fs::read_to_string(test_world_dir(&dir).join("inventory.txt")).unwrap();
        assert!(
            inventory_text.contains("openblock:iron_chestplate 1 77"),
            "unequipped armor durability is stored in inventory.txt: {inventory_text}"
        );
        drop(host);

        let reopened = Host::open(Arc::clone(&registry), config.clone()).expect("reopen");
        assert_eq!(*reopened.equipment(), equipment_before);
        assert_eq!(reopened.inventory()[8], Some(spare));
        assert_eq!(reopened.state_hash(), hash_before);

        // Missing file: a pre-slice save loads with no armor.
        std::fs::remove_file(test_world_dir(&dir).join("equipment.txt")).unwrap();
        drop(reopened);
        let reopened = Host::open(Arc::clone(&registry), config.clone()).expect("reopen without equipment");
        assert_eq!(*reopened.equipment(), [None; ARMOR_SLOTS]);
        assert_ne!(reopened.state_hash(), hash_before);
        drop(reopened);

        // Malformed lines are skipped; valid ones still load.
        std::fs::write(
            test_world_dir(&dir).join("equipment.txt"),
            "helmet openblock:iron_helmet 999\n\
             chestplate openblock:leather_helmet 30\n\
             legs openblock:not_an_item 10\n\
             boots openblock:iron_boots\n\
             boots openblock:leather_boots 20\n\
             nope openblock:iron_boots 10\n",
        )
        .expect("write malformed equipment");
        let reopened = Host::open(Arc::clone(&registry), config).expect("reopen malformed equipment");
        assert_eq!(
            reopened.equipment()[0].unwrap().durability,
            Some(165),
            "clamped to full"
        );
        assert_eq!(reopened.equipment()[1], None, "wrong slot is rejected");
        assert_eq!(reopened.equipment()[2], None, "unknown item is rejected");
        assert_eq!(
            reopened.equipment()[3].unwrap().item,
            boots.item,
            "first boots line wins"
        );
        assert_eq!(
            reopened.equipment()[3].unwrap().durability,
            Some(195),
            "missing field defaults to full"
        );
    }

    #[test]
    fn ore_drops_respect_tiers_and_iron_tools_work() {
        let (_dir, mut host) = survival_host("ores", 423);
        converge(&mut host);
        let target = high_target(&host);
        let coal_ore = host.registry().id("openblock:coal_ore").unwrap();
        let iron_ore = host.registry().id("openblock:iron_ore").unwrap();
        let copper_ore = host.registry().id("openblock:copper_ore").unwrap();
        let coal = host.registry().item_id("openblock:coal").unwrap();
        let raw_iron = host.registry().item_id("openblock:raw_iron").unwrap();
        let raw_copper = host.registry().item_id("openblock:raw_copper").unwrap();
        let wood_pick = host.registry().item_id("openblock:wooden_pickaxe").unwrap();
        let stone_pick = host.registry().item_id("openblock:stone_pickaxe").unwrap();
        let iron_pick = host.registry().item_id("openblock:iron_pickaxe").unwrap();

        // Coal ore needs a tier-1 pickaxe; a hand breaks it without loot.
        aim_at(&mut host, target, coal_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.broke, Some(coal_ore));
        assert!(outcome.drops.is_empty(), "hand mining a coal ore drops nothing");

        give_and_select(&mut host, wood_pick, 1);
        aim_at(&mut host, target, coal_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.drops, vec![ItemStack::new(coal, 1)]);
        assert_eq!(host.selected_stack().unwrap().durability, Some(58));

        // Iron and copper need tier 2; wood is too weak.
        aim_at(&mut host, target, iron_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert!(outcome.drops.is_empty(), "a wooden pickaxe cannot harvest iron");
        give_and_select(&mut host, stone_pick, 1);
        aim_at(&mut host, target, iron_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.drops, vec![ItemStack::new(raw_iron, 1)]);
        aim_at(&mut host, target, copper_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.drops, vec![ItemStack::new(raw_copper, 1)]);

        // Iron tools are tier 3 and faster than stone.
        give_and_select(&mut host, iron_pick, 1);
        assert_eq!(host.selected_stack().unwrap().durability, Some(251));
        assert!((host.mining_seconds(iron_ore) - 0.5).abs() < 1e-6);
        aim_at(&mut host, target, iron_ore);
        let outcome = host.apply_action_with_outcome(&break_action());
        assert_eq!(outcome.drops, vec![ItemStack::new(raw_iron, 1)]);
        assert_eq!(host.selected_stack().unwrap().durability, Some(250));
    }

    #[test]
    fn new_foods_restore_their_hunger_values() {
        let (_dir, mut host) = survival_host("new-foods", 425);
        for (name, points) in [
            ("openblock:raw_beef", 3.0f32),
            ("openblock:cooked_beef", 8.0),
            ("openblock:raw_porkchop", 3.0),
            ("openblock:cooked_porkchop", 8.0),
        ] {
            let item = host.registry().item_id(name).unwrap();
            assert_eq!(host.registry().item(item).food, Some(points as u8));
            give_and_select(&mut host, item, 1);
            host.player_mut().hunger = 10.0;
            host.player_mut().health = MAX_HEALTH;
            assert!(host.eat_selected(), "{name} must be edible");
            assert!(
                (host.player().hunger - (10.0 + points).min(MAX_HUNGER)).abs() < 1e-4,
                "{name}: {}",
                host.player().hunger
            );
            assert_eq!(host.selected_stack(), None, "{name} is consumed");
        }
    }

    #[test]
    fn furnace_chest_iron_and_armor_recipes_craft() {
        let (_dir, mut host) = survival_host("new-recipes", 427);
        place_table_near_player(&mut host);
        let cobble = host.registry().item_id("openblock:cobblestone").unwrap();
        let planks = host.registry().item_id("openblock:planks").unwrap();
        let sticks = host.registry().item_id("openblock:stick").unwrap();
        let ingots = host.registry().item_id("openblock:iron_ingot").unwrap();
        let leather = host.registry().item_id("openblock:leather").unwrap();
        assert_eq!(host.give_item(cobble, 8), 0);
        assert_eq!(host.give_item(planks, 8), 0);
        assert_eq!(host.give_item(sticks, 4), 0);
        assert_eq!(host.give_item(ingots, 11), 0);
        assert_eq!(host.give_item(leather, 5), 0);

        let outcome = craft(&mut host, "openblock:furnace");
        assert!(outcome.crafted, "{}", outcome.message);
        assert_eq!(
            item_count(&host, host.registry().item_id("openblock:furnace").unwrap()),
            1
        );
        let outcome = craft(&mut host, "openblock:chest");
        assert!(outcome.crafted, "{}", outcome.message);
        assert_eq!(
            item_count(&host, host.registry().item_id("openblock:chest").unwrap()),
            1
        );
        let outcome = craft(&mut host, "openblock:iron_pickaxe");
        assert!(outcome.crafted, "{}", outcome.message);
        let outcome = craft(&mut host, "openblock:iron_chestplate");
        assert!(outcome.crafted, "{}", outcome.message);
        let outcome = craft(&mut host, "openblock:leather_helmet");
        assert!(outcome.crafted, "{}", outcome.message);

        let pickaxe = host.registry().item_id("openblock:iron_pickaxe").unwrap();
        let chestplate = host.registry().item_id("openblock:iron_chestplate").unwrap();
        let helmet = host.registry().item_id("openblock:leather_helmet").unwrap();
        assert_eq!(item_count(&host, pickaxe), 1);
        assert_eq!(item_count(&host, chestplate), 1);
        assert_eq!(item_count(&host, helmet), 1);
        assert!(
            host.inventory()
                .iter()
                .flatten()
                .any(|stack| stack.item == chestplate && stack.durability == Some(240)),
            "crafted armor starts at full durability"
        );
        assert!(
            host.inventory()
                .iter()
                .flatten()
                .any(|stack| stack.item == helmet && stack.durability == Some(55))
        );
        assert!(
            host.inventory()
                .iter()
                .flatten()
                .any(|stack| stack.item == pickaxe && stack.durability == Some(251))
        );
    }

    #[test]
    fn remote_edits_create_and_remove_block_entity_records() {
        let (_dir, mut host) = open_host("remote-block-entities", 429);
        converge(&mut host);
        let pos = block_entity_pos(&host, 0, 150);
        let furnace = host.registry().id("openblock:furnace").unwrap();
        let chest = host.registry().id("openblock:chest").unwrap();
        let raw_iron = named_stack(&host, "openblock:raw_iron", 2);

        assert!(host.apply_remote_edit(pos, furnace));
        assert_eq!(host.world().get_block(pos), furnace);
        assert_eq!(host.furnace_at(pos), Some(&FurnaceState::default()));
        assert_eq!(host.put_furnace_slot(pos, FurnaceSlot::Input, raw_iron), None);
        assert!(host.apply_remote_edit(pos, BlockId::AIR));
        assert!(host.furnace_at(pos).is_none(), "remote break removes the record");
        assert!(host.item_entities().is_empty(), "remote edits have no loot authority");

        assert!(host.apply_remote_edit(pos, chest));
        assert_eq!(host.chest_at(pos), Some(&ContainerState::default()));
        let dirt = named_stack(&host, "openblock:dirt", 3);
        assert_eq!(host.put_chest_slot(pos, 0, dirt), None);
        assert!(host.apply_remote_edit(pos, BlockId::AIR));
        assert!(host.chest_at(pos).is_none());
        assert!(host.item_entities().is_empty());
    }

    /// Places a grass platform (and clears three cells above it) in a square.
    fn grass_platform(host: &mut Host, center_x: i32, center_z: i32, y: i32, radius: i32) {
        let grass = host.registry().id_or_air("openblock:grass");
        for z in center_z - radius..=center_z + radius {
            for x in center_x - radius..=center_x + radius {
                assert!(
                    host.world_mut().set_block(BlockPos::new(x, y, z), grass),
                    "platform ({x}, {y}, {z}) is not in a loaded chunk"
                );
                for dy in 1..=3 {
                    assert!(host.world_mut().set_block(BlockPos::new(x, y + dy, z), BlockId::AIR));
                }
            }
        }
    }

    /// Teleports the host player and clears motion, keeping the camera level.
    fn teleport_player(host: &mut Host, pos: WorldVec) {
        let player = host.player_mut();
        player.pos = pos;
        player.velocity = WorldVec::ZERO;
        player.on_ground = false;
        player.yaw = 0.0;
        player.pitch = 0.0;
    }

    /// Streams until the area around the current player position is resident.
    fn converge_here(host: &mut Host) {
        for _ in 0..200 {
            if host.ensure_streaming(256) == 0 {
                return;
            }
        }
        panic!("streaming did not converge");
    }

    /// Horizontal distance used by the natural-spawn assertions.
    fn horizontal_distance(a: WorldVec, b: WorldVec) -> f64 {
        let dx = a.x - b.x;
        let dz = a.z - b.z;
        (dx * dx + dz * dz).sqrt()
    }

    #[test]
    fn natural_passive_spawning_respects_caps_distance_and_grass() {
        let (_dir, mut host) = open_host("mob-natural-passive", 42);
        host.set_render_distance_chunks(6);
        let spawn = host.player().pos;
        let center_x = spawn.x.floor() as i32;
        let center_z = spawn.z.floor() as i32;
        let platform_y = 200;
        teleport_player(
            &mut host,
            WorldVec::new(center_x as f64 + 0.5, platform_y as f64 + 1.0, center_z as f64 + 0.5),
        );
        converge_here(&mut host);
        grass_platform(&mut host, center_x, center_z, platform_y, 44);
        converge_here(&mut host);

        let grass = host.registry().id_or_air("openblock:grass");
        let mut observed = HashSet::new();
        for _ in 0..600 {
            let before: HashSet<u64> = host.mobs().iter().map(|entity| entity.id).collect();
            host.tick(&PlayerInput::default());
            for entity in host.mobs() {
                if before.contains(&entity.id) || !observed.insert(entity.id) {
                    continue;
                }
                let spec = host.registry().mob(entity.mob.kind);
                assert!(!spec.hostile, "a hostile mob spawned during daylight");
                let distance = horizontal_distance(entity.mob.pos, host.player().pos);
                assert!(
                    (12.0..=40.0).contains(&distance),
                    "{:?} spawned {distance} m from the player",
                    entity.mob.kind
                );
                let floor = BlockPos::new(
                    entity.mob.pos.x.floor() as i32,
                    entity.mob.pos.y.floor() as i32 - 1,
                    entity.mob.pos.z.floor() as i32,
                );
                assert_eq!(
                    host.world().get_block(floor),
                    grass,
                    "{:?} did not spawn on grass at {floor:?}",
                    entity.mob.kind
                );
                assert_eq!(host.world().get_block(floor.offset(0, 1, 0)), BlockId::AIR);
                assert_eq!(host.world().get_block(floor.offset(0, 2, 0)), BlockId::AIR);
                assert!(
                    host.world().chunk(floor.chunk()).is_some(),
                    "spawn column must be loaded"
                );
            }
        }
        assert_eq!(
            host.passive_mob_count(),
            MAX_PASSIVE_MOBS,
            "passive cap did not fill ({} mobs)",
            host.mob_count()
        );
        assert_eq!(host.hostile_mob_count(), 0);
        assert_eq!(observed.len(), MAX_PASSIVE_MOBS);
        assert!(host.mobs().windows(2).all(|pair| pair[0].id < pair[1].id));
    }

    /// Carves a 3-high stone-floored room around the player for underground
    /// spawn tests.
    fn carve_cave_room(host: &mut Host, center_x: i32, center_z: i32, floor_y: i32, radius: i32) {
        let stone = host.registry().id_or_air("openblock:stone");
        for z in center_z - radius..=center_z + radius {
            for x in center_x - radius..=center_x + radius {
                assert!(host.world_mut().set_block(BlockPos::new(x, floor_y, z), stone));
                for dy in 1..=3 {
                    assert!(
                        host.world_mut()
                            .set_block(BlockPos::new(x, floor_y + dy, z), BlockId::AIR)
                    );
                }
            }
        }
    }

    #[test]
    fn natural_hostile_spawning_fills_the_night_cap_underground() {
        let (_dir, mut host) = open_host("mob-natural-hostile", 43);
        host.set_render_distance_chunks(6);
        let spawn = host.player().pos;
        let center_x = spawn.x.floor() as i32;
        let center_z = spawn.z.floor() as i32;

        teleport_player(
            &mut host,
            WorldVec::new(center_x as f64 + 0.5, 21.0, center_z as f64 + 0.5),
        );
        converge_here(&mut host);
        carve_cave_room(&mut host, center_x, center_z, 20, 44);
        host.tick_count = 13_000;
        assert!(host.is_night());

        let mut observed = HashSet::new();
        for _ in 0..600 {
            let before: HashSet<u64> = host.mobs().iter().map(|entity| entity.id).collect();
            host.tick(&PlayerInput::default());
            for entity in host.mobs() {
                if before.contains(&entity.id) || !observed.insert(entity.id) {
                    continue;
                }
                let spec = host.registry().mob(entity.mob.kind);
                assert!(spec.hostile);
                assert_eq!(entity.mob.kind, MobKind::Zombie);
                let distance = horizontal_distance(entity.mob.pos, host.player().pos);
                assert!(
                    (16.0..=48.0).contains(&distance),
                    "{:?} spawned {distance} m from the player",
                    entity.mob.kind
                );
                let floor = BlockPos::new(
                    entity.mob.pos.x.floor() as i32,
                    entity.mob.pos.y.floor() as i32 - 1,
                    entity.mob.pos.z.floor() as i32,
                );
                assert!(host.world().is_solid(floor), "zombie floor is not solid");
                assert_eq!(host.world().get_block(floor.offset(0, 1, 0)), BlockId::AIR);
                assert_eq!(host.world().get_block(floor.offset(0, 2, 0)), BlockId::AIR);
            }
        }
        assert_eq!(
            host.hostile_mob_count(),
            MAX_HOSTILE_MOBS,
            "hostile cap did not fill ({} mobs)",
            host.mob_count()
        );
        assert_eq!(host.passive_mob_count(), 0, "animals must not spawn at night");
        assert_eq!(observed.len(), MAX_HOSTILE_MOBS);
    }

    #[test]
    fn underground_hostiles_spawn_even_in_daylight() {
        let (_dir, mut host) = open_host("mob-hostile-depth-day", 52);
        host.set_render_distance_chunks(6);
        let spawn = host.player().pos;
        let center_x = spawn.x.floor() as i32;
        let center_z = spawn.z.floor() as i32;
        teleport_player(
            &mut host,
            WorldVec::new(center_x as f64 + 0.5, 21.0, center_z as f64 + 0.5),
        );
        converge_here(&mut host);
        carve_cave_room(&mut host, center_x, center_z, 20, 44);
        host.tick_count = 1_000;
        assert!(host.is_day());
        let mut observed = HashSet::new();
        for _ in 0..100 {
            let before: HashSet<u64> = host.mobs().iter().map(|entity| entity.id).collect();
            host.tick(&PlayerInput::default());
            for entity in host.mobs() {
                if before.contains(&entity.id) || !host.registry().mob(entity.mob.kind).hostile {
                    continue;
                }
                observed.insert(entity.id);
                let distance = horizontal_distance(entity.mob.pos, host.player().pos);
                assert!((16.0..=48.0).contains(&distance), "hostile at {distance} m");
                let floor = BlockPos::new(
                    entity.mob.pos.x.floor() as i32,
                    entity.mob.pos.y.floor() as i32 - 1,
                    entity.mob.pos.z.floor() as i32,
                );
                assert!(host.world().is_solid(floor));
                assert_eq!(host.world().get_block(floor.offset(0, 1, 0)), BlockId::AIR);
                assert_eq!(host.world().get_block(floor.offset(0, 2, 0)), BlockId::AIR);
            }
        }
        assert!(
            !observed.is_empty(),
            "a player below Y=40 must attract hostiles during the day"
        );
        assert!(host.is_day());
    }

    #[test]
    fn zombies_burn_in_daylight_and_die_without_drops() {
        let (_dir, mut host) = open_host("mob-sunburn", 44);
        host.set_render_distance_chunks(6);
        let spawn = host.player().pos;
        let center_x = spawn.x.floor() as i32;
        let center_z = spawn.z.floor() as i32;
        let platform_y = 210;
        teleport_player(
            &mut host,
            WorldVec::new(center_x as f64 + 0.5, platform_y as f64 + 1.0, center_z as f64 + 0.5),
        );
        converge_here(&mut host);
        grass_platform(&mut host, center_x, center_z, platform_y, 16);
        converge_here(&mut host);
        host.tick_count = 1_000;
        assert!(host.is_day());

        let exposed_id = host
            .spawn_mob(
                MobKind::Zombie,
                WorldVec::new(center_x as f64 + 5.5, platform_y as f64 + 1.0, center_z as f64 + 0.5),
            )
            .expect("zombie spawns on the platform");
        let exposed_health = host.mob_entity(exposed_id).unwrap().mob.health;
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
        }
        let exposed = host
            .mob_entity(exposed_id)
            .expect("zombie still alive after one burn second");
        assert!(
            exposed.mob.health < exposed_health - 0.9,
            "burn lost only {} health",
            exposed_health - exposed.mob.health
        );

        // A roof blocks direct sky light and therefore the burn. It covers the
        // whole wander area for the one-second measurement.
        let stone = host.registry().id_or_air("openblock:stone");
        for z in center_z - 5..=center_z + 5 {
            for x in center_x - 13..=center_x - 1 {
                assert!(host.world_mut().set_block(BlockPos::new(x, platform_y + 3, z), stone));
            }
        }
        let roofed_id = host
            .spawn_mob(
                MobKind::Zombie,
                WorldVec::new(
                    center_x as f64 - 5.0 + 0.5,
                    platform_y as f64 + 1.0,
                    center_z as f64 + 0.5,
                ),
            )
            .expect("roofed zombie spawns");
        let roofed_health = host.mob_entity(roofed_id).unwrap().mob.health;
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(
            host.mob_entity(roofed_id).unwrap().mob.health,
            roofed_health,
            "a roofed zombie must not burn"
        );

        // A nearly dead exposed zombie is removed by burn damage with no drops.
        let doomed = host
            .spawn_mob(
                MobKind::Zombie,
                WorldVec::new(center_x as f64 + 7.5, platform_y as f64 + 1.0, center_z as f64 + 0.5),
            )
            .expect("doomed zombie spawns");
        if let Some(index) = host.mobs.iter().position(|entity| entity.id == doomed) {
            host.mobs[index].mob.health = 0.45;
        }
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
        }
        assert!(host.mob_entity(doomed).is_none(), "burning must kill the zombie");
        assert!(
            host.item_entities().is_empty(),
            "a burned zombie drops nothing in this slice"
        );
    }

    #[test]
    fn day_night_phases_follow_the_existing_cycle() {
        let (_dir, mut host) = open_host("mob-day-phase", 51);
        for (ticks, day, night) in [
            (0, true, false),
            (11_999, true, false),
            (12_000, false, false),
            (12_999, false, false),
            (13_000, false, true),
            (22_999, false, true),
            (23_000, false, false),
            (23_999, false, false),
            (24_000, true, false),
            (24_000 + 13_500, false, true),
        ] {
            host.tick_count = ticks;
            assert_eq!(host.day_phase(), ticks % 24_000, "phase for {ticks}");
            assert_eq!(host.is_day(), day, "day at {ticks}");
            assert_eq!(host.is_night(), night, "night at {ticks}");
        }
        assert_eq!(DAY_LENGTH_TICKS, 24_000);
    }

    /// Builds a small high grass arena and teleports the player onto it.
    fn test_arena(label: &str, seed: u64) -> (TempDir, Host) {
        let (dir, mut host) = open_host(label, seed);
        host.set_render_distance_chunks(2);
        let spawn = host.player().pos;
        let center_x = spawn.x.floor() as i32;
        let center_z = spawn.z.floor() as i32;
        teleport_player(
            &mut host,
            WorldVec::new(center_x as f64 + 0.5, 201.0, center_z as f64 + 0.5),
        );
        converge_here(&mut host);
        grass_platform(&mut host, center_x, center_z, 200, 12);
        converge_here(&mut host);
        (dir, host)
    }

    #[test]
    fn player_attack_hand_damage_kill_drops_and_ray_selection() {
        let (_dir, mut host) = test_arena("mob-combat-hand", 45);
        host.set_game_mode(GameMode::Survival);
        let player = host.player().pos;
        let cow_id = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y, player.z))
            .expect("cow spawns on the arena");
        assert_eq!(host.mob_count(), 1);
        assert_eq!(host.passive_mob_count(), 1);
        assert_eq!(host.hostile_mob_count(), 0);

        // The shared ray helper sees the cow when the camera aims at its body.
        let origin = host.player().eye();
        let target = host.mob_entity(cow_id).unwrap().mob.pos + WorldVec::new(0.0, 0.65, 0.0);
        let forward = (target - origin).normalize();
        assert_eq!(host.mob_hit_by_ray(origin, forward, REACH), Some(cow_id));
        assert_eq!(host.mob_hit_by_ray(origin, forward, 0.5), None);

        let hash_before = host.state_hash();
        let first = host.attack_mob(cow_id);
        assert!(first.hit);
        assert_eq!(first.damage, 1.0);
        assert!(!first.killed);
        assert!(first.drops.is_empty());
        assert!(first.tool_broke.is_none());
        assert_eq!(host.mob_entity(cow_id).unwrap().mob.health, 9.0);
        assert!(host.mob_entity(cow_id).unwrap().mob.hurt_timer > 0.0);
        assert_ne!(host.state_hash(), hash_before, "a hit must change the state hash");

        // Outside MOB_ATTACK_REACH the host refuses the hit even though the
        // mob exists and is visible.
        assert_eq!(MOB_ATTACK_REACH, REACH + 0.5);
        let far_id = host
            .spawn_mob(MobKind::Pig, WorldVec::new(player.x + 8.0, player.y, player.z))
            .expect("far pig spawns");
        let missed = host.attack_mob(far_id);
        assert!(!missed.hit);
        assert_eq!(missed.damage, 0.0);
        assert_eq!(host.mob_entity(far_id).unwrap().mob.health, 10.0);
        assert!(host.mob_entity(far_id).unwrap().mob.hurt_timer == 0.0);

        let mut outcome = first;
        for _ in 0..9 {
            outcome = host.attack_mob(cow_id);
        }
        assert!(outcome.hit);
        assert!(outcome.killed);
        assert_eq!(outcome.damage, 1.0);
        assert!(host.mob_entity(cow_id).is_none());
        assert_eq!(host.mob_count(), 1, "only the untouched far pig remains");
        assert_eq!(outcome.drops.len(), 2);
        let beef = host.registry().item_id("openblock:raw_beef").unwrap();
        let leather = host.registry().item_id("openblock:leather").unwrap();
        assert_eq!(outcome.drops, vec![ItemStack::new(beef, 1), ItemStack::new(leather, 1)]);
        assert_eq!(host.item_entities().len(), 2, "every drop is an item entity");
        let mut drops: Vec<ItemId> = host.item_entities().iter().map(|entity| entity.stack.item).collect();
        drops.sort_by_key(|item| item.0);
        assert_eq!(drops, vec![beef.min(leather), beef.max(leather)]);
        assert!(!host.attack_mob(cow_id).hit, "missing ids are rejected");
    }

    #[test]
    fn survival_attacks_wear_tools_and_break_at_zero() {
        let (_dir, mut host) = test_arena("mob-combat-tool", 46);
        host.set_game_mode(GameMode::Survival);
        let player = host.player().pos;
        let sword = host.registry().item_id("openblock:wooden_sword").unwrap();
        give_and_select(&mut host, sword, 1);
        let zombie_id = host
            .spawn_mob(MobKind::Zombie, WorldVec::new(player.x + 2.0, player.y, player.z))
            .expect("zombie spawns");
        let hit = host.attack_mob(zombie_id);
        assert!(hit.hit);
        assert_eq!(hit.damage, 4.0, "wooden sword deals 4");
        assert_eq!(host.mob_entity(zombie_id).unwrap().mob.health, 16.0);
        let worn = host.selected_stack().unwrap();
        assert_eq!(worn.item, sword);
        assert_eq!(worn.durability, Some(58), "one attack wears one durability");

        // A tool at one durability breaks on the next landed hit, even if the
        // hit kills the target.
        let cow_id = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y, player.z + 2.0))
            .expect("cow spawns");
        let slot = host.hotbar_selection();
        host.inventory[slot] = Some(worn.with_durability(1u16));
        let broken = host.attack_mob(cow_id);
        assert!(broken.hit);
        assert_eq!(broken.tool_broke, Some(sword));
        assert!(host.selected_stack().is_none(), "the broken tool leaves the hotbar");
        assert_eq!(host.mob_entity(cow_id).unwrap().mob.health, 6.0);

        // Creative attacks deal damage but never wear tools.
        host.set_game_mode(GameMode::Creative);
        give_and_select(&mut host, sword, 1);
        let durability = host.selected_stack().unwrap().durability;
        let chicken_id = host
            .spawn_mob(
                MobKind::Chicken,
                WorldVec::new(player.x + 2.0, player.y, player.z - 2.0),
            )
            .expect("chicken spawns");
        let hit = host.attack_mob(chicken_id);
        assert!(hit.hit);
        assert_eq!(hit.damage, 4.0);
        assert_eq!(hit.tool_broke, None);
        assert_eq!(host.selected_stack().unwrap().durability, durability);
        assert!(hit.killed, "a 4-damage hit kills a 4 HP chicken");
        assert!(hit.drops.is_empty(), "chickens drop nothing");
        assert!(host.mob_entity(chicken_id).is_none());
    }

    #[test]
    fn zombie_attacks_are_armor_mitigated_and_knock_the_player_back() {
        let (_dir, mut host) = test_arena("mob-combat-armor", 47);
        host.set_game_mode(GameMode::Survival);
        let player = host.player().pos;
        let helmet = host.registry().item_id("openblock:leather_helmet").unwrap();
        give_and_select(&mut host, helmet, 1);
        assert!(host.equip_armor(host.hotbar_selection()));
        assert_eq!(host.armor_defense(), 1);

        let zombie_id = host
            .spawn_mob(MobKind::Zombie, WorldVec::new(player.x + 1.0, player.y, player.z))
            .expect("zombie spawns next to the player");
        let health_before = host.player().health;
        host.tick(&PlayerInput::default());
        let mitigated = 3.0 * (1.0 - armor_damage_reduction(1));
        let health_after = host.player().health;
        assert!(
            (health_before - health_after - mitigated).abs() < 1e-4,
            "armor mitigation was {} instead of {mitigated}",
            health_before - health_after
        );
        assert_eq!(
            host.equipment()[0].unwrap().durability,
            Some(54),
            "one armor durability per damaging hit"
        );
        assert!(
            host.player().velocity.x < 0.0,
            "knockback must push away from the zombie"
        );
        assert!(host.mob_entity(zombie_id).is_some());

        // Creative players are ignored by mobs and take no damage, while their
        // own attacks still hurt mobs.
        host.set_game_mode(GameMode::Creative);
        let health = host.player().health;
        for _ in 0..20 {
            host.tick(&PlayerInput::default());
        }
        assert_eq!(host.player().health, health, "creative players are invulnerable");
        let cow_id = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x - 2.0, player.y, player.z))
            .expect("cow spawns");
        let hit = host.attack_mob(cow_id);
        assert!(hit.hit);
        assert_eq!(hit.damage, 1.0, "creative hand damage still applies");
    }

    #[test]
    fn mobs_despawn_by_age_distance_and_the_void() {
        let (_dir, mut host) = open_host("mob-despawn-host", 48);
        host.set_game_mode(GameMode::Survival);
        host.set_render_distance_chunks(6);
        let player = host.player().pos;
        converge_here(&mut host);

        let near = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y + 3.0, player.z))
            .expect("near cow spawns");
        let old = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 3.0, player.y + 3.0, player.z))
            .expect("old cow spawns");
        if let Some(index) = host.mobs.iter().position(|entity| entity.id == old) {
            host.mobs[index].mob.age = 601.0;
        }
        let far = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 70.0, player.y + 3.0, player.z))
            .expect("far cow spawns in a loaded column");
        let void = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 4.0, player.y + 3.0, player.z))
            .expect("void cow spawns");
        if let Some(index) = host.mobs.iter().position(|entity| entity.id == void) {
            host.mobs[index].mob.pos.y = -65.0;
        }

        host.tick(&PlayerInput::default());
        assert!(host.mob_entity(old).is_none(), "age > 600 s must despawn");
        assert!(host.mob_entity(far).is_none(), "distance > 64 m must despawn");
        assert!(host.mob_entity(void).is_none(), "y < -64 must despawn");
        assert!(host.mob_entity(near).is_some(), "a fresh near mob must survive");
    }

    #[test]
    fn mobs_persist_sorted_and_state_hash_round_trips() {
        let (_dir, mut host) = test_arena("mob-save", 49);
        let player = host.player().pos;
        let cow_id = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y, player.z))
            .expect("cow spawns");
        let zombie_id = host
            .spawn_mob(MobKind::Zombie, WorldVec::new(player.x - 2.0, player.y, player.z + 1.0))
            .expect("zombie spawns");
        // Age and damage are persisted even though AI/velocity are not.
        for (id, age, health) in [(cow_id, 12.5, 7.25), (zombie_id, 3.0, 17.5)] {
            let index = host.mobs.iter().position(|entity| entity.id == id).unwrap();
            host.mobs[index].mob.age = age;
            host.mobs[index].mob.health = health;
            host.mobs[index].mob.yaw = 1.25;
            host.mobs[index].mob.ai = MobAi::Wander {
                target: WorldVec::new(1.0, 2.0, 3.0),
                timer: 1.0,
            };
        }
        let snapshot: Vec<(u64, MobKind, WorldVec, f32, f32, f64)> = host
            .mobs()
            .iter()
            .map(|entity| {
                (
                    entity.id,
                    entity.mob.kind,
                    entity.mob.pos,
                    entity.mob.yaw,
                    entity.mob.health,
                    entity.mob.age,
                )
            })
            .collect();
        converge_here(&mut host);
        let hash_before = host.state_hash();
        host.save().expect("save succeeds");
        let mobs_path = test_world_dir(&_dir).join("mobs.txt");
        let text = std::fs::read_to_string(&mobs_path).expect("mobs.txt is written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let ids: Vec<u64> = lines
            .iter()
            .map(|line| line.split_whitespace().next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(ids, vec![cow_id.min(zombie_id), cow_id.max(zombie_id)]);
        for line in &lines {
            assert_eq!(line.split_whitespace().count(), 8, "id kind x y z yaw health age");
        }
        assert!(text.contains("openblock:cow"));
        assert!(text.contains("openblock:zombie"));

        let config = host.config().clone();
        let registry = Arc::clone(host.registry());
        drop(host);
        let mut reopened = Host::open(registry, config).expect("host reopens");
        converge_here(&mut reopened);
        let loaded: Vec<(u64, MobKind, WorldVec, f32, f32, f64)> = reopened
            .mobs()
            .iter()
            .map(|entity| {
                (
                    entity.id,
                    entity.mob.kind,
                    entity.mob.pos,
                    entity.mob.yaw,
                    entity.mob.health,
                    entity.mob.age,
                )
            })
            .collect();
        assert_eq!(loaded, snapshot, "persisted fields must round-trip exactly");
        assert_eq!(reopened.mob_count(), 2);
        assert_eq!(reopened.state_hash(), hash_before, "state hash must round-trip");
        assert_eq!(reopened.mobs()[0].mob.ai, MobAi::Idle, "AI restarts idle");

        // A missing sidecar (old world) loads as an empty population.
        std::fs::remove_file(&mobs_path).expect("remove sidecar");
        let config = reopened.config().clone();
        let registry = Arc::clone(reopened.registry());
        drop(reopened);
        let empty = Host::open(registry, config).expect("host reopens without mobs");
        assert_eq!(empty.mob_count(), 0);
    }

    #[test]
    fn mob_loader_skips_malformed_lines_and_clamps_values() {
        let (dir, mut host) = test_arena("mob-load", 50);
        let player = host.player().pos;
        let cow_id = host
            .spawn_mob(MobKind::Cow, WorldVec::new(player.x + 2.0, player.y, player.z))
            .expect("cow spawns");
        host.save().expect("save succeeds");
        let registry = Arc::clone(host.registry());
        let config = host.config().clone();
        drop(host);

        let path = test_world_dir(&dir).join("mobs.txt");
        let malicious = format!(
            "{} openblock:sheep 0 201 0 0.5 99 3\n\
             {cow_id} openblock:enderman 0 201 0 0 10 0\n\
             {cow_id} openblock:cow 0 201 0 0 10 0\n\
             not-a-number openblock:cow 0 201 0 0 10 0\n\
             99 openblock:cow 0 201 0 0 NaN 0\n\
             {cow_id} openblock:cow 0 201 0 0\n",
            cow_id + 1
        );
        std::fs::write(&path, malicious).expect("write malformed sidecar");
        let mut reopened = Host::open(registry, config).expect("host tolerates malformed mobs.txt");
        converge_here(&mut reopened);
        assert_eq!(reopened.mob_count(), 1, "only the first valid row survives");
        let cow = &reopened.mobs()[0];
        assert_eq!(cow.mob.kind, MobKind::Sheep);
        assert_eq!(cow.mob.health, 8.0, "health clamps to the species maximum");
        assert_eq!(cow.mob.age, 3.0);
        assert_eq!(cow.mob.pos.y, 201.0);
    }

    #[test]
    fn two_hosts_with_the_same_seed_spawn_the_same_mobs() {
        let (_dir_a, mut a) = survival_host("mob-det-a", 42);
        let (_dir_b, mut b) = survival_host("mob-det-b", 42);
        for _ in 0..300 {
            a.tick(&PlayerInput::default());
            b.tick(&PlayerInput::default());
        }
        for _ in 0..100 {
            if a.ensure_streaming(64) == 0 && b.ensure_streaming(64) == 0 {
                break;
            }
        }
        assert_eq!(a.mob_count(), b.mob_count());
        assert!(a.mob_count() > 0, "the determinism sample spawned no mobs");
        for (left, right) in a.mobs().iter().zip(b.mobs()) {
            assert_eq!(left.id, right.id);
            assert_eq!(left.mob.kind, right.mob.kind);
            assert_eq!(left.mob.pos.x.to_bits(), right.mob.pos.x.to_bits());
            assert_eq!(left.mob.pos.y.to_bits(), right.mob.pos.y.to_bits());
            assert_eq!(left.mob.pos.z.to_bits(), right.mob.pos.z.to_bits());
            assert_eq!(left.mob.yaw.to_bits(), right.mob.yaw.to_bits());
            assert_eq!(left.mob.health.to_bits(), right.mob.health.to_bits());
            assert_eq!(left.mob.age.to_bits(), right.mob.age.to_bits());
            assert_eq!(left.mob.ai, right.mob.ai);
        }
        assert_eq!(a.state_hash(), b.state_hash(), "same seed, ticks and mobs");
    }

    /// Full 65×65 streaming and unload at the 512-block render distance.
    ///
    /// This allocates the whole 322 MiB world heap and is therefore ignored in
    /// normal test runs; the LOD/streaming benchmark exercises the same path.
    /// Run with:
    /// `./dev-cargo test -p ob_server --release --lib -- --ignored --nocapture streaming_unload_plateau`
    #[test]
    #[ignore = "allocates a full 512-block world; run explicitly"]
    fn streaming_unload_plateau_at_512_blocks() {
        let data_dir = TempDir::new("rd512-stream");
        let registry = Arc::new(Registry::base());
        let mut host = Host::open(
            registry,
            HostConfig {
                seed: 0x5125_1251_2512_5125,
                world_name: "rd512-stream".to_string(),
                data_dir: Some(data_dir.path().to_path_buf()),
                render_distance_chunks: 32,
                generate_budget: 64,
            },
        )
        .expect("host opens");
        host.set_game_mode(GameMode::Creative);
        let center = host.player().pos;
        let started = Instant::now();
        let mut generated = 0usize;
        let mut calls = 0usize;
        loop {
            let count = host.ensure_streaming_around(center, 32, 512);
            generated += count;
            if count == 0 {
                break;
            }
            calls += 1;
            assert!(calls < 100, "streaming did not converge");
        }
        assert_eq!(host.world().chunk_count(), 65 * 65, "65x65 columns");
        assert!(generated >= 65 * 65 - 16, "most columns came from the drain loop");
        assert_eq!(host.ensure_streaming_around(center, 32, 64), 0, "plateau");
        let elapsed = started.elapsed();
        println!(
            "512-block streaming: {generated} columns in {:.1} ms ({calls} calls, {} MiB heap)",
            elapsed.as_secs_f64() * 1000.0,
            host.memory_bytes() / (1024 * 1024)
        );
        // Shrink the interest radius: every column outside radius + hysteresis
        // must be released and a second call must be a no-op.
        assert!(host.set_render_distance_chunks(6));
        let removed = host.unload_chunks_outside(&[center], 8);
        let expected_kept = 17 * 17;
        assert_eq!(removed, 65 * 65 - expected_kept);
        assert_eq!(host.world().chunk_count(), expected_kept);
        assert_eq!(host.unload_chunks_outside(&[center], 8), 0, "unload plateau");
        assert_eq!(host.take_unloaded_chunks().len(), removed);
    }
}

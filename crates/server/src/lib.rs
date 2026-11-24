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

use ob_content::{BlockId, ItemId, ItemType, RecipeId, Registry};
use ob_math::{BlockPos, ChunkPos, SEA_LEVEL, SectionPos, WorldVec};
use ob_sim::{
    BlockAction, MAX_HEALTH, MAX_HUNGER, Player, PlayerInput, REACH, TICK_DT, advance_vitals, apply_block_actions,
    apply_damage, apply_fall_damage, eat, reset_vitals, spawn_position, step_player,
};
use ob_world::{ChunkColumn, World};
use ob_worldgen::Generator;
use rayon::prelude::*;

mod items;

pub use items::ItemEntity;

/// Number of slots in the player's hotbar.
pub const HOTBAR_SLOTS: usize = 9;
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
    /// initialises durability to the tool's full durability.
    pub fn for_item(item: ItemId, item_type: &ItemType, count: u8) -> Self {
        let max = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let count = count.clamp(1, max);
        Self {
            item,
            count,
            durability: item_type.tool.map(|spec| spec.durability),
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
    pickup_events: Vec<ItemStack>,
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
                items: Vec::new(),
                next_item_id: 1,
                pickup_events: Vec::new(),
            };
            host.inventory = load_inventory(&host.world_dir, &host.registry);
            host.items = items::load_items(&host.world_dir, &host.registry);
            host.next_item_id = items::next_item_id(&host.items);
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
                items: Vec::new(),
                next_item_id: 1,
                pickup_events: Vec::new(),
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
    pub fn give_item(&mut self, item: ItemId, count: u8) -> u8 {
        if count == 0 || item.0 as usize >= self.registry.item_len() {
            return count;
        }
        let item_type = self.registry.item(item);
        let max_stack = item_type.max_stack.clamp(1, MAX_STACK_SIZE);
        let durability = item_type.tool.map(|spec| spec.durability);
        let mut remaining = count;
        // Tools (max_stack == 1) never merge into an existing stack; this loop
        // naturally does nothing for them because stack.count < max is false.
        for stack in self.inventory.iter_mut().flatten() {
            if stack.item == item && stack.durability == durability && stack.count < max_stack {
                let added = remaining.min(max_stack - stack.count);
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
                    let added = remaining.min(max_stack);
                    *slot = Some(ItemStack {
                        item,
                        count: added,
                        durability,
                    });
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
        self.tick_items(TICK_DT);
        self.mark_player_dirty();
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
            let mut index = 0;
            while index < self.items.len() {
                let near = self.items[index].age >= items::ITEM_PICKUP_DELAY
                    && items::can_pick_up(&self.player, &self.items[index]);
                if near {
                    let item = self.items[index].stack.item;
                    let count = self.items[index].stack.count;
                    if count > 0 {
                        let overflow = self.give_item(item, count);
                        let added = count - overflow;
                        if added > 0 {
                            let mut event = self.items[index].stack;
                            event.count = added;
                            self.pickup_events.push(event);
                            if overflow == 0 {
                                self.items.remove(index);
                                continue;
                            }
                            self.items[index].stack.count = overflow;
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
            let held = self.selected_stack();
            let held_item = held.map(|stack| stack.item);
            if block != BlockId::AIR
                && self.registry.block(block).breakable()
                && self.world.set_block(hit.block, BlockId::AIR)
            {
                outcome.changed = true;
                outcome.broke = Some(block);
                // A tiered block still breaks with a hand or an under-tier
                // tool, but yields no loot (Minecraft-like).
                if self.registry.can_drop(block, held_item) {
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
                }
                // Dropped loot becomes authoritative item entities at the
                // block centre; the inventory is only touched by pickup in
                // `tick`, never by the break itself.
                let centre = block_centre(hit.block);
                for drop in &outcome.drops {
                    self.spawn_item_entity(centre, *drop);
                }
                // Any selected tool loses one durability per broken block,
                // whether or not its kind matched. At zero the stack is removed
                // and the break is reported.
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
                    self.mark_player_dirty();
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

    /// Saves the world, player state, inventory and dropped items atomically.
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
        write_atomic(
            &self.world_dir.join("items.txt"),
            items::items_state(&self.registry, &self.items).as_bytes(),
        )?;
        self.dirty_player = false;
        Ok(())
    }

    /// Deterministic hash of the simulation state (world blocks in canonical
    /// order, player position and vitals quantised, inventory, dropped item
    /// entities sorted by id, and tick count). Used by the smoke test and by
    /// cross-platform determinism checks.
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
        if let Some(spec) = item_type.tool {
            let durability = match fields.get(3) {
                Some(text) => match text.parse::<u32>() {
                    Ok(value) => value.clamp(1, u32::from(spec.durability)) as u16,
                    Err(_) => {
                        log::warn!("inventory.txt:{line_number}: invalid durability '{text}'");
                        continue;
                    }
                },
                // v0.0.7 lines predate durability: tools start undamaged.
                None => spec.durability,
            };
            stack.durability = Some(durability);
        }
        let slot = slot.min(HOTBAR_SLOTS - 1);
        inventory[slot] = Some(stack);
    }
    inventory
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
    let durability = item_type.tool.map(|spec| spec.durability);
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

    use ob_content::{BlockId, ItemId, RecipeId, Registry};
    use ob_math::{BlockPos, CHUNK_SIZE, WORLD_MAX_Y, WORLD_MIN_Y, WorldVec};
    use ob_sim::{BlockAction, MAX_HEALTH, MAX_HUNGER, PlayerInput, REACH};
    use ob_world::ChunkColumn;
    use ob_worldgen::Generator;

    use super::{
        CraftOutcome, GameMode, HOTBAR_SLOTS, Host, HostConfig, ItemStack, StableHasher, apple_drop, block_centre,
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
}

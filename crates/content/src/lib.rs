//! The content model: block types, textures and tags.
//!
//! This is the v0.0.1 in-code registry. The data-driven content-pack pipeline
//! described in `docs/spec/15-content-system.md` replaces the hard-coded
//! `Registry::base()` at milestone MS-05 without changing this public API.
//!
//! The first slice of that pipeline lives in [`pack`]: [`registry_from_pack`]
//! loads the versioned JSON block schema and builds a [`Registry`] through the
//! same [`Registry::from_blocks`] path used by [`Registry::base`].
//!
//! # Items, tools, armor and recipes (interim)
//!
//! Every non-air block gets a matching block item (`id` = the block's resource
//! id). The catalogue also contains `openblock:apple`, `openblock:stick`, the
//! eight wooden/stone tools, iron tools, leather/iron armor, ores and metals,
//! and the animal-food items. Item ids are frozen: v0.0.7 and v0.0.8 positions
//! never move, the v0.0.9 non-block items occupy ids 24..=46, and block items
//! for blocks appended after `crafting_table` follow them. Recipes are
//! code-defined ingredient totals in deterministic id order; furnace smelting
//! and fuel values are pure lookup tables. Shaped/grid crafting, item
//! components, block loot tables and the data-driven item pipeline arrive in
//! `docs/spec/11-inventory-crafting-items.md`.
//!
//! The interim catalogue also carries an eight-species mob table
//! ([`MobKind`], [`MobSpec`]) and a melee `attack_damage` value on every tool;
//! see [`Registry::mob`] and [`Registry::attack_damage`].

use rustc_hash::FxHashMap;

pub mod pack;

pub use pack::{MAX_BLOCKS, MAX_PACK_BYTES, PackError, load_blocks_json, registry_from_pack};

/// A dense, session-local block type index. Never persisted or networked.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u16);

impl BlockId {
    /// Air.
    pub const AIR: BlockId = BlockId(0);
}

/// A dense, session-local item type index. Never persisted or networked by
/// numeric value: saves use the namespaced [`ItemType::id`] string instead.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub u16);

/// The block a tool is designed for.
///
/// Tools are intentionally a small, code-defined catalogue in this slice: a
/// tool's kind decides which blocks it can harvest efficiently, its tier
/// decides whether it satisfies a block's `required_tier`, and its speed is the
/// mining multiplier applied to eligible blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Stone and cobblestone.
    Pickaxe,
    /// Wood, planks and crafting tables.
    Axe,
    /// Dirt, sand, gravel, clay, snow and grass.
    Shovel,
    /// No block has a sword preference; swords exist for combat later.
    Sword,
    /// Farmland and future tilling targets.
    Hoe,
}

impl ToolKind {
    /// Stable lower-case name used by content packs and hints.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pickaxe => "pickaxe",
            Self::Axe => "axe",
            Self::Shovel => "shovel",
            Self::Sword => "sword",
            Self::Hoe => "hoe",
        }
    }
}

/// Mining and melee statistics for one tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolSpec {
    /// Which blocks this tool is efficient against.
    pub kind: ToolKind,
    /// Harvest tier: a block with `required_tier <= tier` drops when mined.
    pub tier: u8,
    /// Mining-speed multiplier against blocks of the matching kind.
    pub speed: f32,
    /// Base melee damage in health points when this tool is selected.
    ///
    /// A hand (no selected item) deals `1.0`; see
    /// [`Registry::attack_damage`]. Every tool keeps its mining fields, so a
    /// pickaxe is both a slower weapon and a mining tool.
    pub attack_damage: f32,
    /// Durability in blocks broken (or mobs hit) before the tool disappears.
    pub durability: u16,
}

/// The armor slot an [`ArmorSpec`] occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArmorSlot {
    /// Head slot.
    Helmet,
    /// Chest slot.
    Chestplate,
    /// Leg slot.
    Leggings,
    /// Feet slot.
    Boots,
}

impl ArmorSlot {
    /// Stable lower-case name used by content packs and hints.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Helmet => "helmet",
            Self::Chestplate => "chestplate",
            Self::Leggings => "leggings",
            Self::Boots => "boots",
        }
    }
}

/// Armor statistics for one wearable item.
///
/// The interim model exposes defense points and durability only; toughness,
/// knockback resistance and enchantability arrive with the data-driven item
/// pipeline in `docs/spec/11-inventory-crafting-items.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmorSpec {
    /// Equip slot this piece occupies.
    pub slot: ArmorSlot,
    /// Armor points contributed while worn.
    pub defense: u8,
    /// Durability in hits absorbed before the piece breaks.
    pub durability: u16,
}

/// One mob species in the interim 1.0 catalogue.
///
/// Mobs are a deliberately small, code-defined set in this slice. The
/// data-driven entity pipeline in `docs/spec/09-entities-ai.md` replaces this
/// enum with registry definitions after 1.0; the namespaced names below are
/// stable save keys and never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MobKind {
    /// A passive cow.
    Cow,
    /// A passive pig.
    Pig,
    /// A passive sheep.
    Sheep,
    /// A passive chicken.
    Chicken,
    /// A hostile zombie.
    Zombie,
    /// A fast hostile spider (1.3).
    Spider,
    /// A hostile skeleton archer (1.3).
    Skeleton,
    /// A hostile creeper that explodes (1.3).
    Creeper,
}

impl MobKind {
    /// Every mob kind, in stable catalogue order.
    pub const ALL: [MobKind; 8] = [
        MobKind::Cow,
        MobKind::Pig,
        MobKind::Sheep,
        MobKind::Chicken,
        MobKind::Zombie,
        MobKind::Spider,
        MobKind::Skeleton,
        MobKind::Creeper,
    ];

    /// Stable namespaced resource id, e.g. `openblock:cow`.
    ///
    /// This is the string written to `mobs.txt` and the key accepted by
    /// [`MobKind::from_name`]; it never changes for a shipped kind.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cow => "openblock:cow",
            Self::Pig => "openblock:pig",
            Self::Sheep => "openblock:sheep",
            Self::Chicken => "openblock:chicken",
            Self::Zombie => "openblock:zombie",
            Self::Spider => "openblock:spider",
            Self::Skeleton => "openblock:skeleton",
            Self::Creeper => "openblock:creeper",
        }
    }

    /// Parses a namespaced mob id from a save line.
    ///
    /// Only the exact namespaced form is accepted, so malformed or
    /// unnamespaced ids are skipped by the loader instead of being guessed.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Stable catalogue index (`Cow == 0` .. `Creeper == 7`).
    ///
    /// [`Registry::mob`] indexes its spec table by this value.
    pub const fn index(self) -> usize {
        match self {
            Self::Cow => 0,
            Self::Pig => 1,
            Self::Sheep => 2,
            Self::Chicken => 3,
            Self::Zombie => 4,
            Self::Spider => 5,
            Self::Skeleton => 6,
            Self::Creeper => 7,
        }
    }
}

/// A projectile species carried on the wire and simulated by the host.
///
/// The 1.3 slice ships one `Arrow` kind. Namespaced names are stable save and
/// protocol keys, exactly like mob names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProjectileKind {
    /// A skeleton arrow with gravity and a block/player impact.
    Arrow,
}

impl ProjectileKind {
    /// Every projectile kind, in stable catalogue order.
    pub const ALL: [ProjectileKind; 1] = [ProjectileKind::Arrow];

    /// Stable namespaced resource id.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Arrow => "openblock:arrow",
        }
    }

    /// Parses a namespaced projectile id.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// How a hostile mob attacks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MobAttack {
    /// No attack (every passive species).
    None,
    /// A melee attack with a server-side wind-up and cooldown.
    Melee {
        /// Damage before armor, in health points.
        damage: f32,
        /// Maximum centre-to-centre reach in metres.
        range: f32,
        /// Seconds between completed attacks.
        cooldown: f64,
        /// Seconds between entering range and the hit landing.
        windup: f64,
    },
    /// A ranged attack that keeps a distance band and fires a projectile.
    Ranged {
        /// Damage before armor per projectile, in health points.
        damage: f32,
        /// Closer than this, the mob backs away, in metres.
        min_range: f32,
        /// Farther than this, the mob closes in, in metres.
        max_range: f32,
        /// Seconds between shots.
        cooldown: f64,
    },
    /// A creeper-style fuse followed by a block/entity explosion.
    Explosive {
        /// Distance at which the fuse starts, in metres.
        trigger_range: f32,
        /// Fuse length in seconds.
        fuse: f64,
        /// Block/entity damage radius in metres.
        radius: f32,
        /// Damage at the centre before armor and falloff.
        max_damage: f32,
    },
}

/// Definition and deterministic drop table for one mob kind.
///
/// Drop entries are resolved against the registry when it is built, so a
/// custom block pack that lacks an item simply omits that drop. The list is
/// static and deterministic: no RNG is involved in a mob death.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MobSpec {
    /// Species this definition describes.
    pub kind: MobKind,
    /// ASCII display name for hints, HUD and tests.
    pub display_name: &'static str,
    /// Maximum and starting health in health points.
    pub health: f32,
    /// Horizontal movement speed in metres per second.
    pub speed: f32,
    /// Whether the mob attacks players (zombies).
    pub hostile: bool,
    /// Collision-box width in metres.
    pub width: f32,
    /// Collision-box height in metres.
    pub height: f32,
    /// Deterministic death drops as `(item, count)` pairs.
    pub drops: &'static [(ItemId, u8)],
    /// Attack pattern and tuning.
    pub attack: MobAttack,
    /// Direct player XP awarded for a kill.
    pub xp_reward: u32,
    /// Whether the mob stays neutral in gameplay light `>= 9` (spider).
    pub neutral_in_light: bool,
    /// Whether the mob can step up one-block ledges (spider).
    pub climbs: bool,
    /// Whether direct daylight burns the mob (zombie/skeleton).
    pub burns_in_daylight: bool,
    /// Projectile fired by a ranged attack.
    pub projectile: Option<ProjectileKind>,
}

impl MobSpec {
    /// Whether this mob belongs to the passive (creature) cap.
    pub fn passive(&self) -> bool {
        !self.hostile
    }
}

/// An item type definition.
///
/// The interim catalogue is code-defined (see the crate docs). Data-driven item
/// packs with components and loot tables come later; the fields here are the
/// minimum the survival slice needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemType {
    /// Namespaced resource id, e.g. `openblock:stone` or `openblock:apple`.
    pub id: &'static str,
    /// The block this item places, for block items.
    pub block: Option<BlockId>,
    /// Hunger points restored by eating; `None` or `Some(0)` means inedible.
    pub food: Option<u8>,
    /// Tool statistics; `None` for every non-tool item.
    pub tool: Option<ToolSpec>,
    /// Armor statistics; `None` for every non-wearable item.
    pub armor: Option<ArmorSpec>,
    /// Maximum number of items in one stack (`1..=`[`MAX_ITEM_STACK`]).
    pub max_stack: u8,
}

impl ItemType {
    /// Whether eating this item can restore hunger.
    pub fn edible(&self) -> bool {
        matches!(self.food, Some(points) if points > 0)
    }

    /// Whether this item is a tool.
    pub fn is_tool(&self) -> bool {
        self.tool.is_some()
    }

    /// Whether this item is a wearable armor piece.
    pub fn is_armor(&self) -> bool {
        self.armor.is_some()
    }

    /// The block this item places, for block items.
    pub fn block(&self) -> Option<BlockId> {
        self.block
    }
}

/// Largest stack any base item declares. The server keeps this as a defensive
/// cap so a malformed pack cannot request an unbounded stack.
pub const MAX_ITEM_STACK: u8 = 64;

/// One weighted entry in the structure-chest loot table.
///
/// The table is deliberately small and code-defined in 1.3: it is the
/// foundation for the data-driven loot-table pipeline, not the final schema.
/// Every item is an existing base item so a chest never depends on a new block
/// or tool tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructureLootEntry {
    /// Stable namespaced item id.
    pub item: &'static str,
    /// Minimum stack count when this entry is chosen.
    pub min: u8,
    /// Maximum stack count when this entry is chosen.
    pub max: u8,
    /// Relative lottery weight.
    pub weight: u32,
}

/// Structure-chest loot pool shared by every 1.3 structure kind.
///
/// The server adds one bread/wheat/seed entry per roll plus a small chance at
/// the rarer metal/torch/string/bone/arrow entries. The pool contains exactly
/// one entry per item so a roll order cannot depend on hash-map iteration.
pub const STRUCTURE_LOOT_TABLE: [StructureLootEntry; 10] = [
    StructureLootEntry {
        item: "openblock:bread",
        min: 1,
        max: 2,
        weight: 3,
    },
    StructureLootEntry {
        item: "openblock:wheat",
        min: 1,
        max: 3,
        weight: 3,
    },
    StructureLootEntry {
        item: "openblock:seeds",
        min: 1,
        max: 3,
        weight: 2,
    },
    StructureLootEntry {
        item: "openblock:iron_ingot",
        min: 1,
        max: 2,
        weight: 2,
    },
    StructureLootEntry {
        item: "openblock:coal",
        min: 1,
        max: 4,
        weight: 3,
    },
    StructureLootEntry {
        item: "openblock:torch",
        min: 2,
        max: 4,
        weight: 2,
    },
    StructureLootEntry {
        item: "openblock:string",
        min: 1,
        max: 2,
        weight: 2,
    },
    StructureLootEntry {
        item: "openblock:bone",
        min: 1,
        max: 2,
        weight: 2,
    },
    StructureLootEntry {
        item: "openblock:apple",
        min: 1,
        max: 2,
        weight: 1,
    },
    StructureLootEntry {
        item: "openblock:arrow",
        min: 4,
        max: 8,
        weight: 2,
    },
];

/// Number of loot rolls for one chest (`4..=6`, derived from the structure hash).
pub const fn structure_loot_rolls(structure_hash: u64) -> u8 {
    4 + (structure_hash % 3) as u8
}

/// One splitmix64 finaliser, duplicated locally so `ob_content` does not depend
/// on the math crate for a single hash. Determinism is the contract, not the
/// implementation.
const fn loot_mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Rolls the structure-chest loot for one chest.
///
/// `seed` is the world seed and `structure_hash` the worldgen region hash. The
/// chest block position mixes in as well, so two chests in the same structure
/// never share a roll sequence. The result is sorted by item id order (the
/// table order), contains no empty stacks and depends on no RNG state.
pub fn roll_structure_loot(
    seed: u64,
    structure_hash: u64,
    chest: (i32, i32, i32),
    rolls: u8,
) -> Vec<(&'static str, u8)> {
    let total_weight: u64 = STRUCTURE_LOOT_TABLE.iter().map(|entry| u64::from(entry.weight)).sum();
    if total_weight == 0 {
        return Vec::new();
    }
    let pos_hash = loot_mix64(
        (chest.0 as i64 as u64)
            ^ loot_mix64((chest.1 as i64 as u64).wrapping_mul(31))
            ^ loot_mix64((chest.2 as i64 as u64).wrapping_mul(131)),
    );
    let base = loot_mix64(seed ^ structure_hash.rotate_left(23)) ^ pos_hash;
    let mut out = Vec::with_capacity(usize::from(rolls));
    for roll in 0..u64::from(rolls) {
        let hash = loot_mix64(base ^ roll.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut pick = hash % total_weight;
        let mut chosen = STRUCTURE_LOOT_TABLE[STRUCTURE_LOOT_TABLE.len() - 1];
        for entry in STRUCTURE_LOOT_TABLE {
            let weight = u64::from(entry.weight);
            if pick < weight {
                chosen = entry;
                break;
            }
            pick -= weight;
        }
        let span = u64::from(chosen.max.saturating_sub(chosen.min)) + 1;
        let count = chosen.min + ((hash >> 32) % span) as u8;
        out.push((chosen.item, count.max(1)));
    }
    out.sort_by_key(|(item, _)| *item);
    out
}

/// Resolves [`roll_structure_loot`] names against a registry.
///
/// Custom packs that omit an item simply drop that entry, matching the mob and
/// recipe tables. The result is in ascending [`ItemId`] order so chest slots are
/// stable across builds.
pub fn structure_loot_stacks(
    registry: &Registry,
    seed: u64,
    structure_hash: u64,
    chest: (i32, i32, i32),
    rolls: u8,
) -> Vec<(ItemId, u8)> {
    let mut stacks: Vec<(ItemId, u8)> = roll_structure_loot(seed, structure_hash, chest, rolls)
        .into_iter()
        .filter_map(|(name, count)| registry.item_id(name).map(|item| (item, count)))
        .collect();
    stacks.sort_by_key(|(item, _)| *item);
    stacks
}

/// Index into the block texture array.
///
/// Layer 0 is reserved for a debug checkerboard; layers are assigned by
/// [`textures::NAMES`] and generated procedurally by the renderer at start-up
/// (real texture packs arrive with MS-05).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextureId(pub u8);

/// The procedural block texture catalogue (index = array layer).
pub mod textures {
    use super::TextureId;

    /// Debug checkerboard used for unknown textures.
    pub const DEBUG: TextureId = TextureId(0);
    /// Grass top.
    pub const GRASS_TOP: TextureId = TextureId(1);
    /// Grass side (dirt with a green band).
    pub const GRASS_SIDE: TextureId = TextureId(2);
    /// Dirt.
    pub const DIRT: TextureId = TextureId(3);
    /// Stone.
    pub const STONE: TextureId = TextureId(4);
    /// Sand.
    pub const SAND: TextureId = TextureId(5);
    /// Water.
    pub const WATER: TextureId = TextureId(6);
    /// Log side (bark).
    pub const LOG_SIDE: TextureId = TextureId(7);
    /// Log top (rings).
    pub const LOG_TOP: TextureId = TextureId(8);
    /// Leaves.
    pub const LEAVES: TextureId = TextureId(9);
    /// Bedrock.
    pub const BEDROCK: TextureId = TextureId(10);
    /// Gravel.
    pub const GRAVEL: TextureId = TextureId(11);
    /// Clay.
    pub const CLAY: TextureId = TextureId(12);
    /// Snow.
    pub const SNOW: TextureId = TextureId(13);
    /// Wooden planks.
    pub const PLANKS: TextureId = TextureId(14);
    /// Cobblestone.
    pub const COBBLESTONE: TextureId = TextureId(15);
    /// Crafting table top.
    pub const CRAFTING_TABLE_TOP: TextureId = TextureId(16);
    /// Crafting table side (the front face reuses this layer).
    pub const CRAFTING_TABLE_SIDE: TextureId = TextureId(17);
    /// Coal ore.
    pub const COAL_ORE: TextureId = TextureId(18);
    /// Iron ore.
    pub const IRON_ORE: TextureId = TextureId(19);
    /// Copper ore.
    pub const COPPER_ORE: TextureId = TextureId(20);
    /// Furnace top and bottom.
    pub const FURNACE_TOP: TextureId = TextureId(21);
    /// Furnace side.
    pub const FURNACE_SIDE: TextureId = TextureId(22);
    /// Furnace front (interim: always the +Z face; see [`crate::BlockTextures`]).
    pub const FURNACE_FRONT: TextureId = TextureId(23);
    /// Chest top and bottom.
    pub const CHEST_TOP: TextureId = TextureId(24);
    /// Chest side.
    pub const CHEST_SIDE: TextureId = TextureId(25);
    /// Chest front (interim: always the +Z face; see [`crate::BlockTextures`]).
    pub const CHEST_FRONT: TextureId = TextureId(26);
    /// Torch cut-out (cross model, transparent background).
    pub const TORCH: TextureId = TextureId(27);
    /// Dry farmland top.
    pub const FARMLAND_DRY: TextureId = TextureId(28);
    /// Hydrated farmland top.
    pub const FARMLAND_WET: TextureId = TextureId(29);
    /// Wheat growth stage 0 (sprout).
    pub const WHEAT_0: TextureId = TextureId(30);
    /// Wheat growth stage 1.
    pub const WHEAT_1: TextureId = TextureId(31);
    /// Wheat growth stage 2.
    pub const WHEAT_2: TextureId = TextureId(32);
    /// Wheat growth stage 3 (harvestable).
    pub const WHEAT_3: TextureId = TextureId(33);
    /// Bed foot top (blanket and pillow).
    pub const BED_FOOT_TOP: TextureId = TextureId(34);
    /// Bed foot side.
    pub const BED_FOOT_SIDE: TextureId = TextureId(35);
    /// Bed head top (blanket).
    pub const BED_HEAD_TOP: TextureId = TextureId(36);
    /// Bed head side.
    pub const BED_HEAD_SIDE: TextureId = TextureId(37);
    /// Fire cross cut-out (animated by the renderer; the procedural catalogue
    /// still stores one layer so a client/render agent can drive the animation
    /// from the layer name `fire`).
    pub const FIRE: TextureId = TextureId(38);

    /// Number of texture layers the renderer must generate.
    pub const COUNT: usize = 39;

    /// Names of the procedural textures, indexed by layer.
    pub const NAMES: [&str; COUNT] = [
        "debug",
        "grass_top",
        "grass_side",
        "dirt",
        "stone",
        "sand",
        "water",
        "log_side",
        "log_top",
        "leaves",
        "bedrock",
        "gravel",
        "clay",
        "snow",
        "planks",
        "cobblestone",
        "crafting_table_top",
        "crafting_table_side",
        "coal_ore",
        "iron_ore",
        "copper_ore",
        "furnace_top",
        "furnace_side",
        "furnace_front",
        "chest_top",
        "chest_side",
        "chest_front",
        "torch",
        "farmland_dry",
        "farmland_wet",
        "wheat_0",
        "wheat_1",
        "wheat_2",
        "wheat_3",
        "bed_foot_top",
        "bed_foot_side",
        "bed_head_top",
        "bed_head_side",
        "fire",
    ];

    // Compile-time guard: the layer table and the catalogue must stay in sync.
    const _: () = assert!(NAMES.len() == COUNT);
}

/// Per-face texture set: top, side, bottom and an optional front.
///
/// The simplified v0.x block model has no facing state. Blocks that declare a
/// front texture (the furnace and the chest) use it on the +Z face only; a
/// facing state (and per-placement rotation) arrives post-1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTextures {
    /// Texture for the +Y face.
    pub top: TextureId,
    /// Texture for the four side faces.
    pub side: TextureId,
    /// Texture for the -Y face.
    pub bottom: TextureId,
    /// Optional texture for the +Z face; `None` reuses [`Self::side`].
    pub front: Option<TextureId>,
}

impl BlockTextures {
    /// One texture for every face.
    pub const fn all(id: TextureId) -> Self {
        Self {
            top: id,
            side: id,
            bottom: id,
            front: None,
        }
    }

    /// Distinct top and side textures with a shared bottom.
    pub const fn column(top: TextureId, side: TextureId, bottom: TextureId) -> Self {
        Self {
            top,
            side,
            bottom,
            front: None,
        }
    }

    /// Distinct top, side and bottom textures plus an interim +Z front.
    pub const fn fronted(top: TextureId, side: TextureId, bottom: TextureId, front: TextureId) -> Self {
        Self {
            top,
            side,
            bottom,
            front: Some(front),
        }
    }

    /// The texture used by the interim front face, falling back to the side.
    pub const fn front_texture(&self) -> TextureId {
        match self.front {
            Some(front) => front,
            None => self.side,
        }
    }

    /// Texture for a face direction: 0 = +Y, 1 = -Y, 2/3/5 = sides, 4 = +Z.
    ///
    /// Face 4 is the interim front face: it uses [`Self::front`] when declared
    /// and [`Self::side`] otherwise. Render code can keep calling this method;
    /// a post-1.0 facing state should select the side explicitly instead.
    pub fn for_face(&self, face: u8) -> TextureId {
        match face {
            0 => self.top,
            1 => self.bottom,
            4 => self.front_texture(),
            _ => self.side,
        }
    }
}

/// The rendered geometry of a block type.
///
/// 1.2 adds only the two shapes the first content delta needs; partial cube
/// shapes (slabs, stairs) are deferred to the block-state wave.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockShape {
    /// A full cube: six axis-aligned faces.
    #[default]
    Cube,
    /// Two diagonal, alpha-cut-out quads (torches, crops, flowers).
    Cross,
}

impl BlockShape {
    /// Stable lower-case name used by content packs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cube => "cube",
            Self::Cross => "cross",
        }
    }
}

/// A block type definition. Block states (properties) arrive with MS-01's
/// full palette work; v0.0.x has one state per type.
#[derive(Debug, Clone)]
pub struct BlockType {
    /// Namespaced resource id, e.g. `openblock:grass`.
    pub id: &'static str,
    /// Base colour (used for UI/hotbar fallback and block-break particles).
    pub color: [f32; 3],
    /// Blocks movement and can support an entity.
    pub solid: bool,
    /// Fully hides the neighbouring face (opaque for meshing).
    pub opaque: bool,
    /// Renders and behaves as a fluid (swimming/collision policy at MS-10).
    pub fluid: bool,
    /// Textures for top/side/bottom faces.
    pub textures: BlockTextures,
    /// Hand-breaking hardness in seconds; `f32::INFINITY` means unbreakable.
    ///
    /// The effective duration is `hardness / mining_speed`; see
    /// [`Registry::effective_mining_seconds`].
    pub hardness: f32,
    /// The tool kind that mines this block quickly and may harvest it.
    ///
    /// Packs that omit the field default to `None` (no preferred tool).
    pub preferred_tool: Option<ToolKind>,
    /// Minimum tool tier needed for a drop and for the preferred-tool speed.
    ///
    /// `0` means the block drops by hand and a matching tool only speeds it up;
    /// `1`/`2` require a tool of [`BlockType::preferred_tool`] at or above that
    /// tier. Hand or an under-tier tool still breaks the block, but slowly and
    /// without a drop.
    pub required_tier: u8,
    /// Emitted block-light levels per RGB channel (`0..=15`).
    ///
    /// The base pack uses white emitters (`[14, 14, 14]` for a torch); the
    /// per-channel form exists for 1.3+ coloured lamps and stained glass.
    pub emission: [u8; 3],
    /// Block-light attenuation per RGB channel (`0..=15`).
    ///
    /// Lateral propagation costs `max(1, attenuation[c])` per step, so a
    /// transparent material still loses one level per block. Opaque blocks
    /// declare 15 to stop block light.
    pub attenuation: [u8; 3],
    /// Sky-light attenuation (`0..=15`).
    ///
    /// A column's running sky level is reduced by this value at every cell;
    /// 15 zeroes the column below. Air is 0, water and leaves are 1, opaque
    /// blocks are 15. Wheat and torches are 0; beds are 1.
    pub sky_attenuation: u8,
    /// Whether fire may spread to this block (planks, logs, leaves, beds and
    /// the wooden crafting table/chest). Defaults to `false` for packs that do
    /// not declare it, so a custom pack can never make stone burn by omission.
    pub flammable: bool,
    /// Rendered geometry.
    pub shape: BlockShape,
}

impl BlockType {
    /// Whether the player can break this block by hand.
    pub fn breakable(&self) -> bool {
        self.hardness.is_finite()
    }

    /// Whether this block is an ore.
    ///
    /// The interim catalogue marks ores by the `_ore` resource-id suffix, so a
    /// pack can add `openblock:gold_ore` without an engine change. A real
    /// `#c:ores` tag replaces this naming convention with the data-driven
    /// content pipeline.
    pub fn is_ore(&self) -> bool {
        self.id.ends_with("_ore")
    }

    /// Whether a held tool satisfies this block's tier requirement.
    fn tool_satisfies(&self, tool: Option<ToolSpec>) -> bool {
        let Some(preferred) = self.preferred_tool else {
            return self.required_tier == 0;
        };
        tool.is_some_and(|spec| spec.kind == preferred && spec.tier >= self.required_tier)
    }
}

/// A dense, session-local recipe index.
///
/// Recipe ids are positions in the frozen, deterministic recipe list; they are
/// never persisted or networked by numeric value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecipeId(pub u16);

/// One code-defined crafting recipe.
///
/// Shaped recipes are simplified to ingredient totals in this slice; the grid and
/// pattern model from `docs/spec/11-inventory-crafting-items.md` arrives with
/// the data-driven content pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    /// Namespaced recipe id, e.g. `openblock:wooden_pickaxe`.
    pub id: &'static str,
    /// ASCII display name for hints and UI.
    pub display_name: &'static str,
    /// Output item and count.
    pub output: (ItemId, u8),
    /// Required items and counts, as ingredient totals (order significant only
    /// for deterministic diagnostics).
    pub ingredients: &'static [(ItemId, u8)],
    /// Whether a nearby crafting table is required.
    pub needs_table: bool,
}

/// One code-defined furnace smelting recipe.
///
/// This is a pure lookup table: furnace block state, fuel burn progress and the
/// furnace UI are implemented by later host/client agents. One input item is
/// consumed per smelt and the output is produced after
/// [`SMELT_TICKS_PER_ITEM`] ticks of cooking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmeltRecipe {
    /// Item consumed from the furnace input slot.
    pub input: ItemId,
    /// Item produced in the output slot.
    pub output: ItemId,
    /// Number of output items produced per smelt.
    pub output_count: u8,
}

/// Ticks one item takes to smelt in a furnace (20 ticks/s).
pub const SMELT_TICKS_PER_ITEM: u16 = 200;

/// A frozen registry of block and item types for a session.
#[derive(Debug, Clone)]
pub struct Registry {
    blocks: Vec<BlockType>,
    by_id: FxHashMap<&'static str, BlockId>,
    items: Vec<ItemType>,
    item_by_id: FxHashMap<&'static str, ItemId>,
    item_for_block: Vec<Option<ItemId>>,
    recipes: Vec<Recipe>,
    recipe_by_id: FxHashMap<&'static str, RecipeId>,
    smelt_recipes: Vec<SmeltRecipe>,
    smelt_by_input: FxHashMap<ItemId, usize>,
    fuel_ticks: FxHashMap<ItemId, u16>,
    mobs: Vec<MobSpec>,
}

/// Raw definition tuple used while building the base registry.
type BlockDef = (
    &'static str,
    [f32; 3],
    bool,
    bool,
    bool,
    BlockTextures,
    f32,
    Option<ToolKind>,
    u8,
);

/// Block resource ids whose item ids are frozen at their v0.0.7/v0.0.8 values.
///
/// These fourteen block items occupy item ids 0..=10 (the eleven original
/// block items) and ids 12..=14 after the apple. New blocks are appended after
/// `openblock:crafting_table`; their block items are added after the frozen item
/// catalogue at ids 47..=51 so no existing item id moves.
const FROZEN_BLOCK_ITEMS: [&str; 14] = [
    "openblock:stone",
    "openblock:dirt",
    "openblock:grass",
    "openblock:sand",
    "openblock:water",
    "openblock:log",
    "openblock:leaves",
    "openblock:bedrock",
    "openblock:gravel",
    "openblock:clay",
    "openblock:snow",
    "openblock:planks",
    "openblock:cobblestone",
    "openblock:crafting_table",
];

/// Non-block items appended after the apple, in stable id order.
const NON_BLOCK_ITEMS: [(&str, u8, Option<ToolSpec>); 9] = [
    ("openblock:stick", 64, None),
    (
        "openblock:wooden_pickaxe",
        1,
        Some(ToolSpec {
            kind: ToolKind::Pickaxe,
            tier: 1,
            speed: 2.0,
            attack_damage: 2.0,
            durability: 59,
        }),
    ),
    (
        "openblock:wooden_axe",
        1,
        Some(ToolSpec {
            kind: ToolKind::Axe,
            tier: 1,
            speed: 2.0,
            attack_damage: 2.0,
            durability: 59,
        }),
    ),
    (
        "openblock:wooden_shovel",
        1,
        Some(ToolSpec {
            kind: ToolKind::Shovel,
            tier: 1,
            speed: 2.0,
            attack_damage: 2.0,
            durability: 59,
        }),
    ),
    (
        "openblock:wooden_sword",
        1,
        Some(ToolSpec {
            kind: ToolKind::Sword,
            tier: 1,
            speed: 1.0,
            attack_damage: 4.0,
            durability: 59,
        }),
    ),
    (
        "openblock:stone_pickaxe",
        1,
        Some(ToolSpec {
            kind: ToolKind::Pickaxe,
            tier: 2,
            speed: 4.0,
            attack_damage: 3.0,
            durability: 131,
        }),
    ),
    (
        "openblock:stone_axe",
        1,
        Some(ToolSpec {
            kind: ToolKind::Axe,
            tier: 2,
            speed: 4.0,
            attack_damage: 3.0,
            durability: 131,
        }),
    ),
    (
        "openblock:stone_shovel",
        1,
        Some(ToolSpec {
            kind: ToolKind::Shovel,
            tier: 2,
            speed: 4.0,
            attack_damage: 3.0,
            durability: 131,
        }),
    ),
    (
        "openblock:stone_sword",
        1,
        Some(ToolSpec {
            kind: ToolKind::Sword,
            tier: 2,
            speed: 1.0,
            attack_damage: 5.0,
            durability: 131,
        }),
    ),
];

/// One code-defined non-block item in the v0.0.9 catalogue.
struct NewItemDef {
    id: &'static str,
    max_stack: u8,
    food: Option<u8>,
    tool: Option<ToolSpec>,
    armor: Option<ArmorSpec>,
}

/// `Some` tool statistics for a [`NewItemDef`].
const fn tool_spec(kind: ToolKind, tier: u8, speed: f32, attack_damage: f32, durability: u16) -> Option<ToolSpec> {
    Some(ToolSpec {
        kind,
        tier,
        speed,
        attack_damage,
        durability,
    })
}

/// `Some` armor statistics for a [`NewItemDef`].
const fn armor_spec(slot: ArmorSlot, defense: u8, durability: u16) -> Option<ArmorSpec> {
    Some(ArmorSpec {
        slot,
        defense,
        durability,
    })
}

/// New non-block items appended after the v0.0.8 tools, in stable id order.
///
/// These occupy item ids 24..=46. The block items for the five blocks appended
/// after `crafting_table` occupy ids 47..=51.
const NEW_ITEMS: [NewItemDef; 23] = [
    NewItemDef {
        id: "openblock:coal",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:charcoal",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:raw_iron",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_ingot",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:raw_copper",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:copper_ingot",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_pickaxe",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Pickaxe, 3, 6.0, 4.0, 251),
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_axe",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Axe, 3, 6.0, 4.0, 251),
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_shovel",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Shovel, 3, 6.0, 4.0, 251),
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_sword",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Sword, 3, 1.0, 6.0, 251),
        armor: None,
    },
    NewItemDef {
        id: "openblock:leather_helmet",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Helmet, 1, 55),
    },
    NewItemDef {
        id: "openblock:leather_chestplate",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Chestplate, 3, 80),
    },
    NewItemDef {
        id: "openblock:leather_leggings",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Leggings, 2, 75),
    },
    NewItemDef {
        id: "openblock:leather_boots",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Boots, 1, 65),
    },
    NewItemDef {
        id: "openblock:iron_helmet",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Helmet, 2, 165),
    },
    NewItemDef {
        id: "openblock:iron_chestplate",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Chestplate, 6, 240),
    },
    NewItemDef {
        id: "openblock:iron_leggings",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Leggings, 5, 225),
    },
    NewItemDef {
        id: "openblock:iron_boots",
        max_stack: 1,
        food: None,
        tool: None,
        armor: armor_spec(ArmorSlot::Boots, 2, 195),
    },
    NewItemDef {
        id: "openblock:raw_beef",
        max_stack: 64,
        food: Some(3),
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:cooked_beef",
        max_stack: 64,
        food: Some(8),
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:raw_porkchop",
        max_stack: 64,
        food: Some(3),
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:cooked_porkchop",
        max_stack: 64,
        food: Some(8),
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:leather",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
];

/// 1.2 "Light & Home" items, appended after every existing item and after the
/// block items of the new blocks. Ids are stable: seeds/wheat/bread then the
/// three hoes.
const LIGHT_ITEMS: [NewItemDef; 6] = [
    NewItemDef {
        id: "openblock:seeds",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:wheat",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:bread",
        max_stack: 64,
        food: Some(5),
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:wood_hoe",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Hoe, 1, 2.0, 1.0, 59),
        armor: None,
    },
    NewItemDef {
        id: "openblock:stone_hoe",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Hoe, 2, 4.0, 1.0, 131),
        armor: None,
    },
    NewItemDef {
        id: "openblock:iron_hoe",
        max_stack: 1,
        food: None,
        tool: tool_spec(ToolKind::Hoe, 3, 6.0, 1.0, 250),
        armor: None,
    },
];

/// 1.3 "Weather & Danger" items, appended after the 1.2 catalogue.
///
/// Ids are stable: bone, string, gunpowder then arrow. The arrow is craftable
/// from one stick and one string; flint/feathers are out of scope for 1.3.
const DANGER_ITEMS: [NewItemDef; 4] = [
    NewItemDef {
        id: "openblock:bone",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:string",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:gunpowder",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
    NewItemDef {
        id: "openblock:arrow",
        max_stack: 64,
        food: None,
        tool: None,
        armor: None,
    },
];

/// One name-resolved mob definition used to build [`Registry::base`].
///
/// Stats are copied verbatim into [`MobSpec`]; drop item names are resolved
/// against the registry's item table so packs without an item omit the drop.
struct MobDef {
    kind: MobKind,
    display_name: &'static str,
    health: f32,
    speed: f32,
    hostile: bool,
    width: f32,
    height: f32,
    drops: &'static [(&'static str, u8)],
    attack: MobAttack,
    xp_reward: u32,
    neutral_in_light: bool,
    climbs: bool,
    burns_in_daylight: bool,
    projectile: Option<ProjectileKind>,
}

/// The 1.3 mob catalogue, in stable [`MobKind`] order (1.0 kinds keep their
/// indices; spider/skeleton/creeper append).
///
/// Counts are fixed rather than rolled: a kill always produces exactly this
/// list. A skeleton drops one bone and one arrow; a creeper drops one gunpowder;
/// a spider drops one string. No passive mob drops XP more than once, and a
/// baby never drops anything (enforced by the host).
const MOB_DEFS: [MobDef; 8] = [
    MobDef {
        kind: MobKind::Cow,
        display_name: "Cow",
        health: 10.0,
        speed: 1.4,
        hostile: false,
        width: 0.9,
        height: 1.3,
        drops: &[("openblock:raw_beef", 1), ("openblock:leather", 1)],
        attack: MobAttack::None,
        xp_reward: 1,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: false,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Pig,
        display_name: "Pig",
        health: 10.0,
        speed: 1.4,
        hostile: false,
        width: 0.9,
        height: 0.9,
        drops: &[("openblock:raw_porkchop", 1)],
        attack: MobAttack::None,
        xp_reward: 1,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: false,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Sheep,
        display_name: "Sheep",
        health: 8.0,
        speed: 1.4,
        hostile: false,
        width: 0.9,
        height: 1.3,
        drops: &[("openblock:leather", 1)],
        attack: MobAttack::None,
        xp_reward: 1,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: false,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Chicken,
        display_name: "Chicken",
        health: 4.0,
        speed: 1.5,
        hostile: false,
        width: 0.4,
        height: 0.7,
        drops: &[],
        attack: MobAttack::None,
        xp_reward: 1,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: false,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Zombie,
        display_name: "Zombie",
        health: 20.0,
        speed: 2.2,
        hostile: true,
        width: 0.6,
        height: 1.8,
        drops: &[],
        attack: MobAttack::Melee {
            damage: 3.0,
            range: 1.6,
            cooldown: 1.0,
            windup: 0.5,
        },
        xp_reward: 3,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: true,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Spider,
        display_name: "Spider",
        health: 16.0,
        speed: 4.0,
        hostile: true,
        width: 1.0,
        height: 0.9,
        drops: &[("openblock:string", 1)],
        attack: MobAttack::Melee {
            damage: 2.0,
            range: 1.7,
            cooldown: 1.0,
            windup: 0.4,
        },
        xp_reward: 5,
        neutral_in_light: true,
        climbs: true,
        burns_in_daylight: false,
        projectile: None,
    },
    MobDef {
        kind: MobKind::Skeleton,
        display_name: "Skeleton",
        health: 20.0,
        speed: 3.5,
        hostile: true,
        width: 0.6,
        height: 1.8,
        drops: &[("openblock:bone", 1), ("openblock:arrow", 1)],
        attack: MobAttack::Ranged {
            damage: 3.0,
            min_range: 6.0,
            max_range: 12.0,
            cooldown: 2.0,
        },
        xp_reward: 5,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: true,
        projectile: Some(ProjectileKind::Arrow),
    },
    MobDef {
        kind: MobKind::Creeper,
        display_name: "Creeper",
        health: 20.0,
        speed: 3.0,
        hostile: true,
        width: 0.6,
        height: 1.8,
        drops: &[("openblock:gunpowder", 1)],
        attack: MobAttack::Explosive {
            trigger_range: 3.0,
            fuse: 1.5,
            radius: 3.5,
            max_damage: 12.0,
        },
        xp_reward: 5,
        neutral_in_light: false,
        climbs: false,
        burns_in_daylight: false,
        projectile: None,
    },
];

/// One name-resolved smelting definition used to build [`Registry::base`].
struct SmeltDef {
    input: &'static str,
    output: &'static str,
    output_count: u8,
}

/// The interim furnace smelting table, in deterministic recipe order.
///
/// Glass is intentionally out of scope for 1.0, so `sand` is not smeltable.
/// `cobblestone -> stone` is the one reversible-smelting convenience included
/// in this slice.
const BASE_SMELT_RECIPES: [SmeltDef; 6] = [
    SmeltDef {
        input: "openblock:raw_iron",
        output: "openblock:iron_ingot",
        output_count: 1,
    },
    SmeltDef {
        input: "openblock:raw_copper",
        output: "openblock:copper_ingot",
        output_count: 1,
    },
    SmeltDef {
        input: "openblock:log",
        output: "openblock:charcoal",
        output_count: 1,
    },
    SmeltDef {
        input: "openblock:raw_beef",
        output: "openblock:cooked_beef",
        output_count: 1,
    },
    SmeltDef {
        input: "openblock:raw_porkchop",
        output: "openblock:cooked_porkchop",
        output_count: 1,
    },
    SmeltDef {
        input: "openblock:cobblestone",
        output: "openblock:stone",
        output_count: 1,
    },
];

/// The interim fuel table: burn time in ticks.
///
/// One coal burns for 1,600 ticks, exactly eight 200-tick smelts. Planks and
/// logs burn for 300 ticks (one and a half smelts) and sticks for 100 ticks.
const BASE_FUELS: [(&str, u16); 5] = [
    ("openblock:coal", 1_600),
    ("openblock:charcoal", 1_600),
    ("openblock:planks", 300),
    ("openblock:stick", 100),
    ("openblock:log", 300),
];

/// One name-resolved recipe definition used to build [`Registry::base`].
struct RecipeDef {
    id: &'static str,
    name: &'static str,
    output: (&'static str, u8),
    ingredients: &'static [(&'static str, u8)],
    needs_table: bool,
}

const BASE_RECIPES: [RecipeDef; 31] = [
    RecipeDef {
        id: "openblock:planks",
        name: "Planks",
        output: ("openblock:planks", 4),
        ingredients: &[("openblock:log", 1)],
        needs_table: false,
    },
    RecipeDef {
        id: "openblock:sticks",
        name: "Sticks",
        output: ("openblock:stick", 4),
        ingredients: &[("openblock:planks", 2)],
        needs_table: false,
    },
    RecipeDef {
        id: "openblock:crafting_table",
        name: "Crafting Table",
        output: ("openblock:crafting_table", 1),
        ingredients: &[("openblock:planks", 4)],
        needs_table: false,
    },
    RecipeDef {
        id: "openblock:wooden_pickaxe",
        name: "Wooden Pickaxe",
        output: ("openblock:wooden_pickaxe", 1),
        ingredients: &[("openblock:planks", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:wooden_axe",
        name: "Wooden Axe",
        output: ("openblock:wooden_axe", 1),
        ingredients: &[("openblock:planks", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:wooden_shovel",
        name: "Wooden Shovel",
        output: ("openblock:wooden_shovel", 1),
        ingredients: &[("openblock:planks", 1), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:wooden_sword",
        name: "Wooden Sword",
        output: ("openblock:wooden_sword", 1),
        ingredients: &[("openblock:planks", 2), ("openblock:stick", 1)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:stone_pickaxe",
        name: "Stone Pickaxe",
        output: ("openblock:stone_pickaxe", 1),
        ingredients: &[("openblock:cobblestone", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:stone_axe",
        name: "Stone Axe",
        output: ("openblock:stone_axe", 1),
        ingredients: &[("openblock:cobblestone", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:stone_shovel",
        name: "Stone Shovel",
        output: ("openblock:stone_shovel", 1),
        ingredients: &[("openblock:cobblestone", 1), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:stone_sword",
        name: "Stone Sword",
        output: ("openblock:stone_sword", 1),
        ingredients: &[("openblock:cobblestone", 2), ("openblock:stick", 1)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:furnace",
        name: "Furnace",
        output: ("openblock:furnace", 1),
        ingredients: &[("openblock:cobblestone", 8)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:chest",
        name: "Chest",
        output: ("openblock:chest", 1),
        ingredients: &[("openblock:planks", 8)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_pickaxe",
        name: "Iron Pickaxe",
        output: ("openblock:iron_pickaxe", 1),
        ingredients: &[("openblock:iron_ingot", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_axe",
        name: "Iron Axe",
        output: ("openblock:iron_axe", 1),
        ingredients: &[("openblock:iron_ingot", 3), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_shovel",
        name: "Iron Shovel",
        output: ("openblock:iron_shovel", 1),
        ingredients: &[("openblock:iron_ingot", 1), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_sword",
        name: "Iron Sword",
        output: ("openblock:iron_sword", 1),
        ingredients: &[("openblock:iron_ingot", 2), ("openblock:stick", 1)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:leather_helmet",
        name: "Leather Helmet",
        output: ("openblock:leather_helmet", 1),
        ingredients: &[("openblock:leather", 5)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:leather_chestplate",
        name: "Leather Chestplate",
        output: ("openblock:leather_chestplate", 1),
        ingredients: &[("openblock:leather", 8)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:leather_leggings",
        name: "Leather Leggings",
        output: ("openblock:leather_leggings", 1),
        ingredients: &[("openblock:leather", 7)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:leather_boots",
        name: "Leather Boots",
        output: ("openblock:leather_boots", 1),
        ingredients: &[("openblock:leather", 4)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_helmet",
        name: "Iron Helmet",
        output: ("openblock:iron_helmet", 1),
        ingredients: &[("openblock:iron_ingot", 5)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_chestplate",
        name: "Iron Chestplate",
        output: ("openblock:iron_chestplate", 1),
        ingredients: &[("openblock:iron_ingot", 8)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_leggings",
        name: "Iron Leggings",
        output: ("openblock:iron_leggings", 1),
        ingredients: &[("openblock:iron_ingot", 7)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_boots",
        name: "Iron Boots",
        output: ("openblock:iron_boots", 1),
        ingredients: &[("openblock:iron_ingot", 4)],
        needs_table: true,
    },
    // 1.2 "Light & Home" recipes, append-only after the 1.0 table.
    RecipeDef {
        id: "openblock:torch",
        name: "Torch",
        output: ("openblock:torch", 4),
        ingredients: &[("openblock:coal", 1), ("openblock:stick", 1)],
        needs_table: false,
    },
    RecipeDef {
        id: "openblock:bread",
        name: "Bread",
        output: ("openblock:bread", 1),
        ingredients: &[("openblock:wheat", 3)],
        needs_table: false,
    },
    RecipeDef {
        id: "openblock:wood_hoe",
        name: "Wooden Hoe",
        output: ("openblock:wood_hoe", 1),
        ingredients: &[("openblock:planks", 2), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:stone_hoe",
        name: "Stone Hoe",
        output: ("openblock:stone_hoe", 1),
        ingredients: &[("openblock:cobblestone", 2), ("openblock:stick", 2)],
        needs_table: true,
    },
    RecipeDef {
        id: "openblock:iron_hoe",
        name: "Iron Hoe",
        output: ("openblock:iron_hoe", 1),
        ingredients: &[("openblock:iron_ingot", 2), ("openblock:stick", 2)],
        needs_table: true,
    },
    // 1.3 "Weather & Danger" recipe, append-only.
    RecipeDef {
        id: "openblock:arrow",
        name: "Arrows",
        output: ("openblock:arrow", 4),
        ingredients: &[("openblock:stick", 1), ("openblock:string", 1)],
        needs_table: false,
    },
];

impl Registry {
    /// Builds the first-party base block set.
    pub fn base() -> Self {
        use textures as tex;
        let defs: [BlockDef; 20] = [
            (
                "openblock:air",
                [0.0, 0.0, 0.0],
                false,
                false,
                false,
                BlockTextures::all(tex::DEBUG),
                0.0,
                None,
                0,
            ),
            (
                "openblock:stone",
                [0.48, 0.48, 0.50],
                true,
                true,
                false,
                BlockTextures::all(tex::STONE),
                1.5,
                Some(ToolKind::Pickaxe),
                1,
            ),
            (
                "openblock:dirt",
                [0.42, 0.30, 0.20],
                true,
                true,
                false,
                BlockTextures::all(tex::DIRT),
                0.5,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:grass",
                [0.33, 0.55, 0.25],
                true,
                true,
                false,
                BlockTextures::column(tex::GRASS_TOP, tex::GRASS_SIDE, tex::DIRT),
                0.6,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:sand",
                [0.85, 0.80, 0.55],
                true,
                true,
                false,
                BlockTextures::all(tex::SAND),
                0.5,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:water",
                [0.20, 0.42, 0.75],
                false,
                false,
                true,
                BlockTextures::all(tex::WATER),
                f32::INFINITY,
                None,
                0,
            ),
            (
                "openblock:log",
                [0.36, 0.26, 0.15],
                true,
                true,
                false,
                BlockTextures::column(tex::LOG_TOP, tex::LOG_SIDE, tex::LOG_TOP),
                2.0,
                Some(ToolKind::Axe),
                0,
            ),
            (
                "openblock:leaves",
                [0.18, 0.40, 0.16],
                true,
                false,
                false,
                BlockTextures::all(tex::LEAVES),
                0.2,
                None,
                0,
            ),
            (
                "openblock:bedrock",
                [0.15, 0.15, 0.17],
                true,
                true,
                false,
                BlockTextures::all(tex::BEDROCK),
                f32::INFINITY,
                None,
                0,
            ),
            (
                "openblock:gravel",
                [0.45, 0.43, 0.41],
                true,
                true,
                false,
                BlockTextures::all(tex::GRAVEL),
                0.6,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:clay",
                [0.55, 0.56, 0.58],
                true,
                true,
                false,
                BlockTextures::all(tex::CLAY),
                0.6,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:snow",
                [0.92, 0.94, 0.97],
                true,
                true,
                false,
                BlockTextures::all(tex::SNOW),
                0.1,
                Some(ToolKind::Shovel),
                0,
            ),
            (
                "openblock:planks",
                [0.64, 0.50, 0.30],
                true,
                true,
                false,
                BlockTextures::all(tex::PLANKS),
                2.0,
                Some(ToolKind::Axe),
                0,
            ),
            (
                "openblock:cobblestone",
                [0.46, 0.46, 0.48],
                true,
                true,
                false,
                BlockTextures::all(tex::COBBLESTONE),
                2.0,
                Some(ToolKind::Pickaxe),
                1,
            ),
            (
                "openblock:crafting_table",
                [0.55, 0.41, 0.23],
                true,
                true,
                false,
                BlockTextures::column(tex::CRAFTING_TABLE_TOP, tex::CRAFTING_TABLE_SIDE, tex::PLANKS),
                2.5,
                Some(ToolKind::Axe),
                0,
            ),
            (
                "openblock:coal_ore",
                [0.25, 0.25, 0.27],
                true,
                true,
                false,
                BlockTextures::all(tex::COAL_ORE),
                3.0,
                Some(ToolKind::Pickaxe),
                1,
            ),
            (
                "openblock:iron_ore",
                [0.64, 0.56, 0.47],
                true,
                true,
                false,
                BlockTextures::all(tex::IRON_ORE),
                3.0,
                Some(ToolKind::Pickaxe),
                2,
            ),
            (
                "openblock:copper_ore",
                [0.66, 0.45, 0.32],
                true,
                true,
                false,
                BlockTextures::all(tex::COPPER_ORE),
                3.0,
                Some(ToolKind::Pickaxe),
                2,
            ),
            (
                "openblock:furnace",
                [0.42, 0.42, 0.44],
                true,
                true,
                false,
                BlockTextures::fronted(
                    tex::FURNACE_TOP,
                    tex::FURNACE_SIDE,
                    tex::FURNACE_TOP,
                    tex::FURNACE_FRONT,
                ),
                3.5,
                Some(ToolKind::Axe),
                0,
            ),
            (
                "openblock:chest",
                [0.60, 0.42, 0.22],
                true,
                true,
                false,
                BlockTextures::fronted(tex::CHEST_TOP, tex::CHEST_SIDE, tex::CHEST_TOP, tex::CHEST_FRONT),
                2.5,
                Some(ToolKind::Axe),
                0,
            ),
        ];
        // Existing 1.1 blocks keep their ids and appearance. Light defaults
        // follow the documented material classes: opaque blocks stop sky and
        // block light, water attenuates both by one, air and non-solid plants
        // are transparent. Leaves are the one existing exception (solid but not
        // opaque) and get the explicit 1/1 attenuation below.
        let mut blocks: Vec<BlockType> = defs
            .into_iter()
            .map(
                |(id, color, solid, opaque, fluid, textures, hardness, preferred_tool, required_tier)| BlockType {
                    id,
                    color,
                    solid,
                    opaque,
                    fluid,
                    textures,
                    hardness,
                    preferred_tool,
                    required_tier,
                    emission: [0; 3],
                    attenuation: if opaque {
                        [15; 3]
                    } else if fluid {
                        [1; 3]
                    } else {
                        [0; 3]
                    },
                    sky_attenuation: if opaque {
                        15
                    } else if fluid {
                        1
                    } else {
                        0
                    },
                    flammable: false,
                    shape: BlockShape::Cube,
                },
            )
            .collect();
        for block in &mut blocks {
            if block.id == "openblock:leaves" {
                block.attenuation = [1; 3];
                block.sky_attenuation = 1;
            }
            // 1.3 flammability: the wooden building blocks, natural wood and
            // both bed halves. Wool is not in the palette; a future wool block
            // should declare `flammable: true` in its own definition.
            block.flammable = matches!(
                block.id,
                "openblock:planks"
                    | "openblock:log"
                    | "openblock:leaves"
                    | "openblock:crafting_table"
                    | "openblock:chest"
                    | "openblock:bed_foot"
                    | "openblock:bed_head"
            );
        }
        // 1.2 "Light & Home" content delta. Ids are append-only: torch 20,
        // farmland 21-22, wheat stages 23-26, bed 27-28. None of these blocks
        // carry block states yet; the server owns support, tilling, growth and
        // bed-pair semantics in a later slice.
        blocks.extend([
            BlockType {
                id: "openblock:torch",
                color: [1.0, 0.78, 0.32],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::TORCH),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [14, 14, 14],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
            BlockType {
                id: "openblock:farmland_dry",
                color: [0.36, 0.24, 0.14],
                solid: true,
                opaque: true,
                fluid: false,
                textures: BlockTextures::all(tex::FARMLAND_DRY),
                hardness: 0.6,
                preferred_tool: Some(ToolKind::Shovel),
                required_tier: 0,
                emission: [0; 3],
                attenuation: [15; 3],
                sky_attenuation: 15,
                flammable: false,
                shape: BlockShape::Cube,
            },
            BlockType {
                id: "openblock:farmland_wet",
                color: [0.23, 0.16, 0.10],
                solid: true,
                opaque: true,
                fluid: false,
                textures: BlockTextures::all(tex::FARMLAND_WET),
                hardness: 0.6,
                preferred_tool: Some(ToolKind::Shovel),
                required_tier: 0,
                emission: [0; 3],
                attenuation: [15; 3],
                sky_attenuation: 15,
                flammable: false,
                shape: BlockShape::Cube,
            },
            BlockType {
                id: "openblock:wheat_0",
                color: [0.42, 0.62, 0.22],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::WHEAT_0),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
            BlockType {
                id: "openblock:wheat_1",
                color: [0.52, 0.66, 0.24],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::WHEAT_1),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
            BlockType {
                id: "openblock:wheat_2",
                color: [0.64, 0.68, 0.26],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::WHEAT_2),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
            BlockType {
                id: "openblock:wheat_3",
                color: [0.76, 0.66, 0.28],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::WHEAT_3),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
            // Beds collide as a full cube in 1.2 (no partial shapes yet) but
            // stay non-opaque so their blanket, sides and pillow render as a
            // distinct block instead of hiding against the floor.
            BlockType {
                id: "openblock:bed_foot",
                color: [0.55, 0.12, 0.12],
                solid: true,
                opaque: false,
                fluid: false,
                textures: BlockTextures::column(tex::BED_FOOT_TOP, tex::BED_FOOT_SIDE, tex::PLANKS),
                hardness: 0.2,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [1; 3],
                sky_attenuation: 1,
                flammable: true,
                shape: BlockShape::Cube,
            },
            BlockType {
                id: "openblock:bed_head",
                color: [0.66, 0.22, 0.21],
                solid: true,
                opaque: false,
                fluid: false,
                textures: BlockTextures::column(tex::BED_HEAD_TOP, tex::BED_HEAD_SIDE, tex::PLANKS),
                hardness: 0.2,
                preferred_tool: None,
                required_tier: 0,
                emission: [0; 3],
                attenuation: [1; 3],
                sky_attenuation: 1,
                flammable: true,
                shape: BlockShape::Cube,
            },
            // 1.3 "Weather & Danger" content delta: fire id 29. A non-solid
            // cross that emits full block light and is replaced by air when it
            // burns out or is extinguished. The server owns ignition, spread,
            // burn-out and damage; the block carries no timers itself.
            BlockType {
                id: "openblock:fire",
                color: [1.0, 0.56, 0.12],
                solid: false,
                opaque: false,
                fluid: false,
                textures: BlockTextures::all(tex::FIRE),
                hardness: 0.0,
                preferred_tool: None,
                required_tier: 0,
                emission: [15, 15, 15],
                attenuation: [0; 3],
                sky_attenuation: 0,
                flammable: false,
                shape: BlockShape::Cross,
            },
        ]);
        Self::from_blocks(blocks).expect("the built-in base block set is valid")
    }

    /// Builds a frozen registry from an ordered list of block types.
    ///
    /// This is the single construction path for registries: [`Registry::base`]
    /// and [`registry_from_pack`] both call it. The first block must be
    /// `openblock:air`, ids must be unique, and the list must be non-empty and
    /// no larger than [`MAX_BLOCKS`].
    pub fn from_blocks(blocks: Vec<BlockType>) -> Result<Self, PackError> {
        if blocks.is_empty() {
            return Err(PackError::MissingAir { path: None });
        }
        if blocks.len() > MAX_BLOCKS {
            return Err(PackError::TooManyBlocks {
                path: None,
                count: blocks.len(),
                limit: MAX_BLOCKS,
            });
        }
        if blocks[0].id != pack::AIR_ID {
            return Err(PackError::InvalidOrder {
                path: None,
                id: blocks[0].id.to_owned(),
            });
        }

        let mut by_id = FxHashMap::default();
        for (index, block) in blocks.iter().enumerate() {
            if let Some(first) = by_id.insert(block.id, BlockId(index as u16)) {
                return Err(PackError::DuplicateId {
                    path: None,
                    id: block.id.to_owned(),
                    first: first.0 as usize,
                    second: index,
                });
            }
        }

        // Interim code-defined item catalogue. Item ids are append-only: the
        // v0.0.7/v0.0.8 items keep their frozen positions (0..=23), the v0.0.9
        // non-block items occupy 24..=46, and the block items for the five
        // blocks appended after `crafting_table` occupy 47..=51. Data-driven
        // item packs replace this derivation without changing the lookup API.
        fn add_block_item(
            blocks: &[BlockType],
            block_index: usize,
            items: &mut Vec<ItemType>,
            item_by_id: &mut FxHashMap<&'static str, ItemId>,
            item_for_block: &mut [Option<ItemId>],
        ) {
            let block = &blocks[block_index];
            if block.id == pack::AIR_ID || item_for_block[block_index].is_some() || item_by_id.contains_key(block.id) {
                return;
            }
            let item_id = ItemId(items.len() as u16);
            items.push(ItemType {
                id: block.id,
                block: Some(BlockId(block_index as u16)),
                food: None,
                tool: None,
                armor: None,
                max_stack: MAX_ITEM_STACK,
            });
            item_by_id.insert(block.id, item_id);
            item_for_block[block_index] = Some(item_id);
        }

        fn add_item(
            items: &mut Vec<ItemType>,
            item_by_id: &mut FxHashMap<&'static str, ItemId>,
            id: &'static str,
            food: Option<u8>,
            tool: Option<ToolSpec>,
            armor: Option<ArmorSpec>,
            max_stack: u8,
        ) {
            if item_by_id.contains_key(id) {
                return;
            }
            item_by_id.insert(id, ItemId(items.len() as u16));
            items.push(ItemType {
                id,
                block: None,
                food,
                tool,
                armor,
                max_stack,
            });
        }

        let capacity = blocks.len() + NON_BLOCK_ITEMS.len() + NEW_ITEMS.len() + LIGHT_ITEMS.len() + DANGER_ITEMS.len();
        let mut items: Vec<ItemType> = Vec::with_capacity(capacity);
        let mut item_by_id = FxHashMap::default();
        let mut item_for_block = vec![None; blocks.len()];

        // Item ids 0..=10: the eleven original v0.0.7 block items.
        for name in &FROZEN_BLOCK_ITEMS[..11] {
            if let Some(block_index) = by_id.get(name) {
                add_block_item(
                    &blocks,
                    block_index.0 as usize,
                    &mut items,
                    &mut item_by_id,
                    &mut item_for_block,
                );
            }
        }
        // Item id 11: the apple. A pack may legitimately define a block with
        // this resource id; in that case the block item wins and the interim
        // apple is omitted rather than making the registry invalid.
        const APPLE_ID: &str = "openblock:apple";
        if let Some(block_index) = by_id.get(APPLE_ID) {
            add_block_item(
                &blocks,
                block_index.0 as usize,
                &mut items,
                &mut item_by_id,
                &mut item_for_block,
            );
        } else {
            add_item(&mut items, &mut item_by_id, APPLE_ID, Some(6), None, None, 16);
        }
        // Item ids 12..=14: the v0.0.8 block items.
        for name in &FROZEN_BLOCK_ITEMS[11..] {
            if let Some(block_index) = by_id.get(name) {
                add_block_item(
                    &blocks,
                    block_index.0 as usize,
                    &mut items,
                    &mut item_by_id,
                    &mut item_for_block,
                );
            }
        }

        // Item ids 15..=23: the v0.0.8 non-block items.
        for (name, max_stack, tool) in NON_BLOCK_ITEMS {
            add_item(&mut items, &mut item_by_id, name, None, tool, None, max_stack);
        }

        // Item ids 24..=46: the v0.0.9 ores/metals, iron tools, armor and food.
        for def in NEW_ITEMS {
            add_item(
                &mut items,
                &mut item_by_id,
                def.id,
                def.food,
                def.tool,
                def.armor,
                def.max_stack,
            );
        }

        // Item ids 47..=51: block items for the 1.0 blocks appended after the
        // frozen catalogue; 1.2's nine new blocks take the next free ids
        // (52..=60). Stop at the 1.4 block count (29) so later block additions
        // cannot move the frozen 1.2/1.3 item ids.
        let frozen_block_count = 29usize.min(blocks.len());
        for block_index in 0..frozen_block_count {
            add_block_item(&blocks, block_index, &mut items, &mut item_by_id, &mut item_for_block);
        }

        // 1.2 non-block items append after every frozen block item, so the
        // existing item ids 0..=60 cannot move when blocks are appended later.
        for def in LIGHT_ITEMS {
            add_item(
                &mut items,
                &mut item_by_id,
                def.id,
                def.food,
                def.tool,
                def.armor,
                def.max_stack,
            );
        }

        // 1.3 "Weather & Danger" items append after the 1.2 catalogue.
        for def in DANGER_ITEMS {
            add_item(
                &mut items,
                &mut item_by_id,
                def.id,
                def.food,
                def.tool,
                def.armor,
                def.max_stack,
            );
        }

        // 1.5 weather slice: block items for blocks appended after the 1.4
        // catalogue (fire id 29) go after every frozen item, so the 1.2/1.3
        // non-block ids do not move. Custom-pack blocks that are neither in the
        // frozen catalogue nor among the 1.4 blocks follow in block-index order.
        for block_index in frozen_block_count..blocks.len() {
            add_block_item(&blocks, block_index, &mut items, &mut item_by_id, &mut item_for_block);
        }

        // Resolve the base recipe table against this registry. A custom pack
        // that lacks an ingredient or output simply omits that recipe; the base
        // pack contains everything, so all 25 are present there.
        let mut recipes = Vec::with_capacity(BASE_RECIPES.len());
        let mut recipe_by_id = FxHashMap::default();
        for def in BASE_RECIPES {
            let Some(&output) = item_by_id.get(def.output.0) else {
                continue;
            };
            let mut ingredients: Vec<(ItemId, u8)> = Vec::with_capacity(def.ingredients.len());
            let mut complete = true;
            for (name, count) in def.ingredients {
                let Some(&item) = item_by_id.get(*name) else {
                    complete = false;
                    break;
                };
                // Collapse duplicate ingredient entries so host validation can
                // sum one requirement per item id instead of double-counting.
                if let Some(existing) = ingredients.iter_mut().find(|(id, _)| *id == item) {
                    existing.1 = existing.1.saturating_add(*count);
                } else {
                    ingredients.push((item, *count));
                }
            }
            if !complete {
                continue;
            }
            let recipe_id = RecipeId(recipes.len() as u16);
            let leaked: &'static [(ItemId, u8)] = Box::leak(ingredients.into_boxed_slice());
            recipes.push(Recipe {
                id: def.id,
                display_name: def.name,
                output: (output, def.output.1),
                ingredients: leaked,
                needs_table: def.needs_table,
            });
            recipe_by_id.insert(def.id, recipe_id);
        }

        // Resolve the interim smelting and fuel tables. Like recipes, entries
        // whose item is absent from a custom pack are simply omitted.
        let mut smelt_recipes = Vec::with_capacity(BASE_SMELT_RECIPES.len());
        let mut smelt_by_input = FxHashMap::default();
        for def in BASE_SMELT_RECIPES {
            let (Some(&input), Some(&output)) = (item_by_id.get(def.input), item_by_id.get(def.output)) else {
                continue;
            };
            smelt_by_input.insert(input, smelt_recipes.len());
            smelt_recipes.push(SmeltRecipe {
                input,
                output,
                output_count: def.output_count,
            });
        }
        let mut fuel_ticks = FxHashMap::default();
        for (name, ticks) in BASE_FUELS {
            if let Some(&item) = item_by_id.get(name) {
                fuel_ticks.insert(item, ticks);
            }
        }

        // Resolve the interim mob catalogue against this registry. Every base
        // kind is always present; only a drop whose item is absent from a
        // custom pack is omitted, so a pack cannot make `mob(kind)` panic.
        let mut mobs = Vec::with_capacity(MOB_DEFS.len());
        for def in MOB_DEFS {
            let mut drops: Vec<(ItemId, u8)> = Vec::with_capacity(def.drops.len());
            for (name, count) in def.drops {
                if let Some(&item) = item_by_id.get(*name) {
                    drops.push((item, *count));
                }
            }
            let leaked: &'static [(ItemId, u8)] = Box::leak(drops.into_boxed_slice());
            mobs.push(MobSpec {
                kind: def.kind,
                display_name: def.display_name,
                health: def.health,
                speed: def.speed,
                hostile: def.hostile,
                width: def.width,
                height: def.height,
                drops: leaked,
                attack: def.attack,
                xp_reward: def.xp_reward,
                neutral_in_light: def.neutral_in_light,
                climbs: def.climbs,
                burns_in_daylight: def.burns_in_daylight,
                projectile: def.projectile,
            });
        }
        debug_assert_eq!(mobs.len(), MobKind::ALL.len());

        Ok(Self {
            blocks,
            by_id,
            items,
            item_by_id,
            item_for_block,
            recipes,
            recipe_by_id,
            smelt_recipes,
            smelt_by_input,
            fuel_ticks,
            mobs,
        })
    }

    /// Looks up a block type by dense id.
    pub fn block(&self, id: BlockId) -> &BlockType {
        &self.blocks[id.0 as usize]
    }

    /// Looks up a block id by resource id.
    pub fn id(&self, id: &str) -> Option<BlockId> {
        self.by_id.get(id).copied()
    }

    /// Looks up an item type by dense id.
    ///
    /// # Panics
    ///
    /// Panics when `id` was not produced by this registry's item table.
    pub fn item(&self, id: ItemId) -> &ItemType {
        &self.items[id.0 as usize]
    }

    /// Looks up an item id by namespaced resource id.
    pub fn item_id(&self, name: &str) -> Option<ItemId> {
        self.item_by_id.get(name).copied()
    }

    /// The block item for a block, if the block has one (air does not).
    pub fn item_for_block(&self, block: BlockId) -> Option<ItemId> {
        self.item_for_block.get(block.0 as usize).copied().flatten()
    }

    /// Number of registered item types.
    pub fn item_len(&self) -> usize {
        self.items.len()
    }

    /// Iterates over the interim mob catalogue in stable [`MobKind`] order.
    pub fn mobs(&self) -> impl Iterator<Item = &MobSpec> + '_ {
        self.mobs.iter()
    }

    /// The definition for one mob kind.
    ///
    /// The base registry always contains every [`MobKind`]; a custom block pack
    /// cannot remove a kind, only individual drop items. The table is indexed by
    /// [`MobKind::index`].
    pub fn mob(&self, kind: MobKind) -> &MobSpec {
        &self.mobs[kind.index()]
    }

    /// The definition for a namespaced mob id (`openblock:cow`, ...).
    pub fn mob_by_name(&self, name: &str) -> Option<&MobSpec> {
        MobKind::from_name(name).map(|kind| self.mob(kind))
    }

    /// Parses a namespaced mob id into a kind.
    ///
    /// This is the save-facing lookup: unknown ids return `None` and the mob
    /// loader skips the line rather than inventing a kind.
    pub fn mob_id(&self, name: &str) -> Option<MobKind> {
        MobKind::from_name(name)
    }

    /// Number of registered mob kinds.
    pub fn mob_len(&self) -> usize {
        self.mobs.len()
    }

    /// Base melee damage for a selected item, or hand damage.
    ///
    /// A hand (`None`) and every non-tool item deal `1.0` health point. A tool
    /// deals its [`ToolSpec::attack_damage`], so a sword is best and a pickaxe,
    /// axe or shovel still improves on a fist. An out-of-range id also falls
    /// back to `1.0` instead of panicking.
    pub fn attack_damage(&self, held: Option<ItemId>) -> f32 {
        held.filter(|item| (item.0 as usize) < self.items.len())
            .and_then(|item| self.items[item.0 as usize].tool)
            .map_or(1.0, |spec| spec.attack_damage)
    }

    /// Iterates over all item types with their ids.
    pub fn items(&self) -> impl Iterator<Item = (ItemId, &ItemType)> + '_ {
        self.items.iter().enumerate().map(|(i, item)| (ItemId(i as u16), item))
    }

    /// Looks up a recipe by dense id.
    pub fn recipe(&self, id: RecipeId) -> Option<&Recipe> {
        self.recipes.get(id.0 as usize)
    }

    /// Looks up a recipe id by namespaced resource id.
    pub fn recipe_id(&self, id: &str) -> Option<RecipeId> {
        self.recipe_by_id.get(id).copied()
    }

    /// Iterates over all recipes in deterministic recipe-id order.
    pub fn recipes(&self) -> impl Iterator<Item = (RecipeId, &Recipe)> + '_ {
        self.recipes
            .iter()
            .enumerate()
            .map(|(index, recipe)| (RecipeId(index as u16), recipe))
    }

    /// Number of registered recipes.
    pub fn recipe_len(&self) -> usize {
        self.recipes.len()
    }

    /// Iterates over the interim furnace smelting table in deterministic order.
    pub fn smelt_recipes(&self) -> impl Iterator<Item = &SmeltRecipe> + '_ {
        self.smelt_recipes.iter()
    }

    /// The smelting result for `input`, if the item is smeltable.
    ///
    /// Returns the output item and count; cooking progress and the furnace
    /// block state belong to the host agent.
    pub fn smelt_recipe_for(&self, input: ItemId) -> Option<(ItemId, u8)> {
        self.smelt_by_input
            .get(&input)
            .map(|&index| (self.smelt_recipes[index].output, self.smelt_recipes[index].output_count))
    }

    /// The output item smelted from `input`, ignoring the output count.
    ///
    /// Convenience for recipe hints and UI; use [`Self::smelt_recipe_for`] when
    /// the count matters.
    pub fn smelt_result_for(&self, input: ItemId) -> Option<ItemId> {
        self.smelt_recipe_for(input).map(|(output, _)| output)
    }

    /// Burn time of `item` as furnace fuel, in ticks (`20` ticks/s).
    ///
    /// One item smelts per [`SMELT_TICKS_PER_ITEM`] ticks, so one coal
    /// (`1_600`) is worth eight smelts.
    pub fn fuel_ticks(&self, item: ItemId) -> Option<u16> {
        self.fuel_ticks.get(&item).copied()
    }

    /// Whether `item` is registered as a furnace fuel.
    pub fn is_fuel(&self, item: ItemId) -> bool {
        self.fuel_ticks.contains_key(&item)
    }

    /// Whether `item` has a furnace smelting recipe.
    pub fn is_smeltable(&self, item: ItemId) -> bool {
        self.smelt_by_input.contains_key(&item)
    }

    /// The mining-speed multiplier for `block` with `held` selected.
    ///
    /// Returns `1.0` for a hand or a wrong/under-tier tool and the tool's
    /// [`ToolSpec::speed`] when the held tool matches the block's
    /// [`BlockType::preferred_tool`] and satisfies its tier. Unbreakable blocks
    /// have no speed semantics; callers should check [`BlockType::breakable`].
    pub fn mining_speed(&self, block: BlockId, held: Option<ItemId>) -> f32 {
        let block_type = self.block(block);
        let tool = held
            .filter(|item| (item.0 as usize) < self.items.len())
            .and_then(|item| self.items[item.0 as usize].tool);
        match (block_type.preferred_tool, tool) {
            (Some(preferred), Some(spec)) if spec.kind == preferred && spec.tier >= block_type.required_tier => {
                spec.speed
            }
            _ => 1.0,
        }
    }

    /// Whether breaking `block` with `held` selected yields its loot.
    ///
    /// A block with `required_tier == 0` always drops (unless its drop table is
    /// empty); a tiered block needs a tool of the preferred kind at or above the
    /// required tier. Air and unbreakable blocks never drop.
    pub fn can_drop(&self, block: BlockId, held: Option<ItemId>) -> bool {
        let block_type = self.block(block);
        if block == BlockId::AIR || !block_type.breakable() {
            return false;
        }
        if block_type.required_tier == 0 {
            return true;
        }
        let tool = held
            .filter(|item| (item.0 as usize) < self.items.len())
            .and_then(|item| self.items[item.0 as usize].tool);
        block_type.tool_satisfies(tool)
    }

    /// Effective seconds to mine `block` with `held` selected.
    ///
    /// This is `hardness / mining_speed`; unbreakable blocks return `0.0`.
    pub fn effective_mining_seconds(&self, block: BlockId, held: Option<ItemId>) -> f32 {
        let block_type = self.block(block);
        if !block_type.breakable() {
            return 0.0;
        }
        block_type.hardness / self.mining_speed(block, held)
    }

    /// The interim block drop table: what breaking `block` yields without a
    /// tool-tier check.
    ///
    /// Most blocks drop themselves once; stone drops cobblestone, grass drops
    /// dirt, ores drop their raw/coal item, leaves drop nothing here (the Host
    /// adds a deterministic rare apple), and unbreakable blocks (water, bedrock)
    /// and air drop nothing. A block loot table in the content pack will replace
    /// these hard-coded rules later; callers must still apply
    /// [`Registry::can_drop`] for tiering.
    pub fn block_drop(&self, block: BlockId) -> Option<(ItemId, u8)> {
        let block_type = self.block(block);
        if block == BlockId::AIR || !block_type.breakable() {
            return None;
        }
        match block_type.id {
            "openblock:stone" => self
                .item_id("openblock:cobblestone")
                .or_else(|| self.item_for_block(block))
                .map(|item| (item, 1)),
            "openblock:grass" => self.item_id("openblock:dirt").map(|item| (item, 1)),
            "openblock:leaves" => None,
            "openblock:coal_ore" => self
                .item_id("openblock:coal")
                .or_else(|| self.item_for_block(block))
                .map(|item| (item, 1)),
            "openblock:iron_ore" => self
                .item_id("openblock:raw_iron")
                .or_else(|| self.item_for_block(block))
                .map(|item| (item, 1)),
            "openblock:copper_ore" => self
                .item_id("openblock:raw_copper")
                .or_else(|| self.item_for_block(block))
                .map(|item| (item, 1)),
            _ => self.item_for_block(block).map(|item| (item, 1)),
        }
    }

    /// Number of registered block types.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Iterates over all block types with their ids.
    pub fn iter(&self) -> impl Iterator<Item = (BlockId, &BlockType)> + '_ {
        self.blocks.iter().enumerate().map(|(i, b)| (BlockId(i as u16), b))
    }

    /// Convenience: block id for the base blocks used by world generation.
    pub fn id_or_air(&self, id: &str) -> BlockId {
        self.id(id).unwrap_or(BlockId::AIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_registry_is_stable() {
        let reg = Registry::base();
        assert_eq!(reg.id("openblock:air"), Some(BlockId(0)));
        assert_eq!(reg.id("openblock:grass"), Some(BlockId(3)));
        assert!(reg.block(BlockId(3)).opaque);
        assert!(reg.block(reg.id("openblock:water").unwrap()).fluid);
        assert_eq!(reg.len(), 30);
        assert_eq!(reg.block(reg.id("openblock:bedrock").unwrap()).hardness, f32::INFINITY);
        assert!(!reg.block(reg.id("openblock:bedrock").unwrap()).breakable());
        assert_eq!(reg.block(reg.id("openblock:stone").unwrap()).hardness, 1.5);
        assert_eq!(reg.id("openblock:planks"), Some(BlockId(12)));
        assert_eq!(reg.id("openblock:cobblestone"), Some(BlockId(13)));
        assert_eq!(reg.id("openblock:crafting_table"), Some(BlockId(14)));
        assert_eq!(reg.id("openblock:coal_ore"), Some(BlockId(15)));
        assert_eq!(reg.id("openblock:iron_ore"), Some(BlockId(16)));
        assert_eq!(reg.id("openblock:copper_ore"), Some(BlockId(17)));
        assert_eq!(reg.id("openblock:furnace"), Some(BlockId(18)));
        assert_eq!(reg.id("openblock:chest"), Some(BlockId(19)));
        // 1.2 content delta: append-only ids after the 1.0 catalogue.
        assert_eq!(reg.id("openblock:torch"), Some(BlockId(20)));
        assert_eq!(reg.id("openblock:farmland_dry"), Some(BlockId(21)));
        assert_eq!(reg.id("openblock:farmland_wet"), Some(BlockId(22)));
        assert_eq!(reg.id("openblock:wheat_0"), Some(BlockId(23)));
        assert_eq!(reg.id("openblock:wheat_3"), Some(BlockId(26)));
        assert_eq!(reg.id("openblock:bed_foot"), Some(BlockId(27)));
        assert_eq!(reg.id("openblock:bed_head"), Some(BlockId(28)));
        // 1.3 content delta: fire is append-only id 29.
        assert_eq!(reg.id("openblock:fire"), Some(BlockId(29)));
    }

    #[test]
    fn fire_block_is_a_non_solid_full_light_cross_and_wood_is_flammable() {
        let reg = Registry::base();
        let fire = reg.block(reg.id("openblock:fire").unwrap());
        assert!(!fire.solid);
        assert!(!fire.opaque);
        assert_eq!(fire.emission, [15, 15, 15]);
        assert_eq!(fire.attenuation, [0; 3]);
        assert_eq!(fire.shape, BlockShape::Cross);
        assert_eq!(fire.textures.top, textures::FIRE);
        assert!(fire.breakable());
        assert!(!fire.flammable, "fire is not itself a spread target");
        for name in [
            "openblock:planks",
            "openblock:log",
            "openblock:leaves",
            "openblock:crafting_table",
            "openblock:chest",
            "openblock:bed_foot",
            "openblock:bed_head",
        ] {
            assert!(reg.block(reg.id(name).unwrap()).flammable, "{name} must be flammable");
        }
        for name in [
            "openblock:stone",
            "openblock:dirt",
            "openblock:water",
            "openblock:torch",
        ] {
            assert!(
                !reg.block(reg.id(name).unwrap()).flammable,
                "{name} must not be flammable"
            );
        }
        assert!(
            reg.item_for_block(BlockId(29)).is_some(),
            "fire has a creative block item"
        );
    }

    #[test]
    fn new_ore_and_station_blocks_have_properties() {
        let reg = Registry::base();
        let block = |name: &str| reg.block(reg.id(name).unwrap());
        let pickaxe = Some(ToolKind::Pickaxe);

        for (name, texture, tier) in [
            ("openblock:coal_ore", textures::COAL_ORE, 1),
            ("openblock:iron_ore", textures::IRON_ORE, 2),
            ("openblock:copper_ore", textures::COPPER_ORE, 2),
        ] {
            let ore = block(name);
            assert_eq!(ore.hardness, 3.0, "{name}");
            assert!(ore.solid && ore.opaque && !ore.fluid && ore.breakable(), "{name}");
            assert_eq!(ore.preferred_tool, pickaxe, "{name}");
            assert_eq!(ore.required_tier, tier, "{name}");
            assert_eq!(ore.textures, BlockTextures::all(texture), "{name}");
            assert!(ore.is_ore(), "{name}");
            assert_eq!(ore.textures.for_face(4), texture, "{name}");
        }

        let furnace = block("openblock:furnace");
        assert_eq!(furnace.hardness, 3.5);
        assert!(furnace.solid && furnace.opaque && furnace.breakable());
        assert_eq!(furnace.preferred_tool, Some(ToolKind::Axe));
        assert_eq!(furnace.required_tier, 0);
        assert!(!furnace.is_ore());
        assert_eq!(
            furnace.textures,
            BlockTextures::fronted(
                textures::FURNACE_TOP,
                textures::FURNACE_SIDE,
                textures::FURNACE_TOP,
                textures::FURNACE_FRONT
            )
        );
        assert_eq!(furnace.textures.for_face(0), textures::FURNACE_TOP);
        assert_eq!(furnace.textures.for_face(1), textures::FURNACE_TOP);
        for side in [2, 3, 5] {
            assert_eq!(furnace.textures.for_face(side), textures::FURNACE_SIDE);
        }
        assert_eq!(furnace.textures.for_face(4), textures::FURNACE_FRONT);
        assert_eq!(furnace.textures.front, Some(textures::FURNACE_FRONT));

        let chest = block("openblock:chest");
        assert_eq!(chest.hardness, 2.5);
        assert!(chest.solid && chest.opaque && chest.breakable());
        assert_eq!(chest.preferred_tool, Some(ToolKind::Axe));
        assert_eq!(chest.required_tier, 0);
        assert_eq!(
            chest.textures,
            BlockTextures::fronted(
                textures::CHEST_TOP,
                textures::CHEST_SIDE,
                textures::CHEST_TOP,
                textures::CHEST_FRONT
            )
        );
        assert_eq!(chest.textures.for_face(4), textures::CHEST_FRONT);

        // Blocks without a front keep using their side texture on +Z.
        let plain = block("openblock:stone");
        assert_eq!(plain.textures.front, None);
        assert_eq!(plain.textures.for_face(4), textures::STONE);
        assert_eq!(plain.textures.front_texture(), textures::STONE);
    }

    #[test]
    fn new_blocks_have_properties_and_tool_rules() {
        let reg = Registry::base();
        let block = |name: &str| reg.block(reg.id(name).unwrap());
        let pickaxe = Some(ToolKind::Pickaxe);

        let planks = block("openblock:planks");
        assert_eq!(planks.hardness, 2.0);
        assert!(planks.solid && planks.opaque && planks.breakable());
        assert_eq!(planks.preferred_tool, Some(ToolKind::Axe));
        assert_eq!(planks.required_tier, 0);
        assert_eq!(planks.textures, BlockTextures::all(textures::PLANKS));

        let cobblestone = block("openblock:cobblestone");
        assert_eq!(cobblestone.hardness, 2.0);
        assert!(cobblestone.solid && cobblestone.opaque && cobblestone.breakable());
        assert_eq!(cobblestone.preferred_tool, pickaxe);
        assert_eq!(cobblestone.required_tier, 1);
        assert_eq!(cobblestone.textures, BlockTextures::all(textures::COBBLESTONE));

        let table = block("openblock:crafting_table");
        assert_eq!(table.hardness, 2.5);
        assert!(table.solid && table.opaque && table.breakable());
        assert_eq!(table.preferred_tool, Some(ToolKind::Axe));
        assert_eq!(table.required_tier, 0);
        assert_eq!(
            table.textures,
            BlockTextures::column(
                textures::CRAFTING_TABLE_TOP,
                textures::CRAFTING_TABLE_SIDE,
                textures::PLANKS
            )
        );

        // Existing blocks keep their ids and gain tool preferences.
        assert_eq!(block("openblock:stone").preferred_tool, pickaxe);
        assert_eq!(block("openblock:stone").required_tier, 1);
        assert_eq!(block("openblock:log").preferred_tool, Some(ToolKind::Axe));
        assert_eq!(block("openblock:dirt").preferred_tool, Some(ToolKind::Shovel));
        assert_eq!(block("openblock:grass").preferred_tool, Some(ToolKind::Shovel));
        assert_eq!(block("openblock:leaves").preferred_tool, None);
    }

    #[test]
    fn mining_speed_and_drops_follow_tool_rules() {
        let reg = Registry::base();
        let item = |name: &str| reg.item_id(name).unwrap();
        let block = |name: &str| reg.id(name).unwrap();
        let stone = block("openblock:stone");
        let cobble = block("openblock:cobblestone");
        let wood_pick = item("openblock:wooden_pickaxe");
        let stone_pick = item("openblock:stone_pickaxe");
        let wood_axe = item("openblock:wooden_axe");
        let wood_shovel = item("openblock:wooden_shovel");
        let log = block("openblock:log");
        let dirt = block("openblock:dirt");

        // Hand and wrong kind never speed up or satisfy the tier.
        assert_eq!(reg.mining_speed(stone, None), 1.0);
        assert_eq!(reg.mining_speed(stone, Some(wood_axe)), 1.0);
        assert!(!reg.can_drop(stone, None));
        assert!(!reg.can_drop(stone, Some(wood_axe)));
        assert_eq!(reg.effective_mining_seconds(stone, None), 1.5);

        // Pickaxes of tier 1/2 harvest stone and speed it up.
        assert_eq!(reg.mining_speed(stone, Some(wood_pick)), 2.0);
        assert!(reg.can_drop(stone, Some(wood_pick)));
        assert_eq!(reg.effective_mining_seconds(stone, Some(wood_pick)), 0.75);
        assert_eq!(reg.mining_speed(cobble, Some(stone_pick)), 4.0);
        assert!(reg.can_drop(cobble, Some(stone_pick)));
        assert!(!reg.can_drop(cobble, Some(item("openblock:stone_sword"))));

        // Tier-0 blocks drop by hand; a matching tool still helps.
        assert!(reg.can_drop(log, None));
        assert_eq!(reg.mining_speed(log, Some(wood_axe)), 2.0);
        assert_eq!(reg.effective_mining_seconds(log, Some(wood_axe)), 1.0);
        assert!(reg.can_drop(dirt, None));
        assert_eq!(reg.mining_speed(dirt, Some(wood_shovel)), 2.0);

        // Unbreakables never drop and have no mining duration.
        let bedrock = block("openblock:bedrock");
        assert!(!reg.can_drop(bedrock, Some(stone_pick)));
        assert_eq!(reg.effective_mining_seconds(bedrock, Some(stone_pick)), 0.0);
        assert!(!reg.can_drop(BlockId::AIR, None));
    }

    #[test]
    fn texture_catalogue_matches_block_references() {
        let reg = Registry::base();
        for (_, block) in reg.iter() {
            for face in 0..6 {
                let layer = block.textures.for_face(face).0 as usize;
                assert!(layer < textures::COUNT, "texture layer out of range");
            }
        }
        assert_eq!(textures::NAMES.len(), textures::COUNT);
        assert_eq!(textures::COUNT, 39);
        assert_eq!(textures::NAMES[14], "planks");
        assert_eq!(textures::NAMES[15], "cobblestone");
        assert_eq!(textures::NAMES[16], "crafting_table_top");
        assert_eq!(textures::NAMES[17], "crafting_table_side");
        assert_eq!(textures::NAMES[18], "coal_ore");
        assert_eq!(textures::NAMES[19], "iron_ore");
        assert_eq!(textures::NAMES[20], "copper_ore");
        assert_eq!(textures::NAMES[21], "furnace_top");
        assert_eq!(textures::NAMES[22], "furnace_side");
        assert_eq!(textures::NAMES[23], "furnace_front");
        assert_eq!(textures::NAMES[24], "chest_top");
        assert_eq!(textures::NAMES[25], "chest_side");
        assert_eq!(textures::NAMES[26], "chest_front");
        assert_eq!(textures::NAMES[27], "torch");
        assert_eq!(textures::NAMES[28], "farmland_dry");
        assert_eq!(textures::NAMES[29], "farmland_wet");
        assert_eq!(textures::NAMES[30], "wheat_0");
        assert_eq!(textures::NAMES[31], "wheat_1");
        assert_eq!(textures::NAMES[32], "wheat_2");
        assert_eq!(textures::NAMES[33], "wheat_3");
        assert_eq!(textures::NAMES[34], "bed_foot_top");
        assert_eq!(textures::NAMES[35], "bed_foot_side");
        assert_eq!(textures::NAMES[36], "bed_head_top");
        assert_eq!(textures::NAMES[37], "bed_head_side");
        assert_eq!(textures::COAL_ORE, TextureId(18));
        assert_eq!(textures::IRON_ORE, TextureId(19));
        assert_eq!(textures::COPPER_ORE, TextureId(20));
        assert_eq!(textures::FURNACE_TOP, TextureId(21));
        assert_eq!(textures::FURNACE_SIDE, TextureId(22));
        assert_eq!(textures::FURNACE_FRONT, TextureId(23));
        assert_eq!(textures::CHEST_TOP, TextureId(24));
        assert_eq!(textures::CHEST_SIDE, TextureId(25));
        assert_eq!(textures::CHEST_FRONT, TextureId(26));
        assert_eq!(textures::TORCH, TextureId(27));
        assert_eq!(textures::FARMLAND_DRY, TextureId(28));
        assert_eq!(textures::FARMLAND_WET, TextureId(29));
        assert_eq!(textures::WHEAT_0, TextureId(30));
        assert_eq!(textures::WHEAT_3, TextureId(33));
        assert_eq!(textures::BED_FOOT_TOP, TextureId(34));
        assert_eq!(textures::BED_HEAD_SIDE, TextureId(37));
        assert_eq!(textures::FIRE, TextureId(38));
        assert_eq!(textures::NAMES[38], "fire");
        let mut names: Vec<&str> = textures::NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), textures::NAMES.len(), "texture names must be unique");
    }

    #[test]
    fn grass_has_distinct_face_textures() {
        let reg = Registry::base();
        let grass = reg.block(reg.id("openblock:grass").unwrap());
        assert_eq!(grass.textures.top, textures::GRASS_TOP);
        assert_eq!(grass.textures.side, textures::GRASS_SIDE);
        assert_eq!(grass.textures.bottom, textures::DIRT);
    }

    #[test]
    fn item_ids_are_stable_and_unique() {
        let reg = Registry::base();
        // v0.0.7/v0.0.8 item ids are frozen: eleven block items, the apple,
        // three block items, then the stick and eight tools. v0.0.9 appends 23
        // non-block items at 24..=46 and five block items at 47..=51.
        assert_eq!(reg.item_len(), 72);
        assert_eq!(reg.item_id("openblock:stone"), Some(ItemId(0)));
        assert_eq!(reg.item_id("openblock:dirt"), Some(ItemId(1)));
        assert_eq!(reg.item_id("openblock:grass"), Some(ItemId(2)));
        assert_eq!(reg.item_id("openblock:snow"), Some(ItemId(10)));
        assert_eq!(reg.item_id("openblock:apple"), Some(ItemId(11)));
        assert_eq!(reg.item_id("openblock:planks"), Some(ItemId(12)));
        assert_eq!(reg.item_id("openblock:cobblestone"), Some(ItemId(13)));
        assert_eq!(reg.item_id("openblock:crafting_table"), Some(ItemId(14)));
        assert_eq!(reg.item_id("openblock:stick"), Some(ItemId(15)));
        assert_eq!(reg.item_id("openblock:wooden_pickaxe"), Some(ItemId(16)));
        assert_eq!(reg.item_id("openblock:stone_sword"), Some(ItemId(23)));
        assert_eq!(reg.item_id("openblock:coal"), Some(ItemId(24)));
        assert_eq!(reg.item_id("openblock:charcoal"), Some(ItemId(25)));
        assert_eq!(reg.item_id("openblock:raw_iron"), Some(ItemId(26)));
        assert_eq!(reg.item_id("openblock:iron_ingot"), Some(ItemId(27)));
        assert_eq!(reg.item_id("openblock:raw_copper"), Some(ItemId(28)));
        assert_eq!(reg.item_id("openblock:copper_ingot"), Some(ItemId(29)));
        assert_eq!(reg.item_id("openblock:iron_pickaxe"), Some(ItemId(30)));
        assert_eq!(reg.item_id("openblock:iron_sword"), Some(ItemId(33)));
        assert_eq!(reg.item_id("openblock:leather_helmet"), Some(ItemId(34)));
        assert_eq!(reg.item_id("openblock:leather_boots"), Some(ItemId(37)));
        assert_eq!(reg.item_id("openblock:iron_helmet"), Some(ItemId(38)));
        assert_eq!(reg.item_id("openblock:iron_boots"), Some(ItemId(41)));
        assert_eq!(reg.item_id("openblock:raw_beef"), Some(ItemId(42)));
        assert_eq!(reg.item_id("openblock:cooked_porkchop"), Some(ItemId(45)));
        assert_eq!(reg.item_id("openblock:leather"), Some(ItemId(46)));
        assert_eq!(reg.item_id("openblock:coal_ore"), Some(ItemId(47)));
        assert_eq!(reg.item_id("openblock:iron_ore"), Some(ItemId(48)));
        assert_eq!(reg.item_id("openblock:copper_ore"), Some(ItemId(49)));
        assert_eq!(reg.item_id("openblock:furnace"), Some(ItemId(50)));
        assert_eq!(reg.item_id("openblock:chest"), Some(ItemId(51)));
        // 1.2: nine block items (52..=60) then six non-block items (61..=66).
        assert_eq!(reg.item_id("openblock:torch"), Some(ItemId(52)));
        assert_eq!(reg.item_id("openblock:farmland_dry"), Some(ItemId(53)));
        assert_eq!(reg.item_id("openblock:farmland_wet"), Some(ItemId(54)));
        assert_eq!(reg.item_id("openblock:wheat_0"), Some(ItemId(55)));
        assert_eq!(reg.item_id("openblock:wheat_3"), Some(ItemId(58)));
        assert_eq!(reg.item_id("openblock:bed_foot"), Some(ItemId(59)));
        assert_eq!(reg.item_id("openblock:bed_head"), Some(ItemId(60)));
        assert_eq!(reg.item_id("openblock:seeds"), Some(ItemId(61)));
        assert_eq!(reg.item_id("openblock:wheat"), Some(ItemId(62)));
        assert_eq!(reg.item_id("openblock:bread"), Some(ItemId(63)));
        assert_eq!(reg.item_id("openblock:wood_hoe"), Some(ItemId(64)));
        assert_eq!(reg.item_id("openblock:stone_hoe"), Some(ItemId(65)));
        assert_eq!(reg.item_id("openblock:iron_hoe"), Some(ItemId(66)));
        // 1.3: four non-block items append after the 1.2 catalogue.
        assert_eq!(reg.item_id("openblock:bone"), Some(ItemId(67)));
        assert_eq!(reg.item_id("openblock:string"), Some(ItemId(68)));
        assert_eq!(reg.item_id("openblock:gunpowder"), Some(ItemId(69)));
        assert_eq!(reg.item_id("openblock:arrow"), Some(ItemId(70)));
        // 1.5 fire block item appends after every frozen 1.2/1.3 item.
        assert_eq!(reg.item_id("openblock:fire"), Some(ItemId(71)));

        // The v0.0.9 non-block item ids 24..=46 are frozen in this exact order.
        let appended = [
            "openblock:coal",
            "openblock:charcoal",
            "openblock:raw_iron",
            "openblock:iron_ingot",
            "openblock:raw_copper",
            "openblock:copper_ingot",
            "openblock:iron_pickaxe",
            "openblock:iron_axe",
            "openblock:iron_shovel",
            "openblock:iron_sword",
            "openblock:leather_helmet",
            "openblock:leather_chestplate",
            "openblock:leather_leggings",
            "openblock:leather_boots",
            "openblock:iron_helmet",
            "openblock:iron_chestplate",
            "openblock:iron_leggings",
            "openblock:iron_boots",
            "openblock:raw_beef",
            "openblock:cooked_beef",
            "openblock:raw_porkchop",
            "openblock:cooked_porkchop",
            "openblock:leather",
        ];
        for (offset, name) in appended.iter().enumerate() {
            assert_eq!(reg.item_id(name), Some(ItemId(24 + offset as u16)), "{name}");
        }

        let mut seen = std::collections::BTreeSet::new();
        for (id, item) in reg.items() {
            assert!(seen.insert(item.id), "duplicate item id {}", item.id);
            assert_eq!(reg.item(id).id, item.id);
            assert_eq!(reg.item_id(item.id), Some(id));
        }
        assert_eq!(seen.len(), reg.item_len());
    }

    #[test]
    fn apple_is_edible_and_not_a_block() {
        let reg = Registry::base();
        let apple_id = reg.item_id("openblock:apple").expect("apple item exists");
        let apple = reg.item(apple_id);
        assert!(apple.edible());
        assert_eq!(apple.food, Some(6));
        assert_eq!(apple.block, None);
        assert_eq!(apple.block(), None);
        assert_eq!(apple.max_stack, 16);
        assert!(!apple.is_tool());
        assert!(!apple.is_armor());
        assert!(!apple.id.contains("air"));
    }

    #[test]
    fn new_items_have_stable_stats() {
        let reg = Registry::base();
        let item = |name: &str| reg.item(name_id(&reg, name));
        let stick = item("openblock:stick");
        assert_eq!(stick.max_stack, 64);
        assert_eq!(stick.tool, None);
        assert!(!stick.is_tool());

        for name in ["openblock:planks", "openblock:cobblestone", "openblock:crafting_table"] {
            let block_item = item(name);
            assert_eq!(block_item.max_stack, 64);
            assert!(block_item.block.is_some());
            assert!(!block_item.is_tool());
        }

        let wood_pick = item("openblock:wooden_pickaxe");
        let stone_pick = item("openblock:stone_pickaxe");
        assert_eq!(wood_pick.max_stack, 1);
        assert!(wood_pick.is_tool());
        assert_eq!(
            wood_pick.tool,
            Some(ToolSpec {
                kind: ToolKind::Pickaxe,
                tier: 1,
                speed: 2.0,
                attack_damage: 2.0,
                durability: 59
            })
        );
        assert_eq!(
            stone_pick.tool,
            Some(ToolSpec {
                kind: ToolKind::Pickaxe,
                tier: 2,
                speed: 4.0,
                attack_damage: 3.0,
                durability: 131
            })
        );
        for name in [
            "openblock:wooden_axe",
            "openblock:wooden_shovel",
            "openblock:wooden_sword",
            "openblock:stone_axe",
            "openblock:stone_shovel",
            "openblock:stone_sword",
        ] {
            let tool = item(name);
            assert_eq!(tool.max_stack, 1);
            assert!(tool.is_tool());
            assert_eq!(
                tool.tool.unwrap().durability,
                if name.contains("wooden") { 59 } else { 131 }
            );
        }
        assert_eq!(item("openblock:wooden_sword").tool.unwrap().speed, 1.0);
        assert_eq!(item("openblock:stone_sword").tool.unwrap().speed, 1.0);
        assert_eq!(item("openblock:wooden_sword").tool.unwrap().kind, ToolKind::Sword);
        assert_eq!(ToolKind::Pickaxe.name(), "pickaxe");
        assert_eq!(ToolKind::Axe.name(), "axe");
        assert_eq!(ToolKind::Shovel.name(), "shovel");
        assert_eq!(ToolKind::Sword.name(), "sword");
        assert_eq!(ToolKind::Hoe.name(), "hoe");
    }

    #[test]
    fn iron_tools_have_stable_stats() {
        let reg = Registry::base();
        let item = |name: &str| reg.item(name_id(&reg, name));
        for (name, kind, speed) in [
            ("openblock:iron_pickaxe", ToolKind::Pickaxe, 6.0),
            ("openblock:iron_axe", ToolKind::Axe, 6.0),
            ("openblock:iron_shovel", ToolKind::Shovel, 6.0),
            ("openblock:iron_sword", ToolKind::Sword, 1.0),
        ] {
            let tool = item(name);
            assert_eq!(tool.max_stack, 1, "{name}");
            assert!(tool.is_tool(), "{name}");
            assert!(!tool.is_armor(), "{name}");
            assert_eq!(tool.block, None, "{name}");
            assert_eq!(
                tool.tool,
                Some(ToolSpec {
                    kind,
                    tier: 3,
                    speed,
                    attack_damage: if kind == ToolKind::Sword { 6.0 } else { 4.0 },
                    durability: 251
                }),
                "{name}"
            );
        }
    }

    #[test]
    fn armor_items_have_stable_stats() {
        let reg = Registry::base();
        let item = |name: &str| reg.item(name_id(&reg, name));
        let expected = [
            ("openblock:leather_helmet", ArmorSlot::Helmet, 1, 55),
            ("openblock:leather_chestplate", ArmorSlot::Chestplate, 3, 80),
            ("openblock:leather_leggings", ArmorSlot::Leggings, 2, 75),
            ("openblock:leather_boots", ArmorSlot::Boots, 1, 65),
            ("openblock:iron_helmet", ArmorSlot::Helmet, 2, 165),
            ("openblock:iron_chestplate", ArmorSlot::Chestplate, 6, 240),
            ("openblock:iron_leggings", ArmorSlot::Leggings, 5, 225),
            ("openblock:iron_boots", ArmorSlot::Boots, 2, 195),
        ];
        for (name, slot, defense, durability) in expected {
            let armor = item(name);
            assert_eq!(armor.max_stack, 1, "{name}");
            assert!(armor.is_armor(), "{name}");
            assert!(!armor.is_tool(), "{name}");
            assert!(!armor.edible(), "{name}");
            assert_eq!(armor.block, None, "{name}");
            assert_eq!(
                armor.armor,
                Some(ArmorSpec {
                    slot,
                    defense,
                    durability
                }),
                "{name}"
            );
        }
        assert_eq!(ArmorSlot::Helmet.name(), "helmet");
        assert_eq!(ArmorSlot::Chestplate.name(), "chestplate");
        assert_eq!(ArmorSlot::Leggings.name(), "leggings");
        assert_eq!(ArmorSlot::Boots.name(), "boots");
    }

    #[test]
    fn animal_food_items_are_edible() {
        let reg = Registry::base();
        for (name, food) in [
            ("openblock:raw_beef", 3),
            ("openblock:cooked_beef", 8),
            ("openblock:raw_porkchop", 3),
            ("openblock:cooked_porkchop", 8),
        ] {
            let item = reg.item(name_id(&reg, name));
            assert_eq!(item.food, Some(food), "{name}");
            assert!(item.edible(), "{name}");
            assert_eq!(item.max_stack, 64, "{name}");
            assert_eq!(item.block, None, "{name}");
            assert!(!item.is_tool() && !item.is_armor(), "{name}");
        }
        let leather = reg.item(name_id(&reg, "openblock:leather"));
        assert_eq!(leather.food, None);
        assert_eq!(leather.max_stack, 64);
        assert!(!leather.edible() && !leather.is_tool() && !leather.is_armor());
    }

    #[test]
    fn smelting_and_fuel_tables_are_stable() {
        let reg = Registry::base();
        let item = |name: &str| name_id(&reg, name);
        assert_eq!(reg.smelt_recipes().count(), 6);
        assert_eq!(SMELT_TICKS_PER_ITEM, 200);

        let expected = [
            ("openblock:raw_iron", "openblock:iron_ingot", 1),
            ("openblock:raw_copper", "openblock:copper_ingot", 1),
            ("openblock:log", "openblock:charcoal", 1),
            ("openblock:raw_beef", "openblock:cooked_beef", 1),
            ("openblock:raw_porkchop", "openblock:cooked_porkchop", 1),
            ("openblock:cobblestone", "openblock:stone", 1),
        ];
        for (input, output, count) in expected {
            assert_eq!(
                reg.smelt_recipe_for(item(input)),
                Some((item(output), count)),
                "{input}"
            );
            assert_eq!(reg.smelt_result_for(item(input)), Some(item(output)));
            assert!(reg.is_smeltable(item(input)));
        }

        // Sand/glass is explicitly out of scope for 1.0.
        assert_eq!(reg.smelt_recipe_for(item("openblock:sand")), None);
        assert!(!reg.is_smeltable(item("openblock:sand")));
        assert_eq!(reg.smelt_result_for(ItemId(999)), None);

        assert_eq!(reg.fuel_ticks(item("openblock:coal")), Some(1_600));
        assert_eq!(reg.fuel_ticks(item("openblock:charcoal")), Some(1_600));
        assert_eq!(reg.fuel_ticks(item("openblock:planks")), Some(300));
        assert_eq!(reg.fuel_ticks(item("openblock:stick")), Some(100));
        assert_eq!(reg.fuel_ticks(item("openblock:log")), Some(300));
        assert_eq!(reg.fuel_ticks(item("openblock:apple")), None);
        assert_eq!(reg.fuel_ticks(ItemId(999)), None);
        assert!(reg.is_fuel(item("openblock:coal")));
        assert!(!reg.is_fuel(item("openblock:apple")));

        // One coal is exactly eight smelts.
        assert_eq!(
            reg.fuel_ticks(item("openblock:coal")).unwrap() / SMELT_TICKS_PER_ITEM,
            8
        );
    }

    #[test]
    fn ore_tiers_control_drops_and_mining_speed() {
        let reg = Registry::base();
        let item = |name: &str| name_id(&reg, name);
        let block = |name: &str| reg.id(name).unwrap();
        let wood_pick = item("openblock:wooden_pickaxe");
        let stone_pick = item("openblock:stone_pickaxe");
        let iron_pick = item("openblock:iron_pickaxe");

        // Coal ore is harvestable with any pickaxe; hand mining drops nothing.
        let coal_ore = block("openblock:coal_ore");
        assert!(!reg.can_drop(coal_ore, None));
        assert!(reg.can_drop(coal_ore, Some(wood_pick)));
        assert_eq!(reg.mining_speed(coal_ore, Some(wood_pick)), 2.0);
        assert_eq!(reg.mining_speed(coal_ore, Some(stone_pick)), 4.0);
        assert_eq!(reg.mining_speed(coal_ore, Some(iron_pick)), 6.0);
        assert_eq!(reg.effective_mining_seconds(coal_ore, Some(iron_pick)), 0.5);
        assert_eq!(reg.block_drop(coal_ore), Some((item("openblock:coal"), 1)));

        // Iron/copper ore require a tier-2 pickaxe.
        for (ore, drop) in [
            ("openblock:iron_ore", "openblock:raw_iron"),
            ("openblock:copper_ore", "openblock:raw_copper"),
        ] {
            let block_id = block(ore);
            assert!(!reg.can_drop(block_id, None), "{ore}");
            assert!(!reg.can_drop(block_id, Some(wood_pick)), "{ore}");
            assert!(reg.can_drop(block_id, Some(stone_pick)), "{ore}");
            assert!(reg.can_drop(block_id, Some(iron_pick)), "{ore}");
            assert!(!reg.can_drop(block_id, Some(item("openblock:stone_sword"))), "{ore}");
            assert_eq!(reg.block_drop(block_id), Some((item(drop), 1)), "{ore}");
        }

        // Furnace and chest drop themselves.
        for name in ["openblock:furnace", "openblock:chest"] {
            let block_id = block(name);
            assert!(reg.can_drop(block_id, None), "{name}");
            assert_eq!(
                reg.block_drop(block_id),
                Some((reg.item_for_block(block_id).unwrap(), 1)),
                "{name}"
            );
            assert!(!reg.block(block_id).is_ore(), "{name}");
        }
    }

    fn name_id(reg: &Registry, name: &str) -> ItemId {
        reg.item_id(name).unwrap_or_else(|| panic!("missing item {name}"))
    }

    #[test]
    fn recipe_table_is_stable_and_complete() {
        let reg = Registry::base();
        assert_eq!(reg.recipe_len(), 31);
        let expected = [
            "openblock:planks",
            "openblock:sticks",
            "openblock:crafting_table",
            "openblock:wooden_pickaxe",
            "openblock:wooden_axe",
            "openblock:wooden_shovel",
            "openblock:wooden_sword",
            "openblock:stone_pickaxe",
            "openblock:stone_axe",
            "openblock:stone_shovel",
            "openblock:stone_sword",
            "openblock:furnace",
            "openblock:chest",
            "openblock:iron_pickaxe",
            "openblock:iron_axe",
            "openblock:iron_shovel",
            "openblock:iron_sword",
            "openblock:leather_helmet",
            "openblock:leather_chestplate",
            "openblock:leather_leggings",
            "openblock:leather_boots",
            "openblock:iron_helmet",
            "openblock:iron_chestplate",
            "openblock:iron_leggings",
            "openblock:iron_boots",
            "openblock:torch",
            "openblock:bread",
            "openblock:wood_hoe",
            "openblock:stone_hoe",
            "openblock:iron_hoe",
            "openblock:arrow",
        ];
        for (index, id) in expected.iter().enumerate() {
            let recipe_id = reg.recipe_id(id).expect("base recipe exists");
            assert_eq!(recipe_id, RecipeId(index as u16));
            let recipe = reg.recipe(recipe_id).expect("recipe id resolves");
            assert_eq!(recipe.id, *id);
            assert!(!recipe.display_name.is_empty());
            assert!(recipe.output.1 > 0);
            assert!(!recipe.ingredients.is_empty());
        }
        assert_eq!(reg.recipe(RecipeId(99)), None);
        assert_eq!(reg.recipe_id("openblock:nope"), None);

        let recipe = |name: &str| *reg.recipe(reg.recipe_id(name).unwrap()).unwrap();
        assert_eq!(
            recipe("openblock:planks").output,
            (name_id(&reg, "openblock:planks"), 4)
        );
        assert_eq!(
            recipe("openblock:planks").ingredients,
            &[(name_id(&reg, "openblock:log"), 1)]
        );
        assert!(!recipe("openblock:planks").needs_table);
        assert_eq!(recipe("openblock:sticks").output.1, 4);
        assert_eq!(
            recipe("openblock:sticks").ingredients,
            &[(name_id(&reg, "openblock:planks"), 2)]
        );
        assert!(!recipe("openblock:sticks").needs_table);
        assert_eq!(
            recipe("openblock:crafting_table").ingredients,
            &[(name_id(&reg, "openblock:planks"), 4)]
        );
        assert!(!recipe("openblock:crafting_table").needs_table);

        let wood_pick = recipe("openblock:wooden_pickaxe");
        assert_eq!(wood_pick.output, (name_id(&reg, "openblock:wooden_pickaxe"), 1));
        assert_eq!(
            wood_pick.ingredients,
            &[
                (name_id(&reg, "openblock:planks"), 3),
                (name_id(&reg, "openblock:stick"), 2)
            ]
        );
        assert!(wood_pick.needs_table);
        let arrow = recipe("openblock:arrow");
        assert_eq!(arrow.output, (name_id(&reg, "openblock:arrow"), 4));
        assert_eq!(
            arrow.ingredients,
            &[
                (name_id(&reg, "openblock:stick"), 1),
                (name_id(&reg, "openblock:string"), 1)
            ]
        );
        assert!(!arrow.needs_table);
        let stone_sword = recipe("openblock:stone_sword");
        assert_eq!(
            stone_sword.ingredients,
            &[
                (name_id(&reg, "openblock:cobblestone"), 2),
                (name_id(&reg, "openblock:stick"), 1)
            ]
        );

        assert_eq!(
            recipe("openblock:furnace").ingredients,
            &[(name_id(&reg, "openblock:cobblestone"), 8)]
        );
        assert!(recipe("openblock:furnace").needs_table);
        assert_eq!(
            recipe("openblock:chest").ingredients,
            &[(name_id(&reg, "openblock:planks"), 8)]
        );
        assert!(recipe("openblock:chest").needs_table);

        assert!(recipe("openblock:iron_pickaxe").needs_table);
        assert_eq!(
            recipe("openblock:iron_pickaxe").ingredients,
            &[
                (name_id(&reg, "openblock:iron_ingot"), 3),
                (name_id(&reg, "openblock:stick"), 2)
            ]
        );
        assert!(recipe("openblock:iron_axe").needs_table);
        assert_eq!(
            recipe("openblock:iron_axe").ingredients,
            &[
                (name_id(&reg, "openblock:iron_ingot"), 3),
                (name_id(&reg, "openblock:stick"), 2)
            ]
        );
        assert!(recipe("openblock:iron_shovel").needs_table);
        assert_eq!(
            recipe("openblock:iron_shovel").ingredients,
            &[
                (name_id(&reg, "openblock:iron_ingot"), 1),
                (name_id(&reg, "openblock:stick"), 2)
            ]
        );
        assert!(recipe("openblock:iron_sword").needs_table);
        assert_eq!(
            recipe("openblock:iron_sword").ingredients,
            &[
                (name_id(&reg, "openblock:iron_ingot"), 2),
                (name_id(&reg, "openblock:stick"), 1)
            ]
        );
        for (leather, count) in [
            ("openblock:leather_helmet", 5),
            ("openblock:leather_chestplate", 8),
            ("openblock:leather_leggings", 7),
            ("openblock:leather_boots", 4),
        ] {
            assert_eq!(
                recipe(leather).ingredients,
                &[(name_id(&reg, "openblock:leather"), count)],
                "{leather}"
            );
            assert!(recipe(leather).needs_table, "{leather}");
        }
        for (iron, count) in [
            ("openblock:iron_helmet", 5),
            ("openblock:iron_chestplate", 8),
            ("openblock:iron_leggings", 7),
            ("openblock:iron_boots", 4),
        ] {
            assert_eq!(
                recipe(iron).ingredients,
                &[(name_id(&reg, "openblock:iron_ingot"), count)],
                "{iron}"
            );
            assert!(recipe(iron).needs_table, "{iron}");
        }

        for (_, recipe) in reg.recipes() {
            let total: u32 = recipe.ingredients.iter().map(|(_, count)| u32::from(*count)).sum();
            assert!(total > 0);
            // Everything that cannot fit the player's 2x2 grid needs a table.
            if total > 4 || recipe.id.starts_with("openblock:stone_") || recipe.id.starts_with("openblock:iron_") {
                assert!(recipe.needs_table, "{} must need a table", recipe.id);
            }
        }
    }

    #[test]
    fn every_non_air_block_has_an_item() {
        let reg = Registry::base();
        for (block_id, block) in reg.iter() {
            let item = reg.item_for_block(block_id);
            if block_id == BlockId::AIR {
                assert_eq!(item, None);
            } else {
                let item = item.expect("non-air block has a block item");
                assert_eq!(reg.item(item).id, block.id);
                assert_eq!(reg.item(item).block, Some(block_id));
                assert!(!reg.item(item).edible());
            }
        }
    }

    #[test]
    fn interim_drop_rules() {
        let reg = Registry::base();
        let item = |name: &str| reg.item_id(name).unwrap();
        let block = |name: &str| reg.id(name).unwrap();

        assert_eq!(reg.block_drop(BlockId::AIR), None);
        assert_eq!(
            reg.block_drop(block("openblock:stone")),
            Some((item("openblock:cobblestone"), 1))
        );
        assert_eq!(
            reg.block_drop(block("openblock:grass")),
            Some((item("openblock:dirt"), 1))
        );
        assert_eq!(reg.block_drop(block("openblock:leaves")), None);
        assert_eq!(reg.block_drop(block("openblock:water")), None);
        assert_eq!(reg.block_drop(block("openblock:bedrock")), None);
        assert_eq!(reg.block_drop(block("openblock:log")), Some((item("openblock:log"), 1)));
        assert_eq!(
            reg.block_drop(block("openblock:coal_ore")),
            Some((item("openblock:coal"), 1))
        );
        assert_eq!(
            reg.block_drop(block("openblock:iron_ore")),
            Some((item("openblock:raw_iron"), 1))
        );
        assert_eq!(
            reg.block_drop(block("openblock:copper_ore")),
            Some((item("openblock:raw_copper"), 1))
        );
        for name in [
            "openblock:planks",
            "openblock:cobblestone",
            "openblock:crafting_table",
            "openblock:furnace",
            "openblock:chest",
        ] {
            assert_eq!(
                reg.block_drop(block(name)),
                Some((item(name), 1)),
                "{name} must drop itself"
            );
        }
    }

    #[test]
    fn mob_ids_and_kinds_are_stable_and_unique() {
        let reg = Registry::base();
        assert_eq!(reg.mob_len(), 8);
        assert_eq!(MobKind::ALL.len(), reg.mobs().count());
        let mut names = std::collections::HashSet::new();
        for (index, kind) in MobKind::ALL.into_iter().enumerate() {
            assert_eq!(kind.index(), index);
            assert!(names.insert(kind.name()), "duplicate mob id {}", kind.name());
            assert_eq!(MobKind::from_name(kind.name()), Some(kind));
            assert_eq!(reg.mob_id(kind.name()), Some(kind));
            assert_eq!(reg.mob(kind).kind, kind);
            assert_eq!(reg.mob_by_name(kind.name()).map(|spec| spec.kind), Some(kind));
        }
        assert_eq!(MobKind::from_name("cow"), None, "saves are namespaced");
        assert_eq!(MobKind::from_name("openblock:enderman"), None);
        assert_eq!(reg.mob_id("openblock:enderman"), None);
        assert_eq!(reg.mob_id("openblock:cow"), Some(MobKind::Cow));

        // Exhaustive table lookup by index and by iteration agrees.
        for (spec, kind) in reg.mobs().zip(MobKind::ALL) {
            assert_eq!(spec.kind, kind);
            assert_eq!(reg.mob(kind), spec);
        }
    }

    #[test]
    fn mob_catalogue_has_the_interim_stats() {
        let reg = Registry::base();
        let expected = [
            (MobKind::Cow, "Cow", 10.0, 1.4, false, 0.9, 1.3),
            (MobKind::Pig, "Pig", 10.0, 1.4, false, 0.9, 0.9),
            (MobKind::Sheep, "Sheep", 8.0, 1.4, false, 0.9, 1.3),
            (MobKind::Chicken, "Chicken", 4.0, 1.5, false, 0.4, 0.7),
            (MobKind::Zombie, "Zombie", 20.0, 2.2, true, 0.6, 1.8),
            (MobKind::Spider, "Spider", 16.0, 4.0, true, 1.0, 0.9),
            (MobKind::Skeleton, "Skeleton", 20.0, 3.5, true, 0.6, 1.8),
            (MobKind::Creeper, "Creeper", 20.0, 3.0, true, 0.6, 1.8),
        ];
        for (kind, display_name, health, speed, hostile, width, height) in expected {
            let spec = reg.mob(kind);
            assert_eq!(spec.display_name, display_name);
            assert_eq!(spec.health, health);
            assert_eq!(spec.speed, speed);
            assert_eq!(spec.hostile, hostile, "{kind:?}");
            assert_eq!(spec.passive(), !hostile, "{kind:?}");
            assert_eq!(spec.width, width);
            assert_eq!(spec.height, height);
            assert!(spec.health > 0.0 && spec.speed > 0.0);
            assert!(spec.width > 0.0 && spec.height > 0.0);
        }
        assert_eq!(
            reg.mobs().filter(|spec| spec.hostile).count(),
            4,
            "zombie, spider, skeleton and creeper are hostile in 1.3"
        );
        assert!(matches!(
            reg.mob(MobKind::Zombie).attack,
            crate::MobAttack::Melee { .. }
        ));
        assert!(matches!(
            reg.mob(MobKind::Skeleton).attack,
            crate::MobAttack::Ranged { .. }
        ));
        assert!(matches!(
            reg.mob(MobKind::Creeper).attack,
            crate::MobAttack::Explosive { .. }
        ));
        assert!(reg.mob(MobKind::Spider).neutral_in_light);
        assert!(reg.mob(MobKind::Spider).climbs);
        assert!(reg.mob(MobKind::Skeleton).burns_in_daylight);
    }

    #[test]
    fn mob_drops_are_fixed_and_every_item_resolves() {
        let reg = Registry::base();
        let item = |name: &str| reg.item_id(name).unwrap();
        assert_eq!(
            reg.mob(MobKind::Cow).drops,
            &[(item("openblock:raw_beef"), 1), (item("openblock:leather"), 1)]
        );
        assert_eq!(reg.mob(MobKind::Pig).drops, &[(item("openblock:raw_porkchop"), 1)]);
        assert_eq!(reg.mob(MobKind::Sheep).drops, &[(item("openblock:leather"), 1)]);
        assert!(reg.mob(MobKind::Chicken).drops.is_empty());
        assert!(reg.mob(MobKind::Zombie).drops.is_empty());
        assert_eq!(reg.mob(MobKind::Spider).drops, &[(item("openblock:string"), 1)]);
        assert_eq!(
            reg.mob(MobKind::Skeleton).drops,
            &[(item("openblock:bone"), 1), (item("openblock:arrow"), 1)]
        );
        assert_eq!(reg.mob(MobKind::Creeper).drops, &[(item("openblock:gunpowder"), 1)]);
        for spec in reg.mobs() {
            for &(drop, count) in spec.drops {
                assert!(count > 0, "{:?} has a zero-count drop", spec.kind);
                assert!((drop.0 as usize) < reg.item_len(), "{:?} drop is invalid", spec.kind);
                assert!(!reg.item(drop).id.is_empty());
            }
        }
    }

    #[test]
    fn structure_loot_is_deterministic_and_resolves() {
        let reg = Registry::base();
        let first = structure_loot_stacks(&reg, 42, 0x1234_5678_9ABC_DEF0, (10, 64, -3), 5);
        let second = structure_loot_stacks(&reg, 42, 0x1234_5678_9ABC_DEF0, (10, 64, -3), 5);
        assert_eq!(first, second);
        assert!(!first.is_empty());
        assert!(first.len() <= 5);
        assert!(
            first
                .iter()
                .all(|(item, count)| *count > 0 && (item.0 as usize) < reg.item_len())
        );
        let other = structure_loot_stacks(&reg, 42, 0x1234_5678_9ABC_DEF0, (11, 64, -3), 5);
        assert_ne!(first, other, "chest position must salt the roll");
        let seed_changed = structure_loot_stacks(&reg, 43, 0x1234_5678_9ABC_DEF0, (10, 64, -3), 5);
        assert_ne!(first, seed_changed, "world seed must salt the roll");
        for (name, count) in roll_structure_loot(42, 0x1234_5678_9ABC_DEF0, (10, 64, -3), 5) {
            assert!(reg.item_id(name).is_some(), "{name}");
            assert!(count > 0);
        }
    }

    #[test]
    fn attack_damage_table_is_stable() {
        let reg = Registry::base();
        let item = |name: &str| reg.item_id(name);
        assert_eq!(reg.attack_damage(None), 1.0);
        assert_eq!(reg.attack_damage(Some(ItemId(999))), 1.0, "invalid ids fall back");
        assert_eq!(reg.attack_damage(item("openblock:apple")), 1.0);
        assert_eq!(reg.attack_damage(item("openblock:stick")), 1.0);
        assert_eq!(reg.attack_damage(item("openblock:stone")), 1.0);

        for (name, damage) in [
            ("openblock:wooden_pickaxe", 2.0),
            ("openblock:wooden_axe", 2.0),
            ("openblock:wooden_shovel", 2.0),
            ("openblock:wooden_sword", 4.0),
            ("openblock:stone_pickaxe", 3.0),
            ("openblock:stone_axe", 3.0),
            ("openblock:stone_shovel", 3.0),
            ("openblock:stone_sword", 5.0),
            ("openblock:iron_pickaxe", 4.0),
            ("openblock:iron_axe", 4.0),
            ("openblock:iron_shovel", 4.0),
            ("openblock:iron_sword", 6.0),
        ] {
            let id = item(name);
            assert_eq!(reg.attack_damage(id), damage, "{name}");
            assert_eq!(reg.item(id.unwrap()).tool.unwrap().attack_damage, damage, "{name}");
        }

        // Every sword beats every other tool of the same tier.
        for (sword, pickaxe) in [
            ("openblock:wooden_sword", "openblock:wooden_pickaxe"),
            ("openblock:stone_sword", "openblock:stone_pickaxe"),
            ("openblock:iron_sword", "openblock:iron_pickaxe"),
        ] {
            assert!(
                reg.attack_damage(item(sword)) > reg.attack_damage(item(pickaxe)),
                "{sword} must out-damage {pickaxe}"
            );
        }
    }

    #[test]
    fn light_content_blocks_declare_light_and_shape() {
        let reg = Registry::base();
        let block = |name: &str| reg.block(reg.id(name).unwrap());

        // Existing blocks: opaque stops both light kinds, water/leaves attenuate
        // by one, air passes sky through with no loss.
        assert_eq!(block("openblock:air").sky_attenuation, 0);
        assert_eq!(block("openblock:air").attenuation, [0; 3]);
        assert_eq!(block("openblock:stone").sky_attenuation, 15);
        assert_eq!(block("openblock:stone").attenuation, [15; 3]);
        assert_eq!(block("openblock:water").sky_attenuation, 1);
        assert_eq!(block("openblock:water").attenuation, [1; 3]);
        assert_eq!(block("openblock:leaves").sky_attenuation, 1);
        assert_eq!(block("openblock:leaves").attenuation, [1; 3]);
        for (_, existing) in reg.iter() {
            if existing.opaque {
                assert_eq!(existing.sky_attenuation, 15, "{}", existing.id);
            }
        }

        let torch = block("openblock:torch");
        assert_eq!(torch.shape, BlockShape::Cross);
        assert!(!torch.solid && !torch.opaque && !torch.fluid);
        assert_eq!(torch.emission, [14, 14, 14]);
        assert_eq!(torch.attenuation, [0; 3]);
        assert_eq!(torch.sky_attenuation, 0);
        assert_eq!(torch.textures, BlockTextures::all(textures::TORCH));

        for (name, texture) in [
            ("openblock:farmland_dry", textures::FARMLAND_DRY),
            ("openblock:farmland_wet", textures::FARMLAND_WET),
        ] {
            let farmland = block(name);
            assert!(farmland.solid && farmland.opaque && !farmland.fluid, "{name}");
            assert_eq!(farmland.shape, BlockShape::Cube, "{name}");
            assert_eq!(farmland.textures, BlockTextures::all(texture), "{name}");
            assert_eq!(farmland.emission, [0; 3], "{name}");
            assert_eq!(farmland.sky_attenuation, 15, "{name}");
        }

        for (name, texture) in [
            ("openblock:wheat_0", textures::WHEAT_0),
            ("openblock:wheat_1", textures::WHEAT_1),
            ("openblock:wheat_2", textures::WHEAT_2),
            ("openblock:wheat_3", textures::WHEAT_3),
        ] {
            let wheat = block(name);
            assert!(!wheat.solid && !wheat.opaque && !wheat.fluid, "{name}");
            assert_eq!(wheat.shape, BlockShape::Cross, "{name}");
            assert_eq!(wheat.textures, BlockTextures::all(texture), "{name}");
            assert_eq!(wheat.sky_attenuation, 0, "{name}");
        }

        for (name, top, side) in [
            ("openblock:bed_foot", textures::BED_FOOT_TOP, textures::BED_FOOT_SIDE),
            ("openblock:bed_head", textures::BED_HEAD_TOP, textures::BED_HEAD_SIDE),
        ] {
            let bed = block(name);
            assert!(bed.solid && !bed.opaque && !bed.fluid, "{name}");
            assert_eq!(bed.shape, BlockShape::Cube, "{name}");
            assert_eq!(
                bed.textures,
                BlockTextures::column(top, side, textures::PLANKS),
                "{name}"
            );
            assert_eq!(bed.sky_attenuation, 1, "{name}");
            assert_eq!(bed.attenuation, [1; 3], "{name}");
        }
    }

    #[test]
    fn light_content_items_recipes_and_food_are_append_only() {
        let reg = Registry::base();
        let item = |name: &str| reg.item(name_id(&reg, name));
        let block = |name: &str| reg.id(name).unwrap();

        assert_eq!(item("openblock:seeds").max_stack, 64);
        assert_eq!(item("openblock:seeds").food, None);
        assert_eq!(item("openblock:wheat").max_stack, 64);
        assert_eq!(item("openblock:wheat").food, None);
        assert!(item("openblock:bread").edible());
        assert_eq!(item("openblock:bread").food, Some(5));

        for (name, tier, durability) in [
            ("openblock:wood_hoe", 1, 59),
            ("openblock:stone_hoe", 2, 131),
            ("openblock:iron_hoe", 3, 250),
        ] {
            let hoe = item(name).tool.expect("hoe is a tool");
            assert_eq!(hoe.kind, ToolKind::Hoe, "{name}");
            assert_eq!(hoe.tier, tier, "{name}");
            assert_eq!(hoe.durability, durability, "{name}");
            assert!(hoe.attack_damage <= 1.0, "{name}");
            assert_eq!(item(name).max_stack, 1, "{name}");
        }

        let recipe = |name: &str| *reg.recipe(reg.recipe_id(name).unwrap()).unwrap();
        let torch = recipe("openblock:torch");
        assert_eq!(torch.output, (name_id(&reg, "openblock:torch"), 4));
        assert_eq!(
            torch.ingredients,
            &[
                (name_id(&reg, "openblock:coal"), 1),
                (name_id(&reg, "openblock:stick"), 1)
            ]
        );
        assert!(!torch.needs_table);

        let bread = recipe("openblock:bread");
        assert_eq!(bread.output, (name_id(&reg, "openblock:bread"), 1));
        assert_eq!(bread.ingredients, &[(name_id(&reg, "openblock:wheat"), 3)]);

        for (hoe, material, output) in [
            ("openblock:wood_hoe", "openblock:planks", "openblock:wood_hoe"),
            ("openblock:stone_hoe", "openblock:cobblestone", "openblock:stone_hoe"),
            ("openblock:iron_hoe", "openblock:iron_ingot", "openblock:iron_hoe"),
        ] {
            let recipe = recipe(hoe);
            assert_eq!(recipe.output, (name_id(&reg, output), 1), "{hoe}");
            assert_eq!(
                recipe.ingredients,
                &[(name_id(&reg, material), 2), (name_id(&reg, "openblock:stick"), 2)],
                "{hoe}"
            );
            assert!(recipe.needs_table, "{hoe}");
        }

        // Torch and bed block items exist so a placed torch is recoverable.
        assert!(reg.item_for_block(block("openblock:torch")).is_some());
        assert!(reg.item_for_block(block("openblock:bed_foot")).is_some());
    }
}

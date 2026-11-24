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
//! # Items, tools and recipes (interim)
//!
//! Every non-air block gets a matching block item (`id` = the block's resource
//! id). The catalogue also contains `openblock:apple`, `openblock:stick` and the
//! eight wooden/stone tools. Item ids are frozen: v0.0.7 positions never move,
//! appended block items follow the apple, and new non-block items follow them.
//! Recipes are code-defined ingredient totals in deterministic id order;
//! shaped/grid crafting, item components and block loot tables arrive with the
//! data-driven content pipeline in
//! `docs/spec/11-inventory-crafting-items.md`.

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
}

impl ToolKind {
    /// Stable lower-case name used by content packs and hints.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pickaxe => "pickaxe",
            Self::Axe => "axe",
            Self::Shovel => "shovel",
            Self::Sword => "sword",
        }
    }
}

/// Mining statistics for one tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolSpec {
    /// Which blocks this tool is efficient against.
    pub kind: ToolKind,
    /// Harvest tier: a block with `required_tier <= tier` drops when mined.
    pub tier: u8,
    /// Mining-speed multiplier against blocks of the matching kind.
    pub speed: f32,
    /// Durability in blocks broken before the tool disappears.
    pub durability: u16,
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

    /// The block this item places, for block items.
    pub fn block(&self) -> Option<BlockId> {
        self.block
    }
}

/// Largest stack any base item declares. The server keeps this as a defensive
/// cap so a malformed pack cannot request an unbounded stack.
pub const MAX_ITEM_STACK: u8 = 64;

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

    /// Number of texture layers the renderer must generate.
    pub const COUNT: usize = 18;

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
    ];

    // Compile-time guard: the layer table and the catalogue must stay in sync.
    const _: () = assert!(NAMES.len() == COUNT);
}

/// Per-face texture set: top, side, bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockTextures {
    /// Texture for the +Y face.
    pub top: TextureId,
    /// Texture for the four side faces.
    pub side: TextureId,
    /// Texture for the -Y face.
    pub bottom: TextureId,
}

impl BlockTextures {
    /// One texture for every face.
    pub const fn all(id: TextureId) -> Self {
        Self {
            top: id,
            side: id,
            bottom: id,
        }
    }

    /// Distinct top and side textures with a shared bottom.
    pub const fn column(top: TextureId, side: TextureId, bottom: TextureId) -> Self {
        Self { top, side, bottom }
    }

    /// Texture for a face direction: 0 = +Y, 1 = -Y, 2..=5 = sides.
    pub fn for_face(&self, face: u8) -> TextureId {
        match face {
            0 => self.top,
            1 => self.bottom,
            _ => self.side,
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
}

impl BlockType {
    /// Whether the player can break this block by hand.
    pub fn breakable(&self) -> bool {
        self.hardness.is_finite()
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

/// Resource ids whose item ids are frozen at their v0.0.7 values.
///
/// New blocks are appended after the v0.0.7 set, and their block items are
/// appended after these items and the apple so existing item ids never move.
const STABLE_ITEM_ORDER: [&str; 12] = [
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
    "openblock:apple",
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
            durability: 131,
        }),
    ),
];

/// One name-resolved recipe definition used to build [`Registry::base`].
struct RecipeDef {
    id: &'static str,
    name: &'static str,
    output: (&'static str, u8),
    ingredients: &'static [(&'static str, u8)],
    needs_table: bool,
}

const BASE_RECIPES: [RecipeDef; 11] = [
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
];

impl Registry {
    /// Builds the first-party base block set.
    pub fn base() -> Self {
        use textures as tex;
        let defs: [BlockDef; 15] = [
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
        ];
        let blocks = defs
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
                },
            )
            .collect();
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

        // Interim code-defined item catalogue. Block ids are append-only, so
        // item ids must not shift when a new block is appended: the v0.0.7
        // items keep their frozen positions, the apple stays at its v0.0.7 id,
        // and only then do block items for newly appended blocks and the
        // non-block items follow. Data-driven item packs will replace this
        // derivation without changing the lookup API.
        fn add_block_item(
            blocks: &[BlockType],
            block_index: usize,
            items: &mut Vec<ItemType>,
            item_by_id: &mut FxHashMap<&'static str, ItemId>,
            item_for_block: &mut [Option<ItemId>],
        ) {
            let block = &blocks[block_index];
            if block.id == pack::AIR_ID || item_for_block[block_index].is_some() {
                return;
            }
            let item_id = ItemId(items.len() as u16);
            items.push(ItemType {
                id: block.id,
                block: Some(BlockId(block_index as u16)),
                food: None,
                tool: None,
                max_stack: MAX_ITEM_STACK,
            });
            item_by_id.insert(block.id, item_id);
            item_for_block[block_index] = Some(item_id);
        }

        let mut items: Vec<ItemType> = Vec::with_capacity(blocks.len() + NON_BLOCK_ITEMS.len());
        let mut item_by_id = FxHashMap::default();
        let mut item_for_block = vec![None; blocks.len()];

        // Frozen v0.0.7 positions: the eleven original block items, then apple.
        for name in STABLE_ITEM_ORDER.iter().filter(|name| **name != "openblock:apple") {
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
        // A pack may legitimately define a block with this resource id; in that
        // case the block item wins and the interim apple is omitted rather than
        // making the registry invalid.
        const APPLE_ID: &str = "openblock:apple";
        if !by_id.contains_key(APPLE_ID) {
            item_by_id.insert(APPLE_ID, ItemId(items.len() as u16));
            items.push(ItemType {
                id: APPLE_ID,
                block: None,
                food: Some(6),
                tool: None,
                max_stack: 16,
            });
        }

        // Block items for blocks appended after the frozen v0.0.7 catalogue.
        for block_index in 0..blocks.len() {
            add_block_item(&blocks, block_index, &mut items, &mut item_by_id, &mut item_for_block);
        }

        // New non-block items, appended after the apple.
        for (name, max_stack, tool) in NON_BLOCK_ITEMS {
            if item_by_id.contains_key(name) {
                continue;
            }
            item_by_id.insert(name, ItemId(items.len() as u16));
            items.push(ItemType {
                id: name,
                block: None,
                food: None,
                tool,
                max_stack,
            });
        }

        // Resolve the base recipe table against this registry. A custom pack
        // that lacks an ingredient or output simply omits that recipe; the base
        // pack contains everything, so all eleven are present there.
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

        Ok(Self {
            blocks,
            by_id,
            items,
            item_by_id,
            item_for_block,
            recipes,
            recipe_by_id,
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
    /// dirt, leaves drop nothing here (the Host adds a deterministic rare
    /// apple), and unbreakable blocks (water, bedrock) and air drop nothing. A
    /// block loot table in the content pack will replace these hard-coded rules
    /// later; callers must still apply [`Registry::can_drop`] for tiering.
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
        assert_eq!(reg.len(), 15);
        assert_eq!(reg.block(reg.id("openblock:bedrock").unwrap()).hardness, f32::INFINITY);
        assert!(!reg.block(reg.id("openblock:bedrock").unwrap()).breakable());
        assert_eq!(reg.block(reg.id("openblock:stone").unwrap()).hardness, 1.5);
        assert_eq!(reg.id("openblock:planks"), Some(BlockId(12)));
        assert_eq!(reg.id("openblock:cobblestone"), Some(BlockId(13)));
        assert_eq!(reg.id("openblock:crafting_table"), Some(BlockId(14)));
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
        assert_eq!(textures::COUNT, 18);
        assert_eq!(textures::NAMES[14], "planks");
        assert_eq!(textures::NAMES[15], "cobblestone");
        assert_eq!(textures::NAMES[16], "crafting_table_top");
        assert_eq!(textures::NAMES[17], "crafting_table_side");
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
        // v0.0.7 item ids are frozen: eleven block items, then apple. New block
        // items and non-block items are appended after the apple.
        assert_eq!(reg.item_len(), 24);
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
                durability: 59
            })
        );
        assert_eq!(
            stone_pick.tool,
            Some(ToolSpec {
                kind: ToolKind::Pickaxe,
                tier: 2,
                speed: 4.0,
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
    }

    fn name_id(reg: &Registry, name: &str) -> ItemId {
        reg.item_id(name).unwrap_or_else(|| panic!("missing item {name}"))
    }

    #[test]
    fn recipe_table_is_stable_and_complete() {
        let reg = Registry::base();
        assert_eq!(reg.recipe_len(), 11);
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
        let stone_sword = recipe("openblock:stone_sword");
        assert_eq!(
            stone_sword.ingredients,
            &[
                (name_id(&reg, "openblock:cobblestone"), 2),
                (name_id(&reg, "openblock:stick"), 1)
            ]
        );
        for (_, recipe) in reg.recipes() {
            let total: u32 = recipe.ingredients.iter().map(|(_, count)| u32::from(*count)).sum();
            assert!(total > 0);
            if recipe.id.starts_with("openblock:stone_") {
                assert!(recipe.needs_table);
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
        for name in ["openblock:planks", "openblock:cobblestone", "openblock:crafting_table"] {
            assert_eq!(
                reg.block_drop(block(name)),
                Some((item(name), 1)),
                "{name} must drop itself"
            );
        }
    }
}

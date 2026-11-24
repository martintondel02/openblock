//! The content model: block types, textures and tags.
//!
//! This is the v0.0.1 in-code registry. The data-driven content-pack pipeline
//! described in `docs/spec/15-content-system.md` replaces the hard-coded
//! `Registry::base()` at milestone MS-05 without changing this public API.
//!
//! The first slice of that pipeline lives in [`pack`]: [`registry_from_pack`]
//! loads the versioned JSON block schema and builds a [`Registry`] through the
//! same [`Registry::from_blocks`] path used by [`Registry::base`].

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

    /// Number of texture layers the renderer must generate.
    pub const COUNT: usize = 14;

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
    ];
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
    /// Creative-mode instant breaking and tool multipliers arrive with the
    /// inventory/tool system; the client currently uses this as hold-to-mine
    /// duration.
    pub hardness: f32,
}

impl BlockType {
    /// Whether the player can break this block by hand.
    pub fn breakable(&self) -> bool {
        self.hardness.is_finite()
    }
}

/// A frozen registry of block types for a session.
#[derive(Debug, Clone)]
pub struct Registry {
    blocks: Vec<BlockType>,
    by_id: FxHashMap<&'static str, BlockId>,
}

/// Raw definition tuple used while building the base registry.
type BlockDef = (&'static str, [f32; 3], bool, bool, bool, BlockTextures, f32);

impl Registry {
    /// Builds the first-party base block set.
    pub fn base() -> Self {
        use textures as tex;
        let defs: [BlockDef; 12] = [
            (
                "openblock:air",
                [0.0, 0.0, 0.0],
                false,
                false,
                false,
                BlockTextures::all(tex::DEBUG),
                0.0,
            ),
            (
                "openblock:stone",
                [0.48, 0.48, 0.50],
                true,
                true,
                false,
                BlockTextures::all(tex::STONE),
                1.5,
            ),
            (
                "openblock:dirt",
                [0.42, 0.30, 0.20],
                true,
                true,
                false,
                BlockTextures::all(tex::DIRT),
                0.5,
            ),
            (
                "openblock:grass",
                [0.33, 0.55, 0.25],
                true,
                true,
                false,
                BlockTextures::column(tex::GRASS_TOP, tex::GRASS_SIDE, tex::DIRT),
                0.6,
            ),
            (
                "openblock:sand",
                [0.85, 0.80, 0.55],
                true,
                true,
                false,
                BlockTextures::all(tex::SAND),
                0.5,
            ),
            (
                "openblock:water",
                [0.20, 0.42, 0.75],
                false,
                false,
                true,
                BlockTextures::all(tex::WATER),
                f32::INFINITY,
            ),
            (
                "openblock:log",
                [0.36, 0.26, 0.15],
                true,
                true,
                false,
                BlockTextures::column(tex::LOG_TOP, tex::LOG_SIDE, tex::LOG_TOP),
                2.0,
            ),
            (
                "openblock:leaves",
                [0.18, 0.40, 0.16],
                true,
                false,
                false,
                BlockTextures::all(tex::LEAVES),
                0.2,
            ),
            (
                "openblock:bedrock",
                [0.15, 0.15, 0.17],
                true,
                true,
                false,
                BlockTextures::all(tex::BEDROCK),
                f32::INFINITY,
            ),
            (
                "openblock:gravel",
                [0.45, 0.43, 0.41],
                true,
                true,
                false,
                BlockTextures::all(tex::GRAVEL),
                0.6,
            ),
            (
                "openblock:clay",
                [0.55, 0.56, 0.58],
                true,
                true,
                false,
                BlockTextures::all(tex::CLAY),
                0.6,
            ),
            (
                "openblock:snow",
                [0.92, 0.94, 0.97],
                true,
                true,
                false,
                BlockTextures::all(tex::SNOW),
                0.1,
            ),
        ];
        let blocks = defs
            .into_iter()
            .map(|(id, color, solid, opaque, fluid, textures, hardness)| BlockType {
                id,
                color,
                solid,
                opaque,
                fluid,
                textures,
                hardness,
            })
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
        Ok(Self { blocks, by_id })
    }

    /// Looks up a block type by dense id.
    pub fn block(&self, id: BlockId) -> &BlockType {
        &self.blocks[id.0 as usize]
    }

    /// Looks up a block id by resource id.
    pub fn id(&self, id: &str) -> Option<BlockId> {
        self.by_id.get(id).copied()
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
        assert_eq!(reg.len(), 12);
        assert_eq!(reg.block(reg.id("openblock:bedrock").unwrap()).hardness, f32::INFINITY);
        assert!(!reg.block(reg.id("openblock:bedrock").unwrap()).breakable());
        assert_eq!(reg.block(reg.id("openblock:stone").unwrap()).hardness, 1.5);
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
    }

    #[test]
    fn grass_has_distinct_face_textures() {
        let reg = Registry::base();
        let grass = reg.block(reg.id("openblock:grass").unwrap());
        assert_eq!(grass.textures.top, textures::GRASS_TOP);
        assert_eq!(grass.textures.side, textures::GRASS_SIDE);
        assert_eq!(grass.textures.bottom, textures::DIRT);
    }
}

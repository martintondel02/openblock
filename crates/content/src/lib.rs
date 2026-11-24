//! The content model: block and item types with namespaced ids and tags.
//!
//! This is the v0.0.1 in-code registry. The data-driven content-pack pipeline
//! described in `docs/spec/15-content-system.md` replaces the hard-coded
//! `Registry::base()` at milestone MS-05 without changing this public API.

use rustc_hash::FxHashMap;

/// A dense, session-local block type index. Never persisted or networked.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u16);

impl BlockId {
    /// The canonical resource id of a block type, if known.
    pub const AIR: BlockId = BlockId(0);
}

/// A block type definition. Block states (properties) arrive with MS-01's
/// full palette work; v0.0.1 has one state per type.
#[derive(Debug, Clone)]
pub struct BlockType {
    /// Namespaced resource id, e.g. `openblock:grass`.
    pub id: &'static str,
    /// Base colour used by the v0.0.1 vertex-coloured renderer.
    pub color: [f32; 3],
    /// Blocks movement and can support an entity.
    pub solid: bool,
    /// Fully hides the neighbouring face (opaque for meshing).
    pub opaque: bool,
    /// Renders and behaves as a fluid (swimming/collision policy at MS-10).
    pub fluid: bool,
}

/// A frozen registry of block types for a session.
#[derive(Debug, Clone)]
pub struct Registry {
    blocks: Vec<BlockType>,
    by_id: FxHashMap<&'static str, BlockId>,
}

impl Registry {
    /// Builds the first-party base block set.
    pub fn base() -> Self {
        let defs = [
            ("openblock:air", [0.0, 0.0, 0.0], false, false, false),
            ("openblock:stone", [0.48, 0.48, 0.50], true, true, false),
            ("openblock:dirt", [0.42, 0.30, 0.20], true, true, false),
            ("openblock:grass", [0.33, 0.55, 0.25], true, true, false),
            ("openblock:sand", [0.85, 0.80, 0.55], true, true, false),
            ("openblock:water", [0.20, 0.42, 0.75], false, false, true),
            ("openblock:log", [0.36, 0.26, 0.15], true, true, false),
            ("openblock:leaves", [0.18, 0.40, 0.16], true, false, false),
            ("openblock:bedrock", [0.15, 0.15, 0.17], true, true, false),
        ];
        let mut blocks = Vec::with_capacity(defs.len());
        let mut by_id = FxHashMap::default();
        for (index, (id, color, solid, opaque, fluid)) in defs.into_iter().enumerate() {
            by_id.insert(id, BlockId(index as u16));
            blocks.push(BlockType {
                id,
                color,
                solid,
                opaque,
                fluid,
            });
        }
        Self { blocks, by_id }
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
        assert_eq!(reg.len(), 9);
    }
}

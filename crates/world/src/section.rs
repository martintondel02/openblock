//! Paletted 16³ section storage (DEC-004, DEC-005).

use crate::light::Light;
use ob_content::{BlockId, Registry};
use ob_math::{LocalPos, SECTION_SIZE};

/// Number of blocks in one section.
pub const BLOCK_COUNT: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;

/// The index width used by a section's palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// One block type fills the section (zero storage).
    Uniform,
    /// 4-bit indices, at most 16 distinct block types.
    Bits4,
    /// 8-bit indices, at most 256 distinct block types.
    Bits8,
    /// 16-bit indices, up to 65,536 distinct block types.
    Bits16,
}

/// Block storage for one section.
///
/// Indices are packed into `u64` words; an index never straddles a word
/// boundary. When the palette outgrows the current width the section is
/// widened (4 → 8 → 16 bits) without changing the public API.
#[derive(Debug, Clone)]
pub enum SectionStorage {
    /// A single block type.
    Uniform(BlockId),
    /// 4-bit indices with a palette.
    Bits4 {
        /// Packed indices.
        words: Vec<u64>,
        /// Palette entries, at most 16.
        palette: Vec<BlockId>,
    },
    /// 8-bit indices with a palette.
    Bits8 {
        /// Packed indices.
        words: Vec<u64>,
        /// Palette entries, at most 256.
        palette: Vec<BlockId>,
    },
    /// 16-bit indices with a palette.
    Bits16 {
        /// Packed indices.
        words: Vec<u64>,
        /// Palette entries, up to 65,536.
        palette: Vec<BlockId>,
    },
}

impl SectionStorage {
    fn mode(&self) -> StorageMode {
        match self {
            SectionStorage::Uniform(_) => StorageMode::Uniform,
            SectionStorage::Bits4 { .. } => StorageMode::Bits4,
            SectionStorage::Bits8 { .. } => StorageMode::Bits8,
            SectionStorage::Bits16 { .. } => StorageMode::Bits16,
        }
    }

    fn empty_words(bits: usize) -> Vec<u64> {
        let per_word = 64 / bits;
        vec![0u64; BLOCK_COUNT.div_ceil(per_word)]
    }

    fn get(&self, index: usize) -> BlockId {
        match self {
            SectionStorage::Uniform(id) => *id,
            SectionStorage::Bits4 { words, palette } => {
                let (word, shift) = (index / 16, (index % 16) * 4);
                let value = ((words[word] >> shift) & 0xF) as usize;
                palette[value]
            }
            SectionStorage::Bits8 { words, palette } => {
                let (word, shift) = (index / 8, (index % 8) * 8);
                let value = ((words[word] >> shift) & 0xFF) as usize;
                palette[value]
            }
            SectionStorage::Bits16 { words, palette } => {
                let (word, shift) = (index / 4, (index % 4) * 16);
                let value = ((words[word] >> shift) & 0xFFFF) as usize;
                palette[value]
            }
        }
    }

    fn set(&mut self, index: usize, value: BlockId) {
        match self {
            SectionStorage::Uniform(current) => {
                if *current == value {
                    return;
                }
                // Promote to 4-bit storage with two palette entries.
                let mut words = Self::empty_words(4);
                let palette = vec![*current, value];
                let mut set_index = |i: usize, v: usize| {
                    let (word, shift) = (i / 16, (i % 16) * 4);
                    words[word] |= (v as u64) << shift;
                };
                for i in 0..BLOCK_COUNT {
                    set_index(i, 0);
                }
                set_index(index, 1);
                *self = SectionStorage::Bits4 { words, palette };
            }
            SectionStorage::Bits4 { words, palette } => {
                if let Some(p) = palette.iter().position(|b| *b == value) {
                    let (word, shift) = (index / 16, (index % 16) * 4);
                    words[word] = (words[word] & !(0xFu64 << shift)) | ((p as u64) << shift);
                } else if palette.len() < 16 {
                    palette.push(value);
                    let p = palette.len() - 1;
                    let (word, shift) = (index / 16, (index % 16) * 4);
                    words[word] = (words[word] & !(0xFu64 << shift)) | ((p as u64) << shift);
                } else {
                    self.widen_to_bits8(index, value);
                }
            }
            SectionStorage::Bits8 { words, palette } => {
                if let Some(p) = palette.iter().position(|b| *b == value) {
                    let (word, shift) = (index / 8, (index % 8) * 8);
                    words[word] = (words[word] & !(0xFFu64 << shift)) | ((p as u64) << shift);
                } else if palette.len() < 256 {
                    palette.push(value);
                    let p = palette.len() - 1;
                    let (word, shift) = (index / 8, (index % 8) * 8);
                    words[word] = (words[word] & !(0xFFu64 << shift)) | ((p as u64) << shift);
                } else {
                    self.widen_to_bits16(index, value);
                }
            }
            SectionStorage::Bits16 { .. } => {
                // The 16-bit palette is the final storage width and has no
                // practical overflow for a session registry (u16 ids).
                if let SectionStorage::Bits16 { words, palette } = self {
                    let p = match palette.iter().position(|b| *b == value) {
                        Some(p) => p,
                        None => {
                            palette.push(value);
                            palette.len() - 1
                        }
                    };
                    let (word, shift) = (index / 4, (index % 4) * 16);
                    words[word] = (words[word] & !(0xFFFFu64 << shift)) | ((p as u64) << shift);
                }
            }
        }
    }

    fn widen_to_bits8(&mut self, index: usize, value: BlockId) {
        if let SectionStorage::Bits4 { words, palette } = self {
            let mut new_palette = palette.clone();
            new_palette.push(value);
            let mut new_words = Self::empty_words(8);
            let old = SectionStorage::Bits4 {
                words: std::mem::take(words),
                palette: palette.clone(),
            };
            for i in 0..BLOCK_COUNT {
                let id = old.get(i);
                let p = new_palette.iter().position(|b| *b == id).unwrap_or(0);
                let (word, shift) = (i / 8, (i % 8) * 8);
                new_words[word] |= (p as u64) << shift;
            }
            let p = new_palette.len() - 1;
            let (word, shift) = (index / 8, (index % 8) * 8);
            new_words[word] = (new_words[word] & !(0xFFu64 << shift)) | ((p as u64) << shift);
            *self = SectionStorage::Bits8 {
                words: new_words,
                palette: new_palette,
            };
        }
    }

    fn widen_to_bits16(&mut self, index: usize, value: BlockId) {
        if let SectionStorage::Bits8 { words, palette } = self {
            let mut new_palette = palette.clone();
            new_palette.push(value);
            let mut new_words = Self::empty_words(16);
            let old = SectionStorage::Bits8 {
                words: std::mem::take(words),
                palette: palette.clone(),
            };
            for i in 0..BLOCK_COUNT {
                let id = old.get(i);
                let p = new_palette.iter().position(|b| *b == id).unwrap_or(0);
                let (word, shift) = (i / 4, (i % 4) * 16);
                new_words[word] |= (p as u64) << shift;
            }
            let p = new_palette.len() - 1;
            let (word, shift) = (index / 4, (index % 4) * 16);
            new_words[word] = (new_words[word] & !(0xFFFFu64 << shift)) | ((p as u64) << shift);
            *self = SectionStorage::Bits16 {
                words: new_words,
                palette: new_palette,
            };
        }
    }
}

/// One 16³ section: block storage plus packed light.
#[derive(Debug, Clone)]
pub struct Section {
    storage: SectionStorage,
    light: Light,
    non_air: u32,
}

impl Section {
    /// Creates a section filled with one block type.
    pub fn uniform(id: BlockId) -> Self {
        Self {
            storage: SectionStorage::Uniform(id),
            light: Light::uniform_sky(15),
            non_air: if id == BlockId::AIR { 0 } else { BLOCK_COUNT as u32 },
        }
    }

    /// Creates an empty (air) section.
    pub fn empty() -> Self {
        Self::uniform(BlockId::AIR)
    }

    /// The storage mode currently in use.
    pub fn mode(&self) -> StorageMode {
        self.storage.mode()
    }

    /// Number of non-air blocks in the section.
    pub fn non_air_count(&self) -> u32 {
        self.non_air
    }

    /// Whether the section is empty (all air).
    pub fn is_empty(&self) -> bool {
        self.non_air == 0
    }

    /// Reads a block by local position.
    pub fn get(&self, pos: LocalPos) -> BlockId {
        self.storage.get(pos.index())
    }

    /// Writes a block by local position, maintaining the non-air count.
    pub fn set(&mut self, pos: LocalPos, value: BlockId) {
        let index = pos.index();
        let previous = self.storage.get(index);
        if previous == value {
            return;
        }
        self.storage.set(index, value);
        let was_air = previous == BlockId::AIR;
        let is_air = value == BlockId::AIR;
        match (was_air, is_air) {
            (true, false) => self.non_air += 1,
            (false, true) => self.non_air -= 1,
            _ => {}
        }
    }

    /// Immutable access to the section light container.
    pub fn light(&self) -> &Light {
        &self.light
    }

    /// Mutable access to the section light container.
    pub fn light_mut(&mut self) -> &mut Light {
        &mut self.light
    }

    /// Immutable access to the raw storage (persistence).
    pub fn storage(&self) -> &SectionStorage {
        &self.storage
    }

    /// Rebuilds a section from raw storage and light (persistence).
    pub fn from_parts(storage: SectionStorage, light: Light) -> Self {
        let mut non_air = 0u32;
        for i in 0..BLOCK_COUNT {
            if storage.get(i) != BlockId::AIR {
                non_air += 1;
            }
        }
        Self {
            storage,
            light,
            non_air,
        }
    }

    /// Palette entries of the storage (for persistence).
    pub fn palette(&self) -> &[BlockId] {
        match &self.storage {
            SectionStorage::Uniform(_) => &[],
            SectionStorage::Bits4 { palette, .. }
            | SectionStorage::Bits8 { palette, .. }
            | SectionStorage::Bits16 { palette, .. } => palette,
        }
    }

    /// Packed index words of the storage (for persistence).
    pub fn words(&self) -> &[u64] {
        match &self.storage {
            SectionStorage::Uniform(_) => &[],
            SectionStorage::Bits4 { words, .. }
            | SectionStorage::Bits8 { words, .. }
            | SectionStorage::Bits16 { words, .. } => words,
        }
    }

    /// Approximate heap bytes used by block data (for memory accounting).
    pub fn block_bytes(&self) -> usize {
        match &self.storage {
            SectionStorage::Uniform(_) => 0,
            SectionStorage::Bits4 { words, palette } => words.len() * 8 + palette.len() * 2,
            SectionStorage::Bits8 { words, palette } => words.len() * 8 + palette.len() * 2,
            SectionStorage::Bits16 { words, palette } => words.len() * 8 + palette.len() * 2,
        }
    }

    /// Fills the section with the given block type and regenerates its light.
    pub fn fill(&mut self, id: BlockId) {
        self.storage = SectionStorage::Uniform(id);
        self.non_air = if id == BlockId::AIR { 0 } else { BLOCK_COUNT as u32 };
        self.light = Light::uniform_sky(15);
    }

    /// Clears the block-light channels while preserving sky light.
    ///
    /// Used when seed light is recomputed after generation or a migration; the
    /// sky channel is left untouched because the caller owns the column loop.
    pub fn clear_block_light(&mut self) {
        if let Some(uniform) = self.light.uniform_value() {
            let sky = uniform >> 12;
            self.light = Light::Uniform(sky << 12);
            return;
        }
        let mut dense = [0u16; BLOCK_COUNT];
        for (index, word) in dense.iter_mut().enumerate() {
            *word = self.light.get(LocalPos::from_index(index)) & 0xF000;
        }
        self.light = Light::Dense(Box::new(dense));
    }

    /// Clears the sky and/or RGB block-light nibbles inside a local box.
    ///
    /// `min` and `max` are inclusive section-local coordinates. Keeping this
    /// on `Section` avoids one `World`/section lookup and hash invalidation per
    /// cell when an edit relights a 31³ box.
    pub(crate) fn clear_light_box(&mut self, min: [u8; 3], max: [u8; 3], sky: bool, block: bool) {
        if !sky && !block {
            return;
        }
        if let Some(word) = self.light.uniform_value() {
            let mut cleared = word;
            if sky {
                cleared &= 0x0FFF;
            }
            if block {
                cleared &= 0xF000;
            }
            if cleared == word {
                return;
            }
            let whole_section = min == [0, 0, 0] && max == [SECTION_SIZE as u8 - 1; 3];
            if whole_section {
                self.light = Light::Uniform(cleared);
                return;
            }
            // Clearing only part of a uniform section must not darken the
            // cells outside the box: promote and apply the cleared word only
            // inside [min, max].
            let mut data = Box::new([word; BLOCK_COUNT]);
            for y in min[1]..=max[1] {
                for z in min[2]..=max[2] {
                    for x in min[0]..=max[0] {
                        data[LocalPos { x, y, z }.index()] = cleared;
                    }
                }
            }
            self.light = Light::Dense(data);
            return;
        }
        let data = match &mut self.light {
            Light::Dense(data) => data,
            Light::Uniform(_) => return,
        };
        for y in min[1]..=max[1] {
            for z in min[2]..=max[2] {
                for x in min[0]..=max[0] {
                    let index = LocalPos { x, y, z }.index();
                    let word = &mut data[index];
                    if sky {
                        *word &= 0x0FFF;
                    }
                    if block {
                        *word &= 0xF000;
                    }
                }
            }
        }
    }

    /// Clears the sky nibble of every cell while preserving RGB block light.
    ///
    /// Used by the sky relight volume for a section proven fully opaque: no
    /// cell there can hold sky light, and the whole-section clear avoids 4,096
    /// per-cell writes when a column passes through solid rock.
    pub(crate) fn clear_sky(&mut self) {
        match &mut self.light {
            Light::Uniform(word) => *word &= 0x0FFF,
            Light::Dense(data) => {
                for word in data.iter_mut() {
                    *word &= 0x0FFF;
                }
            }
        }
    }

    /// Replaces the light container with a uniform sky value, dropping block
    /// light. Used by the column seeder before block-light sources are re-added.
    pub(crate) fn reset_light_uniform_sky(&mut self, sky: u8) {
        self.light = Light::uniform_sky(sky);
    }

    /// Whether every block in this section stops sky light completely.
    ///
    /// Such a section can never store a non-zero sky level, so the column
    /// seeder may reset the whole container to uniform zero without touching
    /// 4,096 cells. Sections with air, water, leaves or crops are not opaque
    /// and are cleared column by column instead; this is what keeps a shaft
    /// column's light alive when a later solid column is processed.
    pub(crate) fn sky_is_opaque(&self, registry: &Registry) -> bool {
        match &self.storage {
            SectionStorage::Uniform(id) => registry.block(*id).sky_attenuation >= 15,
            SectionStorage::Bits4 { palette, .. }
            | SectionStorage::Bits8 { palette, .. }
            | SectionStorage::Bits16 { palette, .. } => {
                palette.iter().all(|id| registry.block(*id).sky_attenuation >= 15)
            }
        }
    }

    /// Whether this section's palette contains any emissive block type.
    ///
    /// Bulk generation has no light sources, so scanning 4,096 cells per solid
    /// section just to prove that is wasteful. A uniform section checks one id
    /// and a paletted section checks its (usually tiny) palette.
    pub fn may_have_emitters(&self, registry: &Registry) -> bool {
        match &self.storage {
            SectionStorage::Uniform(id) => registry.block(*id).emission != [0, 0, 0],
            SectionStorage::Bits4 { palette, .. }
            | SectionStorage::Bits8 { palette, .. }
            | SectionStorage::Bits16 { palette, .. } => {
                palette.iter().any(|id| registry.block(*id).emission != [0, 0, 0])
            }
        }
    }

    /// Re-derives section-local sky light from `BlockType::sky_attenuation`.
    ///
    /// This is a per-cell approximation kept for callers that only have one
    /// section (hand-built fixtures and the worldgen benchmark's debug path).
    /// Column-aware generation and the live world use
    /// [`crate::ChunkColumn::finalise`], which performs the running top-down
    /// subtraction across all 24 sections; that is what makes caves dark.
    ///
    /// The result is built through [`Light::from_fn`], which collapses to a
    /// compact uniform container when every block receives the same sky light.
    pub fn seed_sky_light(&mut self, registry: &Registry) {
        let light = Light::from_fn(|pos| {
            let block = registry.block(self.get(pos));
            let sky = 15u8.saturating_sub(block.sky_attenuation);
            // Only the sky nibble is reseeded; existing block light is kept.
            let (red, green, blue) = self.light().block_rgb(pos);
            (u16::from(sky) << 12) | (u16::from(red) << 8) | (u16::from(green) << 4) | u16::from(blue)
        });
        self.light = light;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::{LocalPos, SectionPos};

    fn pos(x: u8, y: u8, z: u8) -> LocalPos {
        LocalPos { x, y, z }
    }

    #[test]
    fn uniform_section_costs_nothing() {
        let s = Section::uniform(BlockId(1));
        assert_eq!(s.mode(), StorageMode::Uniform);
        assert_eq!(s.block_bytes(), 0);
        assert_eq!(s.get(pos(0, 0, 0)), BlockId(1));
    }

    #[test]
    fn set_promotes_to_bits4() {
        let mut s = Section::uniform(BlockId(1));
        s.set(pos(3, 4, 5), BlockId(2));
        assert_eq!(s.mode(), StorageMode::Bits4);
        assert_eq!(s.get(pos(3, 4, 5)), BlockId(2));
        assert_eq!(s.get(pos(0, 0, 0)), BlockId(1));
    }

    #[test]
    fn palette_widens_to_8_and_16_bits() {
        let mut s = Section::empty();
        for i in 0..17u16 {
            s.set(LocalPos::from_index(i as usize), BlockId(i + 1));
        }
        assert_eq!(s.mode(), StorageMode::Bits8);
        for i in 0..17u16 {
            assert_eq!(s.get(LocalPos::from_index(i as usize)), BlockId(i + 1));
        }
        for i in 0..300u16 {
            s.set(LocalPos::from_index(i as usize), BlockId(1000 + i));
        }
        assert_eq!(s.mode(), StorageMode::Bits16);
        for i in 0..300u16 {
            assert_eq!(s.get(LocalPos::from_index(i as usize)), BlockId(1000 + i));
        }
    }

    #[test]
    fn non_air_count_tracks_edits() {
        let mut s = Section::empty();
        assert_eq!(s.non_air_count(), 0);
        s.set(pos(1, 2, 3), BlockId(1));
        assert_eq!(s.non_air_count(), 1);
        s.set(pos(1, 2, 3), BlockId::AIR);
        assert_eq!(s.non_air_count(), 0);
    }

    #[test]
    fn light_nibbles_round_trip() {
        let mut s = Section::empty();
        s.light_mut().set_sky(pos(1, 2, 3), 9);
        s.light_mut().set_block(pos(1, 2, 3), 4, 5, 6);
        assert_eq!(s.light().sky(pos(1, 2, 3)), 9);
        assert_eq!(s.light().block_rgb(pos(1, 2, 3)), (4, 5, 6));
    }

    #[test]
    fn seed_sky_light_keeps_uniform_sections_compact() {
        use ob_content::Registry;

        let registry = Registry::base();
        let stone = registry.id_or_air("openblock:stone");
        let water = registry.id_or_air("openblock:water");

        let mut solid = Section::uniform(stone);
        solid.seed_sky_light(&registry);
        assert_eq!(solid.light().uniform_value(), Some(0));
        assert_eq!(solid.light().heap_bytes(), 0);
        assert!(solid.light().is_uniform_zero());
        assert_eq!(solid.light().sky(pos(3, 7, 9)), 0);

        let mut open = Section::empty();
        open.seed_sky_light(&registry);
        assert_eq!(open.light().uniform_value(), Some(15 << 12));
        assert_eq!(open.light().heap_bytes(), 0);
        assert_eq!(open.light().sky(pos(3, 7, 9)), 15);

        let mut fluid = Section::uniform(water);
        fluid.seed_sky_light(&registry);
        assert_eq!(fluid.light().uniform_value(), Some(14 << 12));
        assert_eq!(fluid.light().heap_bytes(), 0);

        // One opaque block among open air is not uniform and must stay dense.
        let mut mixed = Section::empty();
        mixed.set(pos(0, 0, 0), stone);
        mixed.seed_sky_light(&registry);
        assert_eq!(mixed.light().uniform_value(), None);
        assert_eq!(mixed.light().heap_bytes(), BLOCK_COUNT * 2);
        assert_eq!(mixed.light().sky(pos(0, 0, 0)), 0);
        assert_eq!(mixed.light().sky(pos(0, 1, 0)), 15);

        // Reseeding sky light must not erase existing block light.
        let mut lit = Section::empty();
        lit.light_mut().set_block(pos(2, 3, 4), 4, 5, 6);
        lit.seed_sky_light(&registry);
        assert_eq!(lit.light().block_rgb(pos(2, 3, 4)), (4, 5, 6));
        assert_eq!(lit.light().sky(pos(2, 3, 4)), 15);
        assert_eq!(lit.light().block_rgb(pos(0, 0, 0)), (0, 0, 0));
    }

    #[test]
    fn section_pos_index_is_stable() {
        let p = SectionPos::new(0, -4, 0);
        assert_eq!(p.index(), 0);
        let p = SectionPos::new(0, 19, 0);
        assert_eq!(p.index(), 23);
    }
}

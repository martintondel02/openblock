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

    /// Re-derives the section light from the registry's opacity rules and the
    /// vertical sky occlusion handled by [`crate::chunk::ChunkColumn`].
    pub fn seed_sky_light(&mut self, registry: &Registry) {
        for y in (0..SECTION_SIZE).rev() {
            for z in 0..SECTION_SIZE {
                for x in 0..SECTION_SIZE {
                    let pos = LocalPos {
                        x: x as u8,
                        y: y as u8,
                        z: z as u8,
                    };
                    let block = registry.block(self.get(pos));
                    if block.opaque {
                        self.light.set_sky(pos, 0);
                    } else if block.fluid {
                        self.light.set_sky(pos, 12);
                    } else {
                        self.light.set_sky(pos, 15);
                    }
                }
            }
        }
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
    fn section_pos_index_is_stable() {
        let p = SectionPos::new(0, -4, 0);
        assert_eq!(p.index(), 0);
        let p = SectionPos::new(0, 19, 0);
        assert_eq!(p.index(), 23);
    }
}

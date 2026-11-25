//! Chunk columns: 24 sections stacked from `WORLD_MIN_Y` to `WORLD_MAX_Y`.

use ob_content::{BlockId, Registry};
use ob_math::{BlockPos, ChunkPos, LocalPos, N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};

use crate::section::Section;

/// A vertical column of sections at one XZ chunk coordinate.
#[derive(Debug, Clone)]
pub struct ChunkColumn {
    pos: ChunkPos,
    sections: Vec<Section>,
    /// Highest non-air block Y per XZ column, relative to `WORLD_MIN_Y`.
    heightmap: Box<[i16; 256]>,
}

impl ChunkColumn {
    /// Creates an empty column.
    pub fn new(pos: ChunkPos) -> Self {
        Self {
            pos,
            sections: (0..N_SECTIONS).map(|_| Section::empty()).collect(),
            heightmap: Box::new([-1i16; 256]),
        }
    }

    /// Rebuilds a column from persisted sections, recomputing the heightmap.
    pub fn from_sections(pos: ChunkPos, sections: Vec<Section>) -> Self {
        assert_eq!(sections.len(), N_SECTIONS, "column must have N_SECTIONS sections");
        let mut column = Self {
            pos,
            sections,
            heightmap: Box::new([-1i16; 256]),
        };
        column.recompute_heightmap();
        column
    }

    /// Recomputes the heightmap from the current blocks.
    pub fn recompute_heightmap(&mut self) {
        *self.heightmap = [-1i16; 256];
        for y in (WORLD_MIN_Y..WORLD_MIN_Y + (N_SECTIONS * SECTION_SIZE) as i32).rev() {
            for z in 0..SECTION_SIZE {
                for x in 0..SECTION_SIZE {
                    let pos = BlockPos::new(
                        self.pos.x * SECTION_SIZE as i32 + x as i32,
                        y,
                        self.pos.z * SECTION_SIZE as i32 + z as i32,
                    );
                    let column = z * SECTION_SIZE + x;
                    if self.heightmap[column] < 0 && self.get_block(pos) != BlockId::AIR {
                        self.heightmap[column] = (y - WORLD_MIN_Y) as i16;
                    }
                }
            }
        }
    }

    /// The column's chunk position.
    pub fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Section by absolute section position.
    pub fn section(&self, pos: SectionPos) -> Option<&Section> {
        if pos.x != self.pos.x || pos.z != self.pos.z {
            return None;
        }
        self.sections.get(pos.index())
    }

    /// Mutable section by absolute section position.
    pub fn section_mut(&mut self, pos: SectionPos) -> Option<&mut Section> {
        if pos.x != self.pos.x || pos.z != self.pos.z {
            return None;
        }
        self.sections.get_mut(pos.index())
    }

    /// Iterates over the column's sections in ascending Y order.
    pub fn sections(&self) -> impl Iterator<Item = (SectionPos, &Section)> + '_ {
        self.sections.iter().enumerate().map(move |(index, section)| {
            (
                SectionPos::new(self.pos.x, WORLD_MIN_Y / SECTION_SIZE as i32 + index as i32, self.pos.z),
                section,
            )
        })
    }

    /// Reads a block by world position (the caller guarantees the chunk).
    pub fn get_block(&self, pos: BlockPos) -> BlockId {
        let section = match self.section(pos.section()) {
            Some(section) => section,
            None => return BlockId::AIR,
        };
        section.get(pos.local())
    }

    /// Writes a block by world position, updating the column heightmap.
    ///
    /// A position outside this column is ignored. The heightmap only changes
    /// when the write actually changes the topmost non-air block; in particular
    /// writing air (or re-writing the same id) above the current top must not
    /// raise it.
    pub fn set_block(&mut self, pos: BlockPos, value: BlockId) {
        let local = pos.local();
        let Some(section) = self.section_mut(pos.section()) else {
            return;
        };
        section.set(local, value);
        self.update_heightmap(local, pos.y);
    }

    fn update_heightmap(&mut self, local: LocalPos, world_y: i32) {
        let column = (local.z as usize) * SECTION_SIZE + local.x as usize;
        let relative = (world_y - WORLD_MIN_Y) as i16;
        if relative < 0 {
            return;
        }
        let current = self.heightmap[column];
        let world = BlockPos::new(
            self.pos.x * SECTION_SIZE as i32 + local.x as i32,
            world_y,
            self.pos.z * SECTION_SIZE as i32 + local.z as i32,
        );
        if relative > current {
            // Only a non-air block above the current top raises the heightmap.
            // A no-op/air write must not create a phantom surface that spawn
            // and mob-placement checks would trust.
            if self.get_block(world) != BlockId::AIR {
                self.heightmap[column] = relative;
            }
        } else if relative == current && self.get_block(world) == BlockId::AIR {
            // The top block was removed; rescan this column downwards. A
            // no-op or a replacement at the same height leaves it unchanged.
            let mut y = relative - 1;
            while y >= 0 {
                let block_pos = BlockPos::new(
                    self.pos.x * SECTION_SIZE as i32 + local.x as i32,
                    WORLD_MIN_Y + y as i32,
                    self.pos.z * SECTION_SIZE as i32 + local.z as i32,
                );
                if self.get_block(block_pos) != BlockId::AIR {
                    self.heightmap[column] = y;
                    return;
                }
                y -= 1;
            }
            self.heightmap[column] = -1;
        }
    }

    /// Highest non-air Y for a local XZ column, if any.
    pub fn height_at(&self, x: usize, z: usize) -> Option<i32> {
        let h = self.heightmap[z * SECTION_SIZE + x];
        (h >= 0).then_some(WORLD_MIN_Y + h as i32)
    }

    /// Recomputes the column heightmap and sky light after generation.
    pub fn finalise(&mut self, registry: &Registry) {
        for section in self.sections.iter_mut() {
            section.seed_sky_light(registry);
        }
    }

    /// Approximate heap bytes used by the column.
    pub fn heap_bytes(&self) -> usize {
        self.sections
            .iter()
            .map(|s| s.block_bytes() + s.light().heap_bytes())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::{ChunkPos, SectionPos, WORLD_MIN_Y};

    #[test]
    fn air_and_noop_writes_above_the_top_do_not_raise_the_heightmap() {
        let pos = ChunkPos::new(1, -2);
        let mut column = ChunkColumn::new(pos);
        let (origin_x, origin_z) = pos.origin();
        let top = BlockPos::new(origin_x + 3, WORLD_MIN_Y + 10, origin_z + 4);
        column.set_block(top, BlockId(1));
        assert_eq!(column.height_at(3, 4), Some(WORLD_MIN_Y + 10));

        let above = BlockPos::new(origin_x + 3, WORLD_MIN_Y + 15, origin_z + 4);
        column.set_block(above, BlockId(2));
        assert_eq!(column.height_at(3, 4), Some(WORLD_MIN_Y + 15));

        // A no-op non-air write above the top stays put.
        column.set_block(above, BlockId(2));
        assert_eq!(column.height_at(3, 4), Some(WORLD_MIN_Y + 15));

        // Writing air above the top must not invent a surface.
        column.set_block(above, BlockId::AIR);
        assert_eq!(column.height_at(3, 4), Some(WORLD_MIN_Y + 10));
        column.set_block(
            BlockPos::new(origin_x + 3, WORLD_MIN_Y + 20, origin_z + 4),
            BlockId::AIR,
        );
        assert_eq!(column.height_at(3, 4), Some(WORLD_MIN_Y + 10));

        // Removing the actual top rescans downwards.
        column.set_block(top, BlockId::AIR);
        assert_eq!(column.height_at(3, 4), None);
    }

    #[test]
    fn set_block_ignores_positions_outside_the_column() {
        let pos = ChunkPos::new(0, 0);
        let mut column = ChunkColumn::new(pos);
        let other = BlockPos::new(16, WORLD_MIN_Y + 3, 0);
        column.set_block(other, BlockId(1));
        assert_eq!(column.height_at(0, 0), None);
        assert!(column.section(SectionPos::new(1, WORLD_MIN_Y / 16, 0)).is_none());
    }
}

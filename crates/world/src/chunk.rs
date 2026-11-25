//! Chunk columns: 24 sections stacked from `WORLD_MIN_Y` to `WORLD_MAX_Y`.

use ob_content::{BlockId, Registry};
use ob_math::{BlockPos, ChunkPos, LocalPos, N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};

use crate::light::{LightSample, unpack_sky};
use crate::section::Section;

/// RGB block-light levels at one cell.
type BlockLevels = [u8; 3];

/// 16-bucket block-light BFS queue: `(x, absolute y, z, levels)`.
type BlockQueue = [Vec<(u8, u16, u8, BlockLevels)>; 16];

/// A vertical column of sections at one XZ chunk coordinate.
#[derive(Debug, Clone)]
pub struct ChunkColumn {
    pos: ChunkPos,
    sections: Vec<Section>,
    /// Highest non-air block Y per XZ column, relative to `WORLD_MIN_Y`.
    heightmap: Box<[i16; 256]>,
    /// Whether this column's light was seeded by the current engine.
    ///
    /// `false` for a freshly constructed column (tests, hand-built data), so
    /// [`crate::World::insert_chunk`] seeds it once. Generation and the save
    /// loader set it when they hand over authoritative light.
    lit: bool,
}

impl ChunkColumn {
    /// Creates an empty column.
    pub fn new(pos: ChunkPos) -> Self {
        Self {
            pos,
            sections: (0..N_SECTIONS).map(|_| Section::empty()).collect(),
            heightmap: Box::new([-1i16; 256]),
            lit: false,
        }
    }

    /// Rebuilds a column from persisted sections, recomputing the heightmap.
    pub fn from_sections(pos: ChunkPos, sections: Vec<Section>) -> Self {
        assert_eq!(sections.len(), N_SECTIONS, "column must have N_SECTIONS sections");
        let mut column = Self {
            pos,
            sections,
            heightmap: Box::new([-1i16; 256]),
            lit: true,
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

    /// Whether the column's light was seeded by the current engine.
    pub fn is_lit(&self) -> bool {
        self.lit
    }

    /// Marks the stored light as stale so the world reseeds it on insertion.
    pub fn mark_unlit(&mut self) {
        self.lit = false;
    }

    /// Seeds the column's own light after generation (or for hand-built data).
    ///
    /// Sky light is a top-down running subtraction over every XZ column, so an
    /// opaque roof zeroes every cell below it and a cave is genuinely dark.
    /// Block-light emitters are then seeded and flood-filled inside the column.
    /// Cross-chunk propagation happens in [`crate::World::insert_chunk`], which
    /// reconciles the border with every resident neighbour.
    pub fn finalise(&mut self, registry: &Registry) {
        self.seed_sky_columns(registry);
        self.seed_block_light_local(registry);
        self.lit = true;
    }

    /// Seeds sky light top-down for every XZ column.
    ///
    /// The running level starts at 15 above the column and subtracts each
    /// cell's `BlockType::sky_attenuation`. The loop stops
    /// reading blocks once the level reaches zero: no cell below an opaque
    /// block can regain sky light from above, so the rest of the column is
    /// written as zero without touching storage again.
    fn seed_sky_columns(&mut self, registry: &Registry) {
        for z in 0..SECTION_SIZE {
            for x in 0..SECTION_SIZE {
                let mut level = 15u8;
                let mut first_dark_section: Option<usize> = None;
                'sections: for section_index in (0..N_SECTIONS).rev() {
                    if level == 0 {
                        // This whole section and the lower ones are dark for
                        // this column. Fully opaque sections can be collapsed in
                        // one write; any section with a transparent block must
                        // be cleared column by column so another column's shaft
                        // in the same section is not erased.
                        first_dark_section = Some(section_index);
                        break 'sections;
                    }
                    let section = &mut self.sections[section_index];
                    for y in (0..SECTION_SIZE).rev() {
                        let pos = LocalPos {
                            x: x as u8,
                            y: y as u8,
                            z: z as u8,
                        };
                        let block = registry.block(section.get(pos));
                        level = level.saturating_sub(block.sky_attenuation);
                        if level == 0 {
                            section.light_mut().set_sky(pos, 0);
                            // This section was cleared in part above; start the
                            // bulk clear at the section below it.
                            first_dark_section = section_index.checked_sub(1);
                            // Clear the rest of this column in this section.
                            for yy in (0..y).rev() {
                                section.light_mut().set_sky(
                                    LocalPos {
                                        x: x as u8,
                                        y: yy as u8,
                                        z: z as u8,
                                    },
                                    0,
                                );
                            }
                            break;
                        }
                        section.light_mut().set_sky(pos, level);
                    }
                }
                if let Some(highest_dark) = first_dark_section {
                    for lower in (0..=highest_dark).rev() {
                        let section = &mut self.sections[lower];
                        if section.sky_is_opaque(registry) {
                            section.reset_light_uniform_sky(0);
                        } else {
                            for y in 0..SECTION_SIZE {
                                section.light_mut().set_sky(
                                    LocalPos {
                                        x: x as u8,
                                        y: y as u8,
                                        z: z as u8,
                                    },
                                    0,
                                );
                            }
                        }
                    }
                }
            }
        }
        // Lateral sky propagation inside the column: a one-block shaft must
        // light the cave beside it, and an overhang must shade the ground below
        // its lip. Uniform 15/0 sections have no internal lower neighbour and
        // are skipped; the world reconciles chunk borders after insertion.
        self.propagate_sky_local(registry);
        for section in &mut self.sections {
            section.light_mut().compact();
        }
    }

    /// Seeds block light from emitters in this column and flood-fills it.
    fn seed_block_light_local(&mut self, registry: &Registry) {
        for section in &mut self.sections {
            section.clear_block_light();
        }
        let mut buckets: BlockQueue = std::array::from_fn(|_| Vec::new());
        for section_index in 0..self.sections.len() {
            if self.sections[section_index].is_empty() || !self.sections[section_index].may_have_emitters(registry) {
                continue;
            }
            for index in 0..crate::section::BLOCK_COUNT {
                let pos = LocalPos::from_index(index);
                let emission = registry.block(self.sections[section_index].get(pos)).emission;
                if emission == [0, 0, 0] {
                    continue;
                }
                let absolute_y = section_index * SECTION_SIZE + pos.y as usize;
                let section = &mut self.sections[section_index];
                let word = section.light().get(pos);
                section.light_mut().set(
                    pos,
                    crate::light::with_block(word, emission[0], emission[1], emission[2]),
                );
                let bucket = *emission.iter().max().unwrap() as usize;
                buckets[bucket].push((pos.x, absolute_y as u16, pos.z, emission));
            }
        }
        self.propagate_block_local(registry, buckets);
    }

    /// Writes one packed word at a section-local Y in the column.
    fn set_sky_at(&mut self, x: u8, absolute_y: usize, z: u8, level: u8) {
        let section = &mut self.sections[absolute_y / SECTION_SIZE];
        let pos = LocalPos {
            x,
            y: (absolute_y % SECTION_SIZE) as u8,
            z,
        };
        section.light_mut().set_sky(pos, level);
    }

    fn sky_at(&self, x: u8, absolute_y: usize, z: u8) -> u8 {
        let section = &self.sections[absolute_y / SECTION_SIZE];
        let pos = LocalPos {
            x,
            y: (absolute_y % SECTION_SIZE) as u8,
            z,
        };
        section.light().sky(pos)
    }

    fn block_word_at(&self, x: u8, absolute_y: usize, z: u8) -> u16 {
        let section = &self.sections[absolute_y / SECTION_SIZE];
        let pos = LocalPos {
            x,
            y: (absolute_y % SECTION_SIZE) as u8,
            z,
        };
        section.light().get(pos)
    }

    fn set_block_word_at(&mut self, x: u8, absolute_y: usize, z: u8, word: u16) {
        let section = &mut self.sections[absolute_y / SECTION_SIZE];
        let pos = LocalPos {
            x,
            y: (absolute_y % SECTION_SIZE) as u8,
            z,
        };
        section.light_mut().set(pos, word);
    }

    fn block_at(&self, x: u8, absolute_y: usize, z: u8) -> BlockId {
        let section = &self.sections[absolute_y / SECTION_SIZE];
        section.get(LocalPos {
            x,
            y: (absolute_y % SECTION_SIZE) as u8,
            z,
        })
    }

    /// Lateral 6-neighbour sky flood fill inside one column.
    fn propagate_sky_local(&mut self, registry: &Registry) {
        let max_y = N_SECTIONS * SECTION_SIZE;
        let mut buckets: [Vec<(u8, u16, u8, u8)>; 16] = std::array::from_fn(|_| Vec::new());
        for (section_index, section) in self.sections.iter().enumerate() {
            if let Some(uniform) = section.light().uniform_value()
                && (unpack_sky(uniform) == 15 || unpack_sky(uniform) == 0)
            {
                continue;
            }
            for index in 0..crate::section::BLOCK_COUNT {
                let pos = LocalPos::from_index(index);
                let sky = section.light().sky(pos);
                if sky < 2 {
                    continue;
                }
                let absolute_y = section_index * SECTION_SIZE + pos.y as usize;
                let mut lower = false;
                let neighbours = [
                    (pos.x.wrapping_sub(1), absolute_y, pos.z),
                    (pos.x.wrapping_add(1), absolute_y, pos.z),
                    (pos.x, absolute_y.wrapping_sub(1), pos.z),
                    (pos.x, absolute_y + 1, pos.z),
                    (pos.x, absolute_y, pos.z.wrapping_sub(1)),
                    (pos.x, absolute_y, pos.z.wrapping_add(1)),
                ];
                for (nx, ny, nz) in neighbours {
                    if nx >= SECTION_SIZE as u8 || ny >= max_y || nz >= SECTION_SIZE as u8 {
                        continue;
                    }
                    if self.sky_at(nx, ny, nz) + 1 < sky {
                        lower = true;
                        break;
                    }
                }
                if lower {
                    buckets[sky as usize].push((pos.x, absolute_y as u16, pos.z, sky));
                }
            }
        }
        let mut queue = buckets;
        for level in (1..16u8).rev() {
            while let Some((x, y, z, queued_level)) = queue[level as usize].pop() {
                let y = y as usize;
                let current = self.sky_at(x, y, z);
                if queued_level < current {
                    continue;
                }
                for (nx, ny, nz) in neighbours_in_column(x, y, z) {
                    let cost = registry.block(self.block_at(nx, ny, nz)).sky_attenuation.max(1);
                    let next = queued_level.saturating_sub(cost);
                    if next == 0 || next <= self.sky_at(nx, ny, nz) {
                        continue;
                    }
                    self.set_sky_at(nx, ny, nz, next);
                    queue[next as usize].push((nx, ny as u16, nz, next));
                }
            }
        }
    }

    /// Flood-fills RGB block light inside one column from the given buckets.
    fn propagate_block_local(&mut self, registry: &Registry, mut queue: BlockQueue) {
        for level in (1..16u8).rev() {
            while let Some((x, y, z, levels)) = queue[level as usize].pop() {
                let y = y as usize;
                for (nx, ny, nz) in neighbours_in_column(x, y, z) {
                    let block = registry.block(self.block_at(nx, ny, nz));
                    let mut next = [0u8; 3];
                    let mut brightest = 0u8;
                    for channel in 0..3 {
                        let cost = block.attenuation[channel].max(1);
                        next[channel] = levels[channel].saturating_sub(cost);
                        brightest = brightest.max(next[channel]);
                    }
                    if brightest == 0 {
                        continue;
                    }
                    let current = LightSample::from_word(self.block_word_at(nx, ny, nz));
                    let (cr, cg, cb) = (current.red, current.green, current.blue);
                    if next[0] <= cr && next[1] <= cg && next[2] <= cb {
                        continue;
                    }
                    let word = crate::light::with_block(
                        self.block_word_at(nx, ny, nz),
                        cr.max(next[0]),
                        cg.max(next[1]),
                        cb.max(next[2]),
                    );
                    self.set_block_word_at(nx, ny, nz, word);
                    let levels = [cr.max(next[0]), cg.max(next[1]), cb.max(next[2])];
                    queue[brightest as usize].push((nx, ny as u16, nz, levels));
                }
            }
        }
    }

    /// Packed light word at one world-space position inside this column.
    pub(crate) fn light_word_at(&self, pos: BlockPos) -> u16 {
        self.section(pos.section())
            .map_or(0, |section| section.light().get(pos.local()))
    }

    /// Approximate heap bytes used by the column.
    pub fn heap_bytes(&self) -> usize {
        self.sections
            .iter()
            .map(|s| s.block_bytes() + s.light().heap_bytes())
            .sum()
    }
}

/// The in-column 6-neighbour positions of `(x, y, z)`.
fn neighbours_in_column(x: u8, y: usize, z: u8) -> impl Iterator<Item = (u8, usize, u8)> {
    let max_y = N_SECTIONS * SECTION_SIZE;
    let mut out = [(0u8, 0usize, 0u8); 6];
    let mut count = 0;
    let candidates = [
        (x.wrapping_sub(1), y, z),
        (x.wrapping_add(1), y, z),
        (x, y.wrapping_sub(1), z),
        (x, y + 1, z),
        (x, y, z.wrapping_sub(1)),
        (x, y, z.wrapping_add(1)),
    ];
    for (nx, ny, nz) in candidates {
        if nx < SECTION_SIZE as u8 && ny < max_y && nz < SECTION_SIZE as u8 {
            out[count] = (nx, ny, nz);
            count += 1;
        }
    }
    out.into_iter().take(count)
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

//! Deterministic terrain generation shared by client and dedicated server.
//!
//! Generation is a pure function of the world seed and the chunk position: no
//! thread-local state, wall-clock time or hash-map order participates. All
//! terrain maths uses 32-fractional-bit `i64` fixed point and integer lattice
//! hashes, so a chunk cannot drift between platforms, target features or
//! worker counts (REQ-GEN-002, DEC-046).
//!
//! The height field combines four-octave value-noise FBM (cell sizes 64, 32,
//! 16 and 8 blocks), a two-octave continental term (512 and 256 blocks), a
//! low-frequency mountain mask with a sharp ridged-noise term, and a terraced
//! plateau term. Columns are filled with stone, dirt and a grass, sand,
//! gravel, clay, stone or snow surface, water up to [`ob_math::SEA_LEVEL`],
//! sparse deterministic trees and occasional boulders. The finished
//! [`ChunkColumn`] is finalised for sky light and has a consistent heightmap.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // TODO(MS-04): re-enable once the v0.0.1 API surface settles

use ob_content::{BlockId, Registry};
use ob_math::{
    BlockPos, CHUNK_SIZE, ChunkPos, LocalPos, N_SECTIONS, Rng, SEA_LEVEL, SECTION_SIZE, SectionPos, WORLD_MAX_Y,
    WORLD_MIN_Y, derive_seed, mix64,
};
use ob_world::ChunkColumn;

/// One unit in the 32-fractional-bit fixed-point representation.
const FX_ONE: i64 = 1 << 32;

/// Half a fixed-point unit, used for rounding heights to whole blocks.
const FX_HALF: i64 = 1 << 31;

/// Base terrain height before the hills, continental and mountain terms.
const BASE_HEIGHT: i32 = 68;

/// Peak amplitude of the rolling-hills term, in blocks.
const HILL_AMPLITUDE: i32 = 18;

/// Peak amplitude of the large-scale continental term, in blocks.
const CONTINENT_AMPLITUDE: i32 = 12;

/// Peak amplitude of the ridged mountain term, in blocks.
const MOUNTAIN_AMPLITUDE: i32 = 52;

/// Lowest terrain height the generator clamps to, in blocks.
const MIN_HEIGHT: i32 = 40;

/// Highest terrain height the generator clamps to, in blocks.
const MAX_HEIGHT: i32 = 140;

/// Surface height at and above which the top block is snow.
const SNOW_LINE: i32 = 100;

/// Height difference to a neighbouring column that counts as a steep cliff.
const STEEP_SLOPE: i32 = 3;

/// Terraced plateaus quantise the lowland height field to this many blocks.
const PLATEAU_STEP: i32 = 4;

/// Salt for the rolling-hills noise stream.
const SALT_HILLS: i64 = 0x4849_4C4C; // "HILL"

/// Salt for the continental noise stream.
const SALT_CONTINENT: i64 = 0x434F_4E54; // "CONT"

/// Salt for the ridged mountain noise stream.
const SALT_RIDGE: i64 = 0x5249_4447; // "RIDG"

/// Salt for the low-frequency mountain mask.
const SALT_MOUNTAIN: i64 = 0x4D4F_554E; // "MOUN"

/// Salt for the plateau mask.
const SALT_PLATEAU: i64 = 0x504C_4154; // "PLAT"

/// Salt for the shallow-water surface patch noise.
const SALT_SHALLOW: i64 = 0x5348_414C; // "SHAL"

/// Salt for the per-column tree-placement hash.
const SALT_TREE: i64 = 0x5452_4545; // "TREE"

/// Salt for the per-column boulder-placement hash.
const SALT_BOULDER: i64 = 0x424F_554C; // "BOUL"

/// Expected number of tree candidates per world column (one in eighty).
const TREE_RARITY: u64 = 80;

/// Expected number of boulder candidates per world column (one in 320).
const BOULDER_RARITY: u64 = 320;

/// Horizontal canopy radius in blocks; trees keep this margin from chunk edges.
const TREE_MARGIN: i32 = 2;

/// Horizontal boulder radius in blocks; boulders keep this margin.
const BOULDER_MARGIN: i32 = 1;

/// Edge length of the per-chunk height grid, including the one-block border.
const GRID_SIZE: usize = CHUNK_SIZE as usize + 2;

/// Terrain heights for one chunk plus a one-block border (grid index 0 is
/// local `-1`, index 17 is local `16`).
///
/// Computing this once per chunk makes slope and beach decisions O(1) per
/// column instead of rescanning neighbouring columns.
type HeightGrid = [[i32; GRID_SIZE]; GRID_SIZE];

/// The block ids a generated column is built from.
struct TerrainBlocks {
    stone: BlockId,
    dirt: BlockId,
    grass: BlockId,
    sand: BlockId,
    water: BlockId,
    log: BlockId,
    leaves: BlockId,
    bedrock: BlockId,
    gravel: BlockId,
    clay: BlockId,
    snow: BlockId,
}

impl TerrainBlocks {
    /// Resolves the base block palette from a registry, degrading to air when a
    /// block type is absent.
    fn from_registry(registry: &Registry) -> Self {
        Self {
            stone: registry.id_or_air("openblock:stone"),
            dirt: registry.id_or_air("openblock:dirt"),
            grass: registry.id_or_air("openblock:grass"),
            sand: registry.id_or_air("openblock:sand"),
            water: registry.id_or_air("openblock:water"),
            log: registry.id_or_air("openblock:log"),
            leaves: registry.id_or_air("openblock:leaves"),
            bedrock: registry.id_or_air("openblock:bedrock"),
            gravel: registry.id_or_air("openblock:gravel"),
            clay: registry.id_or_air("openblock:clay"),
            snow: registry.id_or_air("openblock:snow"),
        }
    }
}

/// The top and soil block chosen for one generated column.
#[derive(Clone, Copy)]
struct ColumnSurface {
    /// Block at the terrain surface height.
    top: BlockId,
    /// Block for the three layers directly below the surface.
    filler: BlockId,
}

impl ColumnSurface {
    /// Placeholder for the array initialiser; never used for real terrain.
    const EMPTY: Self = Self {
        top: BlockId::AIR,
        filler: BlockId::AIR,
    };

    /// One material all the way down from the surface.
    const fn of(block: BlockId) -> Self {
        Self {
            top: block,
            filler: block,
        }
    }
}

/// A deterministic terrain generator for one world seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generator {
    /// The world seed every noise field and tree hash is derived from.
    pub seed: u64,
}

impl Generator {
    /// Creates a generator for a world seed.
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Generates one finished chunk column at `pos`.
    ///
    /// The result contains terrain, sea water, any trees whose full canopy
    /// fits inside the chunk, occasional boulders, with sky light seeded and
    /// the heightmap rebuilt.
    pub fn generate_chunk(&self, pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        let blocks = TerrainBlocks::from_registry(registry);
        let mut chunk = ChunkColumn::new(pos);
        let (origin_x, origin_z) = pos.origin();
        let heights = self.height_grid(origin_x, origin_z);

        self.fill_terrain(&mut chunk, &heights, &blocks);
        self.plant_trees(&mut chunk, &heights, &blocks);
        self.plant_boulders(&mut chunk, &heights, &blocks);

        chunk.finalise(registry);
        chunk.recompute_heightmap();
        chunk
    }

    /// The generated terrain surface height at world-space block XZ.
    ///
    /// This is the Y of the topmost terrain block (the grass, sand, gravel,
    /// clay, stone or snow layer), independent of trees, boulders and water.
    /// It is the same integer height field used by [`Self::generate_chunk`].
    pub fn height_at(&self, x: i32, z: i32) -> i32 {
        terrain_height(self.seed, x, z)
    }

    /// Computes the height grid for a chunk including its one-block border.
    fn height_grid(&self, origin_x: i32, origin_z: i32) -> HeightGrid {
        let mut grid = [[0i32; GRID_SIZE]; GRID_SIZE];
        for (grid_z, row) in grid.iter_mut().enumerate() {
            let world_z = origin_z + grid_z as i32 - 1;
            for (grid_x, height) in row.iter_mut().enumerate() {
                let world_x = origin_x + grid_x as i32 - 1;
                *height = terrain_height(self.seed, world_x, world_z);
            }
        }
        grid
    }

    /// Writes the terrain layers and sea water for every column.
    ///
    /// Blocks are written directly through each section's storage rather than
    /// through [`ChunkColumn::set_block`]; the heightmap is rebuilt once at the
    /// end of generation, so per-block heightmap maintenance would be wasted
    /// work.
    fn fill_terrain(&self, chunk: &mut ChunkColumn, heights: &HeightGrid, blocks: &TerrainBlocks) {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut surfaces = [ColumnSurface::EMPTY; CHUNK_SIZE as usize * CHUNK_SIZE as usize];
        let mut max_top = SEA_LEVEL;

        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let height = grid_at(heights, x, z);
                let steep = is_steep(heights, x, z);
                let surface = self.column_surface(origin_x + x, origin_z + z, height, steep, blocks);
                surfaces[(z * CHUNK_SIZE + x) as usize] = surface;
                max_top = max_top.max(height);
            }
        }

        let section_size = SECTION_SIZE as i32;
        let first_section_y = WORLD_MIN_Y / section_size;
        let last_section_offset = ((max_top - WORLD_MIN_Y) / section_size).min(N_SECTIONS as i32 - 1);

        for offset in 0..=last_section_offset {
            let section_y = first_section_y + offset;
            let base_y = section_y * section_size;
            let Some(section) = chunk.section_mut(SectionPos::new(
                origin_x / section_size,
                section_y,
                origin_z / section_size,
            )) else {
                continue;
            };

            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let height = grid_at(heights, x, z);
                    let top = height.max(SEA_LEVEL);
                    if base_y > top {
                        continue;
                    }
                    let local = LocalPos {
                        x: x as u8,
                        y: 0,
                        z: z as u8,
                    };
                    let surface = surfaces[(z * CHUNK_SIZE + x) as usize];
                    for local_y in 0..section_size {
                        let y = base_y + local_y;
                        if y > top {
                            break;
                        }
                        let block = column_block(y, height, surface, blocks);
                        if block != BlockId::AIR {
                            section.set(
                                LocalPos {
                                    y: local_y as u8,
                                    ..local
                                },
                                block,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Chooses the surface and soil material for one column.
    ///
    /// Precedence is snow on high peaks, stone on steep high slopes, grass on
    /// the remaining dry uplands, sand on dry beaches, then clay/gravel patches
    /// or sand on the shallow bed, and sand in deeper water. Beach columns above
    /// the waterline always keep a sand top so the shoreline reads as a
    /// continuous beach.
    fn column_surface(
        &self,
        world_x: i32,
        world_z: i32,
        height: i32,
        steep: bool,
        blocks: &TerrainBlocks,
    ) -> ColumnSurface {
        if height >= SNOW_LINE {
            return ColumnSurface {
                top: blocks.snow,
                filler: blocks.stone,
            };
        }
        if height >= SEA_LEVEL + 3 {
            return if steep {
                ColumnSurface::of(blocks.stone)
            } else {
                ColumnSurface {
                    top: blocks.grass,
                    filler: blocks.dirt,
                }
            };
        }
        if height >= SEA_LEVEL {
            return ColumnSurface::of(blocks.sand);
        }
        if SEA_LEVEL - height <= 3 {
            return ColumnSurface::of(self.shallow_surface(world_x, world_z, blocks));
        }
        ColumnSurface::of(blocks.sand)
    }

    /// Picks a shallow-water floor block from a smooth patch noise field.
    ///
    /// Clay and gravel form contiguous patches rather than per-column noise;
    /// about a third of the shallow bed is a patch and the rest stays sand.
    fn shallow_surface(&self, world_x: i32, world_z: i32, blocks: &TerrainBlocks) -> BlockId {
        let patch = value_noise(self.seed, world_x, world_z, 4, SALT_SHALLOW);
        if patch > FX_ONE / 3 {
            blocks.clay
        } else if patch < -(FX_ONE / 6) {
            blocks.gravel
        } else {
            blocks.sand
        }
    }

    /// Plants sparse deterministic trees on dry grass.
    ///
    /// Every candidate is hashed from the world column, so the decision does not
    /// depend on which chunk is generated first. A tree is planted only when all
    /// of its trunk and canopy blocks are currently air and inside this chunk,
    /// which also guarantees that it never overwrites water or crosses a border.
    fn plant_trees(&self, chunk: &mut ChunkColumn, heights: &HeightGrid, blocks: &TerrainBlocks) {
        if blocks.log == BlockId::AIR || blocks.leaves == BlockId::AIR {
            return;
        }
        let (origin_x, origin_z) = chunk.pos().origin();
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                if !(TREE_MARGIN..CHUNK_SIZE - TREE_MARGIN).contains(&x)
                    || !(TREE_MARGIN..CHUNK_SIZE - TREE_MARGIN).contains(&z)
                {
                    continue;
                }
                let surface = grid_at(heights, x, z);
                if surface <= SEA_LEVEL + 2 {
                    continue;
                }
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_TREE);
                if !hash.is_multiple_of(TREE_RARITY) {
                    continue;
                }
                if chunk.get_block(BlockPos::new(world_x, surface, world_z)) != blocks.grass {
                    continue;
                }
                self.plant_tree(chunk, x, z, surface, hash, blocks);
            }
        }
    }

    /// Writes one tree, or does nothing when any planned block is not air.
    ///
    /// The trunk is 4–6 logs. The canopy has up to three layers, with the two
    /// lowest at radius 2 and the top at radius 1, always starting at or above
    /// `surface + 3`, plus a five-block crown above the top log. Every leaf is
    /// therefore within two blocks of the trunk column and touches the trunk or
    /// another leaf.
    fn plant_tree(
        &self,
        chunk: &mut ChunkColumn,
        local_x: i32,
        local_z: i32,
        surface: i32,
        hash: u64,
        blocks: &TerrainBlocks,
    ) {
        let mut rng = Rng::new(mix64(hash));
        let trunk_height = 4 + rng.next_below(3) as i32;
        let base_y = surface + 1;
        let top_y = base_y + trunk_height - 1;

        let mut planned: Vec<(i32, i32, i32, BlockId)> = Vec::with_capacity(80);
        for trunk in 0..trunk_height {
            planned.push((local_x, base_y + trunk, local_z, blocks.log));
        }

        // Up to three canopy layers tapering from radius 2 to radius 1. A
        // four-block trunk cannot fit its lowest layer at `top_y - 2` without
        // dropping below the surface + 3 rule, so clamp the bottom and taper
        // from there.
        let canopy_bottom = (top_y - 2).max(surface + 3);
        for y in canopy_bottom..=top_y {
            let radius = if y == top_y { 1 } else { 2 };
            for dz in -radius..=radius {
                for dx in -radius..=radius {
                    if dx == 0 && dz == 0 {
                        continue; // the trunk occupies this cell
                    }
                    planned.push((local_x + dx, y, local_z + dz, blocks.leaves));
                }
            }
        }

        // Crown directly above the top log, including the trunk column itself,
        // so the canopy always connects to the top of the trunk.
        for (dx, dz) in [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            planned.push((local_x + dx, top_y + 1, local_z + dz, blocks.leaves));
        }

        let (origin_x, origin_z) = chunk.pos().origin();
        for &(x, y, z, _) in &planned {
            if !(0..CHUNK_SIZE).contains(&x) || !(0..CHUNK_SIZE).contains(&z) {
                return;
            }
            if y <= WORLD_MIN_Y || y >= WORLD_MAX_Y {
                return;
            }
            if chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z)) != BlockId::AIR {
                return;
            }
        }
        for &(x, y, z, block) in &planned {
            chunk.set_block(BlockPos::new(origin_x + x, y, origin_z + z), block);
        }
    }

    /// Plants occasional plus-shaped boulders on flat grass.
    ///
    /// All five blocks sit directly on grass at the same height, so a boulder
    /// is always supported and never floats over a slope.
    fn plant_boulders(&self, chunk: &mut ChunkColumn, heights: &HeightGrid, blocks: &TerrainBlocks) {
        if blocks.stone == BlockId::AIR && blocks.gravel == BlockId::AIR {
            return;
        }
        let (origin_x, origin_z) = chunk.pos().origin();
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                if !(BOULDER_MARGIN..CHUNK_SIZE - BOULDER_MARGIN).contains(&x)
                    || !(BOULDER_MARGIN..CHUNK_SIZE - BOULDER_MARGIN).contains(&z)
                {
                    continue;
                }
                let surface = grid_at(heights, x, z);
                if surface <= SEA_LEVEL + 2 {
                    continue;
                }
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_BOULDER);
                if !hash.is_multiple_of(BOULDER_RARITY) {
                    continue;
                }

                let offsets = [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)];
                let mut supported = true;
                for &(dx, dz) in &offsets {
                    if grid_at(heights, x + dx, z + dz) != surface
                        || chunk.get_block(BlockPos::new(world_x + dx, surface, world_z + dz)) != blocks.grass
                        || chunk.get_block(BlockPos::new(world_x + dx, surface + 1, world_z + dz)) != BlockId::AIR
                    {
                        supported = false;
                        break;
                    }
                }
                if !supported {
                    continue;
                }

                let boulder = if blocks.gravel == BlockId::AIR {
                    blocks.stone
                } else if blocks.stone == BlockId::AIR {
                    blocks.gravel
                } else if hash & 1 == 0 {
                    blocks.stone
                } else {
                    blocks.gravel
                };
                for &(dx, dz) in &offsets {
                    chunk.set_block(BlockPos::new(world_x + dx, surface + 1, world_z + dz), boulder);
                }
            }
        }
    }
}

/// Reads a local column from the padded height grid (`x`/`z` in `-1..=16`).
fn grid_at(heights: &HeightGrid, x: i32, z: i32) -> i32 {
    heights[(z + 1) as usize][(x + 1) as usize]
}

/// Whether any of the eight neighbouring columns differs by more than
/// [`STEEP_SLOPE`] blocks.
fn is_steep(heights: &HeightGrid, x: i32, z: i32) -> bool {
    let height = grid_at(heights, x, z);
    for dz in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            if (grid_at(heights, x + dx, z + dz) - height).abs() > STEEP_SLOPE {
                return true;
            }
        }
    }
    false
}

/// The block at one world Y for a column whose terrain surface is `height`.
///
/// Water is only produced at or below [`SEA_LEVEL`]; above the surface and the
/// sea the column is air.
fn column_block(y: i32, height: i32, surface: ColumnSurface, blocks: &TerrainBlocks) -> BlockId {
    if y == WORLD_MIN_Y {
        blocks.bedrock
    } else if y > height {
        if y <= SEA_LEVEL { blocks.water } else { BlockId::AIR }
    } else if y == height {
        surface.top
    } else if y > height - 4 {
        surface.filler
    } else {
        blocks.stone
    }
}

/// Multiplies two unit fixed-point values, truncating toward zero.
fn fx_mul(a: i64, b: i64) -> i64 {
    ((i128::from(a) * i128::from(b)) >> 32) as i64
}

/// Linear interpolation between fixed-point values by the fixed-point `t`.
fn fx_lerp(a: i64, b: i64, t: i64) -> i64 {
    a + ((i128::from(b - a) * i128::from(t)) >> 32) as i64
}

/// Smoothstep for fixed-point `t` in `[0, 1]`, returning `3t² − 2t³`.
fn smoothstep(t: i64) -> i64 {
    let t_squared = fx_mul(t, t);
    fx_mul(t_squared, 3 * FX_ONE - 2 * t)
}

/// Hashes one lattice point into a fixed-point value in `[-FX_ONE, FX_ONE]`.
fn lattice_value(seed: u64, ix: i64, iz: i64, salt: i64) -> i64 {
    let hash = derive_seed(seed, ix, iz, salt);
    let signed = (hash >> 32) as u32 as i32;
    i64::from(signed) << 1
}

/// Bilinear smoothstep value noise at world XZ for a lattice cell size of
/// `2^shift` blocks.
fn value_noise(seed: u64, x: i32, z: i32, shift: u32, salt: i64) -> i64 {
    let cell = 1i32 << shift;
    let ix = (x >> shift) as i64;
    let iz = (z >> shift) as i64;
    let fx = i64::from(x & (cell - 1)) << (32 - shift);
    let fz = i64::from(z & (cell - 1)) << (32 - shift);
    let sx = smoothstep(fx);
    let sz = smoothstep(fz);

    let v00 = lattice_value(seed, ix, iz, salt);
    let v10 = lattice_value(seed, ix + 1, iz, salt);
    let v01 = lattice_value(seed, ix, iz + 1, salt);
    let v11 = lattice_value(seed, ix + 1, iz + 1, salt);
    let bottom = fx_lerp(v00, v10, sx);
    let top = fx_lerp(v01, v11, sx);
    fx_lerp(bottom, top, sz)
}

/// Dyadic value-noise FBM in `[-FX_ONE, FX_ONE]`.
///
/// Amplitudes halve each octave and the lattice cell doubles, so the weights
/// sum to `2^octaves − 1` and the integer division stays exact.
fn fbm(seed: u64, x: i32, z: i32, base_shift: u32, octaves: u32, salt: i64) -> i64 {
    let mut sum = 0i64;
    let mut total = 0i64;
    let mut amplitude = 1i64 << (octaves - 1);
    for octave in 0..octaves {
        sum += value_noise(seed, x, z, base_shift + octave, salt + i64::from(octave)) * amplitude;
        total += amplitude;
        amplitude >>= 1;
    }
    sum / total
}

/// The terrain surface height in world block coordinates for one XZ.
///
/// This is the single source of truth used by both [`Generator::height_at`] and
/// [`Generator::generate_chunk`]. The rolling hills are cells of 64, 32, 16 and
/// 8 blocks; the continental term uses 512 and 256 block cells. A low-frequency
/// mask gates a ridged-mountain term, while a separate plateau mask terraces
/// some lowlands into flat, quantised benches.
fn terrain_height(seed: u64, x: i32, z: i32) -> i32 {
    let hills = fbm(seed, x, z, 6, 4, SALT_HILLS);
    let continent = fbm(seed, x, z, 9, 2, SALT_CONTINENT);
    let base = i64::from(BASE_HEIGHT) * FX_ONE
        + fx_mul(hills, i64::from(HILL_AMPLITUDE) * FX_ONE)
        + fx_mul(continent, i64::from(CONTINENT_AMPLITUDE) * FX_ONE);

    // Flatten selected regions onto four-block benches. The gate keeps the
    // terraces occasional, and the smoothstep narrows the transition into a
    // short cliff band rather than smearing the step over the whole region.
    let plateau_noise = fbm(seed, x, z, 7, 2, SALT_PLATEAU);
    let plateau_gate = fx_mul(plateau_noise - FX_ONE / 4, 4 * FX_ONE).clamp(0, FX_ONE);
    let plateau_factor = smoothstep(plateau_gate);
    let step = i64::from(PLATEAU_STEP) * FX_ONE;
    let terrace = ((base + step / 2).div_euclid(step)) * step;
    let shaped = fx_lerp(base, terrace, plateau_factor);

    // Ridged mountains: the ridge term is sharp near a zero crossing of its
    // noise field, and the mask confines it to broad mountain regions.
    let ridge_noise = fbm(seed, x, z, 6, 3, SALT_RIDGE);
    let ridge = FX_ONE - ridge_noise.abs();
    let ridge = fx_mul(ridge, ridge);
    let mountain_noise = fbm(seed, x, z, 9, 2, SALT_MOUNTAIN);
    let mountain_gate = fx_mul(mountain_noise, 2 * FX_ONE).clamp(0, FX_ONE);
    let mountain_factor = smoothstep(mountain_gate);
    let mountain = fx_mul(mountain_factor, ridge);

    let height = shaped + fx_mul(mountain, i64::from(MOUNTAIN_AMPLITUDE) * FX_ONE);
    let rounded = ((height + FX_HALF) >> 32) as i32;
    rounded.clamp(MIN_HEIGHT, MAX_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::CHUNK_SIZE;

    /// Flattens every block of a chunk column in Y-major, then Z, then X order.
    fn collect_blocks(chunk: &ChunkColumn) -> Vec<BlockId> {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut blocks = Vec::with_capacity((CHUNK_SIZE * CHUNK_SIZE * (WORLD_MAX_Y - WORLD_MIN_Y)) as usize);
        for y in WORLD_MIN_Y..WORLD_MAX_Y {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    blocks.push(chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z)));
                }
            }
        }
        blocks
    }

    /// Reads a world block, treating anything outside the chunk column as air.
    fn block_in_chunk(chunk: &ChunkColumn, x: i32, y: i32, z: i32) -> BlockId {
        let (origin_x, origin_z) = chunk.pos().origin();
        if !(origin_x..origin_x + CHUNK_SIZE).contains(&x)
            || !(origin_z..origin_z + CHUNK_SIZE).contains(&z)
            || !(WORLD_MIN_Y..WORLD_MAX_Y).contains(&y)
        {
            return BlockId::AIR;
        }
        chunk.get_block(BlockPos::new(x, y, z))
    }

    /// Whether the height field at a world column counts as a steep slope.
    fn world_is_steep(generator: &Generator, x: i32, z: i32) -> bool {
        let height = generator.height_at(x, z);
        for dz in -1..=1 {
            for dx in -1..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                if (generator.height_at(x + dx, z + dz) - height).abs() > STEEP_SLOPE {
                    return true;
                }
            }
        }
        false
    }

    /// Finds a chunk containing a column whose height satisfies `predicate`.
    fn find_chunk_with_height<F>(generator: &Generator, radius: i32, mut predicate: F) -> Option<ChunkPos>
    where
        F: FnMut(i32) -> bool,
    {
        for cz in -radius..=radius {
            for cx in -radius..=radius {
                let pos = ChunkPos::new(cx, cz);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        if predicate(generator.height_at(origin_x + x, origin_z + z)) {
                            return Some(pos);
                        }
                    }
                }
            }
        }
        None
    }

    #[test]
    fn generation_is_deterministic() {
        let registry = Registry::base();
        let seed = 0x5EED_1234_ABCD_0001;
        for pos in [ChunkPos::new(0, 0), ChunkPos::new(-3, 7), ChunkPos::new(12, -5)] {
            let first = Generator::new(seed).generate_chunk(pos, &registry);
            let second = Generator::new(seed).generate_chunk(pos, &registry);
            assert_eq!(
                collect_blocks(&first),
                collect_blocks(&second),
                "same seed and chunk must produce identical blocks"
            );
        }
    }

    #[test]
    fn different_seeds_generate_different_terrain() {
        let registry = Registry::base();
        let chunk_a = Generator::new(1).generate_chunk(ChunkPos::new(0, 0), &registry);
        let chunk_b = Generator::new(2).generate_chunk(ChunkPos::new(0, 0), &registry);
        let blocks_a = collect_blocks(&chunk_a);
        let blocks_b = collect_blocks(&chunk_b);
        let differing = blocks_a.iter().zip(&blocks_b).filter(|(a, b)| a != b).count();
        assert!(differing > 100, "only {differing} of {} blocks differ", blocks_a.len());

        let generator_a = Generator::new(1);
        let generator_b = Generator::new(2);
        let mut height_differences = 0;
        for z in -16..16 {
            for x in -16..16 {
                if generator_a.height_at(x, z) != generator_b.height_at(x, z) {
                    height_differences += 1;
                }
            }
        }
        assert!(
            height_differences > 256,
            "only {height_differences} of 1024 height samples differ"
        );
    }

    #[test]
    fn height_at_matches_the_highest_solid_block() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        let generator = Generator::new(42);
        let mut checked = 0;

        for pos in [ChunkPos::new(0, 0), ChunkPos::new(1, -1)] {
            let chunk = generator.generate_chunk(pos, &registry);
            let (origin_x, origin_z) = pos.origin();
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    let surface = generator.height_at(world_x, world_z);

                    // Skip decorated columns: trees and boulders put blocks
                    // above the terrain surface, which is what this test is
                    // trying to avoid comparing against.
                    let decorated = (surface + 1..WORLD_MAX_Y).any(|y| {
                        let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                        block != BlockId::AIR && block != water
                    });
                    if decorated {
                        continue;
                    }

                    let mut highest_terrain = None;
                    for y in (WORLD_MIN_Y..WORLD_MAX_Y).rev() {
                        let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                        if block != BlockId::AIR && block != water {
                            highest_terrain = Some(y);
                            break;
                        }
                    }
                    assert_eq!(
                        highest_terrain,
                        Some(surface),
                        "height_at disagrees at world XZ ({world_x}, {world_z})"
                    );
                    assert_ne!(chunk.get_block(BlockPos::new(world_x, surface, world_z)), BlockId::AIR);
                    checked += 1;
                }
            }
        }
        assert!(checked >= 64, "only {checked} decoration-free columns checked");
    }

    #[test]
    fn water_is_confined_to_sea_level_and_below() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        let generator = Generator::new(0xC0FF_EE00_1234_5678);

        // Find a deterministic ocean chunk so the water rule is exercised.
        let mut ocean_chunk = None;
        'search: for cz in -8..=8 {
            for cx in -8..=8 {
                let pos = ChunkPos::new(cx, cz);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        if generator.height_at(origin_x + x, origin_z + z) < SEA_LEVEL {
                            ocean_chunk = Some(pos);
                            break 'search;
                        }
                    }
                }
            }
        }
        let ocean_chunk = ocean_chunk.expect("height field must contain an ocean within 17 chunks");

        let mut saw_water = false;
        let mut saw_sea_surface = false;
        for pos in [ChunkPos::new(0, 0), ChunkPos::new(1, -1), ocean_chunk] {
            let chunk = generator.generate_chunk(pos, &registry);
            let (origin_x, origin_z) = pos.origin();
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    for y in WORLD_MIN_Y..WORLD_MAX_Y {
                        if chunk.get_block(BlockPos::new(world_x, y, world_z)) != water {
                            continue;
                        }
                        saw_water = true;
                        assert!(y <= SEA_LEVEL, "water at y={y} is above sea level {SEA_LEVEL}");
                        assert!(
                            generator.height_at(world_x, world_z) < SEA_LEVEL,
                            "water at world XZ ({world_x}, {world_z}) where terrain is not below sea level"
                        );
                        if y == SEA_LEVEL {
                            saw_sea_surface = true;
                        }
                    }
                }
            }
        }
        assert!(saw_water, "expected water in the ocean sample");
        assert!(saw_sea_surface, "expected a water column reaching sea level");
    }

    #[test]
    fn bedrock_lines_the_world_floor() {
        let registry = Registry::base();
        let bedrock = registry.id_or_air("openblock:bedrock");
        for seed in [0u64, 7, 123_456] {
            let generator = Generator::new(seed);
            for pos in [ChunkPos::new(0, 0), ChunkPos::new(-1, 2)] {
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        assert_eq!(
                            chunk.get_block(BlockPos::new(origin_x + x, WORLD_MIN_Y, origin_z + z)),
                            bedrock,
                            "missing bedrock at ({}, {WORLD_MIN_Y}, {})",
                            origin_x + x,
                            origin_z + z
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn trees_grow_on_dry_grass_and_never_replace_water() {
        let registry = Registry::base();
        let log = registry.id_or_air("openblock:log");
        let leaves = registry.id_or_air("openblock:leaves");
        let water = registry.id_or_air("openblock:water");
        let grass = registry.id_or_air("openblock:grass");

        let mut found = None;
        'search: for seed in 0..64u64 {
            let generator = Generator::new(seed);
            for pos in [
                ChunkPos::new(0, 0),
                ChunkPos::new(1, 0),
                ChunkPos::new(0, 1),
                ChunkPos::new(-1, 0),
                ChunkPos::new(0, -1),
            ] {
                let (origin_x, origin_z) = pos.origin();
                let has_candidate = (0..CHUNK_SIZE)
                    .any(|z| (0..CHUNK_SIZE).any(|x| generator.height_at(origin_x + x, origin_z + z) >= SEA_LEVEL + 3));
                if !has_candidate {
                    continue;
                }
                let chunk = generator.generate_chunk(pos, &registry);
                let mut tree_blocks = Vec::new();
                for y in WORLD_MIN_Y..WORLD_MAX_Y {
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let block = chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z));
                            if block == log || block == leaves {
                                tree_blocks.push(BlockPos::new(origin_x + x, y, origin_z + z));
                            }
                        }
                    }
                }
                if !tree_blocks.is_empty() {
                    found = Some((seed, pos, chunk, tree_blocks));
                    break 'search;
                }
            }
        }

        let (seed, pos, chunk, tree_blocks) = found.expect("expected at least one tree in the seed sample");
        let generator = Generator::new(seed);
        let (origin_x, origin_z) = pos.origin();
        assert!(!tree_blocks.is_empty(), "tree sample is empty");

        let mut log_blocks = 0;
        for block_pos in tree_blocks {
            let block = chunk.get_block(block_pos);
            assert!(
                block == log || block == leaves,
                "expected log or leaves at {block_pos:?}"
            );
            assert!(
                (origin_x..origin_x + CHUNK_SIZE).contains(&block_pos.x)
                    && (origin_z..origin_z + CHUNK_SIZE).contains(&block_pos.z),
                "tree block {block_pos:?} crosses the chunk border"
            );
            // Water never exists above sea level, so a tree block above the sea
            // cannot have replaced it. Spot-check the column above the block too.
            assert!(
                block_pos.y > SEA_LEVEL,
                "tree block {block_pos:?} is at or below sea level"
            );
            for y in block_pos.y..WORLD_MAX_Y {
                assert_ne!(
                    chunk.get_block(BlockPos::new(block_pos.x, y, block_pos.z)),
                    water,
                    "tree block {block_pos:?} is stacked on water"
                );
            }
            if block == log {
                log_blocks += 1;
                let surface = generator.height_at(block_pos.x, block_pos.z);
                assert!(
                    surface >= SEA_LEVEL + 3,
                    "tree trunk at {block_pos:?} has surface {surface}, below the grass band"
                );
                assert_eq!(
                    chunk.get_block(BlockPos::new(block_pos.x, surface, block_pos.z)),
                    grass,
                    "tree trunk at {block_pos:?} does not stand on grass"
                );
            }
        }
        assert!(log_blocks > 0, "found leaves without any logs");
    }

    #[test]
    fn snow_appears_only_on_high_surfaces() {
        let registry = Registry::base();
        let snow = registry.id_or_air("openblock:snow");
        let generator = Generator::new(1);
        let snowy = find_chunk_with_height(&generator, 8, |height| height >= SNOW_LINE)
            .expect("expected a snowy peak within 17 chunks of the origin");

        let mut snow_blocks = 0usize;
        for pos in [snowy, ChunkPos::new(0, 0), ChunkPos::new(snowy.x + 1, snowy.z)] {
            let chunk = generator.generate_chunk(pos, &registry);
            let (origin_x, origin_z) = pos.origin();
            for y in WORLD_MIN_Y..WORLD_MAX_Y {
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        if chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z)) != snow {
                            continue;
                        }
                        let height = generator.height_at(origin_x + x, origin_z + z);
                        assert!(
                            height >= SNOW_LINE,
                            "snow at surface {height} is below the snow line {SNOW_LINE}"
                        );
                        assert_eq!(y, height, "snow is not the top terrain block");
                        snow_blocks += 1;
                    }
                }
            }
        }
        assert!(snow_blocks > 0, "expected snow on the sampled peak");
    }

    #[test]
    fn beaches_near_sea_level_have_a_sand_top() {
        let registry = Registry::base();
        let sand = registry.id_or_air("openblock:sand");
        let generator = Generator::new(1);
        let pos = find_chunk_with_height(&generator, 12, |height| (SEA_LEVEL..=SEA_LEVEL + 2).contains(&height))
            .expect("expected a beach within 25 chunks of the origin");
        let chunk = generator.generate_chunk(pos, &registry);
        let (origin_x, origin_z) = pos.origin();

        let mut beach_columns = 0usize;
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let height = generator.height_at(world_x, world_z);
                if (height - SEA_LEVEL).abs() > 2 || height < SEA_LEVEL {
                    continue; // not a beach, or the column is under water
                }
                assert_eq!(
                    chunk.get_block(BlockPos::new(world_x, height, world_z)),
                    sand,
                    "beach column ({world_x}, {world_z}) at surface {height} is not sand"
                );
                beach_columns += 1;
            }
        }
        assert!(beach_columns > 0, "beach sample contained no dry beach columns");
    }

    #[test]
    fn surface_materials_follow_the_terrain_rules() {
        let registry = Registry::base();
        let grass = registry.id_or_air("openblock:grass");
        let sand = registry.id_or_air("openblock:sand");
        let gravel = registry.id_or_air("openblock:gravel");
        let clay = registry.id_or_air("openblock:clay");
        let snow = registry.id_or_air("openblock:snow");
        let stone = registry.id_or_air("openblock:stone");
        let water = registry.id_or_air("openblock:water");

        let lowland = Generator::new(0);
        let highland = Generator::new(1);
        let shallow = find_chunk_with_height(&lowland, 8, |height| (SEA_LEVEL - 3..SEA_LEVEL).contains(&height))
            .expect("expected shallow water within 17 chunks");
        let beach = find_chunk_with_height(&highland, 12, |height| (SEA_LEVEL..=SEA_LEVEL + 2).contains(&height))
            .expect("expected a beach within 25 chunks");
        let peak = find_chunk_with_height(&highland, 8, |height| height >= SNOW_LINE)
            .expect("expected a snowy peak within 17 chunks");

        let samples = [
            (0u64, ChunkPos::new(0, 0)),
            (0, ChunkPos::new(1, 1)),
            (0, shallow),
            (0, ChunkPos::new(2, 4)),
            (26, ChunkPos::new(0, 0)),
            (1, ChunkPos::new(0, 0)),
            (1, beach),
            (1, peak),
        ];

        let mut seen = [0usize; 6]; // grass, sand, gravel, clay, snow, stone
        for (seed, pos) in samples {
            let generator = Generator::new(seed);
            let chunk = generator.generate_chunk(pos, &registry);
            let (origin_x, origin_z) = pos.origin();
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    let height = generator.height_at(world_x, world_z);
                    let top = chunk.get_block(BlockPos::new(world_x, height, world_z));
                    let steep = world_is_steep(&generator, world_x, world_z);

                    assert_ne!(top, BlockId::AIR, "air at the terrain surface ({world_x}, {world_z})");
                    assert_ne!(top, water, "water at the terrain surface ({world_x}, {world_z})");

                    if height >= SNOW_LINE {
                        assert_eq!(top, snow, "high surface {height} is not snow");
                        seen[4] += 1;
                    } else if height >= SEA_LEVEL + 3 {
                        if steep {
                            assert_eq!(top, stone, "steep surface {height} is not stone");
                            seen[5] += 1;
                        } else {
                            assert_eq!(top, grass, "gentle surface {height} is not grass");
                            seen[0] += 1;
                        }
                    } else if height >= SEA_LEVEL {
                        assert_eq!(top, sand, "beach surface {height} is not sand");
                        seen[1] += 1;
                    } else {
                        assert!(
                            top == sand || top == gravel || top == clay,
                            "underwater surface {height} is neither sand, gravel nor clay"
                        );
                        if top == gravel {
                            assert!(
                                SEA_LEVEL - height <= 3,
                                "gravel at depth {} is deeper than 3",
                                SEA_LEVEL - height
                            );
                            seen[2] += 1;
                        } else if top == clay {
                            assert!(
                                SEA_LEVEL - height <= 3,
                                "clay at depth {} is deeper than 3",
                                SEA_LEVEL - height
                            );
                            seen[3] += 1;
                        } else {
                            seen[1] += 1;
                        }
                    }

                    // Cross-cutting rules: decorations never change these.
                    if top == gravel || top == clay {
                        assert!(height < SEA_LEVEL, "gravel or clay exposed above water");
                    }
                    if top == sand && height >= SEA_LEVEL {
                        assert!(height <= SEA_LEVEL + 2, "inland sand at surface {height}");
                    }
                    if top == stone {
                        assert!(steep, "stone at ({world_x}, {world_z}) is not on a steep slope");
                    }
                    if top == grass {
                        assert!(!steep, "grass at ({world_x}, {world_z}) is on a steep slope");
                    }
                }
            }
        }

        assert!(seen[0] > 0, "sample contained no grass");
        assert!(seen[1] > 0, "sample contained no sand");
        assert!(seen[2] > 0, "sample contained no gravel");
        assert!(seen[3] > 0, "sample contained no clay");
        assert!(seen[4] > 0, "sample contained no snow");
        assert!(seen[5] > 0, "sample contained no exposed stone");
    }

    #[test]
    fn tree_canopies_are_connected_to_their_trunks() {
        let registry = Registry::base();
        let log = registry.id_or_air("openblock:log");
        let leaves = registry.id_or_air("openblock:leaves");
        let grass = registry.id_or_air("openblock:grass");

        let mut found = None;
        'search: for seed in 0..64u64 {
            let generator = Generator::new(seed);
            for pos in [
                ChunkPos::new(0, 0),
                ChunkPos::new(1, 0),
                ChunkPos::new(0, 1),
                ChunkPos::new(-1, 0),
                ChunkPos::new(0, -1),
            ] {
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                let mut has_tree = false;
                for y in WORLD_MIN_Y..WORLD_MAX_Y {
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let block = chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z));
                            has_tree |= block == log || block == leaves;
                        }
                    }
                }
                if has_tree {
                    found = Some((generator, pos, chunk));
                    break 'search;
                }
            }
        }
        let (generator, pos, chunk) = found.expect("expected at least one tree in the seed sample");
        let (origin_x, origin_z) = pos.origin();

        let mut logs = Vec::new();
        let mut leaves_blocks = Vec::new();
        for y in WORLD_MIN_Y..WORLD_MAX_Y {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let block_pos = BlockPos::new(origin_x + x, y, origin_z + z);
                    let block = chunk.get_block(block_pos);
                    if block == log {
                        logs.push(block_pos);
                    } else if block == leaves {
                        leaves_blocks.push(block_pos);
                    }
                }
            }
        }
        assert!(!logs.is_empty(), "found leaves without any logs");
        assert!(!leaves_blocks.is_empty(), "found logs without any leaves");

        // No leaf may float: every leaf touches another leaf or a log in its
        // 26-neighbourhood.
        for &leaf in &leaves_blocks {
            let mut connected = false;
            'neighbours: for dy in -1..=1 {
                for dz in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        let neighbour = block_in_chunk(&chunk, leaf.x + dx, leaf.y + dy, leaf.z + dz);
                        if neighbour == log || neighbour == leaves {
                            connected = true;
                            break 'neighbours;
                        }
                    }
                }
            }
            assert!(connected, "floating leaves block at {leaf:?}");
            assert!(
                logs.iter()
                    .any(|log| (log.x - leaf.x).abs() <= 2 && (log.z - leaf.z).abs() <= 2),
                "leaves block at {leaf:?} is more than two blocks from a trunk"
            );
        }

        // Every log is part of a 4–6 block trunk standing on grass, and has a
        // canopy above it within the tree's two-block radius.
        for &log_pos in &logs {
            let surface = generator.height_at(log_pos.x, log_pos.z);
            assert_eq!(
                block_in_chunk(&chunk, log_pos.x, surface, log_pos.z),
                grass,
                "trunk at {log_pos:?} does not stand on grass"
            );
            assert!(
                block_in_chunk(&chunk, log_pos.x, log_pos.y - 1, log_pos.z) == log || log_pos.y == surface + 1,
                "log at {log_pos:?} is not contiguous with its trunk"
            );
            let mut canopy_above = false;
            for &leaf in &leaves_blocks {
                if leaf.y > log_pos.y && (leaf.x - log_pos.x).abs() <= 2 && (leaf.z - log_pos.z).abs() <= 2 {
                    canopy_above = true;
                    break;
                }
            }
            assert!(canopy_above, "log at {log_pos:?} has no canopy above it");
        }

        let mut trunks = 0usize;
        for &log_pos in &logs {
            if block_in_chunk(&chunk, log_pos.x, log_pos.y - 1, log_pos.z) == log {
                continue; // not the bottom log of its trunk
            }
            let surface = generator.height_at(log_pos.x, log_pos.z);
            assert_eq!(log_pos.y, surface + 1, "trunk base is not on the surface");
            let mut trunk_height = 0i32;
            while block_in_chunk(&chunk, log_pos.x, log_pos.y + trunk_height, log_pos.z) == log {
                trunk_height += 1;
            }
            assert!(
                (4..=6).contains(&trunk_height),
                "trunk at {log_pos:?} is {trunk_height} blocks tall, expected 4..=6"
            );
            trunks += 1;
        }
        assert!(trunks > 0, "found no complete tree trunks");
    }

    #[test]
    fn boulders_rest_on_grass_and_do_not_float() {
        let registry = Registry::base();
        let grass = registry.id_or_air("openblock:grass");
        let stone = registry.id_or_air("openblock:stone");
        let gravel = registry.id_or_air("openblock:gravel");
        let generator = Generator::new(0);

        let mut boulder_blocks = 0usize;
        for cz in -2..=2 {
            for cx in -2..=2 {
                let pos = ChunkPos::new(cx, cz);
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let surface = generator.height_at(world_x, world_z);
                        let above = chunk.get_block(BlockPos::new(world_x, surface + 1, world_z));
                        if above != stone && above != gravel {
                            continue;
                        }
                        boulder_blocks += 1;
                        assert!(surface > SEA_LEVEL + 2, "boulder above water at ({world_x}, {world_z})");
                        assert_eq!(
                            chunk.get_block(BlockPos::new(world_x, surface, world_z)),
                            grass,
                            "boulder at ({world_x}, {world_z}) is not supported by grass"
                        );
                    }
                }
            }
        }
        assert!(
            boulder_blocks > 0,
            "expected at least one boulder in the 5x5 chunk sample"
        );
    }
}

//! Deterministic terrain generation shared by client and dedicated server.
//!
//! Generation is a pure function of the world seed and the chunk position: no
//! thread-local state, wall-clock time or hash-map order participates. All
//! terrain maths uses 32-fractional-bit `i64` fixed point and integer lattice
//! hashes, so a chunk cannot drift between platforms, target features or
//! worker counts (REQ-GEN-002, DEC-046).
//!
//! The height field is four-octave value-noise FBM (cell sizes 64, 32, 16 and
//! 8 blocks) plus a two-octave continental term (512 and 256 blocks). Columns
//! are filled with stone, dirt, a grass or sand surface, water up to
//! [`ob_math::SEA_LEVEL`], and sparse deterministic trees on dry grass. The
//! finished [`ChunkColumn`] is finalised for sky light and has a consistent
//! heightmap.

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

/// Base terrain height before the hills and continental terms are added.
const BASE_HEIGHT: i32 = 68;

/// Peak amplitude of the rolling-hills term, in blocks.
const HILL_AMPLITUDE: i32 = 18;

/// Peak amplitude of the large-scale continental term, in blocks.
const CONTINENT_AMPLITUDE: i32 = 12;

/// Lowest terrain height the generator clamps to, in blocks.
const MIN_HEIGHT: i32 = 40;

/// Highest terrain height the generator clamps to, in blocks.
const MAX_HEIGHT: i32 = 100;

/// Salt for the rolling-hills noise stream.
const SALT_HILLS: i64 = 0x4849_4C4C; // "HILL"

/// Salt for the continental noise stream.
const SALT_CONTINENT: i64 = 0x434F_4E54; // "CONT"

/// Salt for the per-column tree-placement hash.
const SALT_TREE: i64 = 0x5452_4545; // "TREE"

/// Expected number of tree candidates per world column (one in eighty).
const TREE_RARITY: u64 = 80;

/// Horizontal canopy radius in blocks; trees keep this margin from chunk edges.
const TREE_MARGIN: i32 = 2;

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
        }
    }

    /// The terrain block at one world Y for a column whose surface is `height`.
    ///
    /// Water is only produced at or below [`SEA_LEVEL`]; above the surface and
    /// the sea the column is air.
    fn column_block(&self, y: i32, height: i32) -> BlockId {
        if y == WORLD_MIN_Y {
            self.bedrock
        } else if y > height {
            if y <= SEA_LEVEL { self.water } else { BlockId::AIR }
        } else if y == height {
            if (height - SEA_LEVEL).abs() <= 2 {
                self.sand
            } else {
                self.grass
            }
        } else if y > height - 4 {
            self.dirt
        } else {
            self.stone
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
    /// The result contains terrain, sea water and any trees whose full canopy
    /// fits inside the chunk, with sky light seeded and the heightmap rebuilt.
    pub fn generate_chunk(&self, pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        let blocks = TerrainBlocks::from_registry(registry);
        let mut chunk = ChunkColumn::new(pos);
        let (origin_x, origin_z) = pos.origin();

        let mut heights = [0i32; CHUNK_SIZE as usize * CHUNK_SIZE as usize];
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let index = (z * CHUNK_SIZE + x) as usize;
                heights[index] = terrain_height(self.seed, origin_x + x, origin_z + z);
            }
        }

        self.fill_terrain(&mut chunk, &heights, &blocks);
        self.plant_trees(&mut chunk, &heights, &blocks);

        chunk.finalise(registry);
        chunk.recompute_heightmap();
        chunk
    }

    /// The generated terrain surface height at world-space block XZ.
    ///
    /// This is the Y of the topmost terrain block (the grass or sand layer),
    /// independent of trees and water. It is the same integer height field
    /// used by [`Self::generate_chunk`].
    pub fn height_at(&self, x: i32, z: i32) -> i32 {
        terrain_height(self.seed, x, z)
    }

    /// Writes the terrain layers and sea water for every column.
    ///
    /// Blocks are written directly through each section's storage rather than
    /// through [`ChunkColumn::set_block`]; the heightmap is rebuilt once at the
    /// end of generation, so per-block heightmap maintenance would be wasted
    /// work.
    fn fill_terrain(
        &self,
        chunk: &mut ChunkColumn,
        heights: &[i32; CHUNK_SIZE as usize * CHUNK_SIZE as usize],
        blocks: &TerrainBlocks,
    ) {
        let (origin_x, origin_z) = chunk.pos().origin();
        let max_top = heights.iter().copied().max().unwrap_or(WORLD_MIN_Y).max(SEA_LEVEL);
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
                    let height = heights[(z * CHUNK_SIZE + x) as usize];
                    let top = height.max(SEA_LEVEL);
                    if base_y > top {
                        continue;
                    }
                    let local = LocalPos {
                        x: x as u8,
                        y: 0,
                        z: z as u8,
                    };
                    for local_y in 0..section_size {
                        let y = base_y + local_y;
                        if y > top {
                            break;
                        }
                        let block = blocks.column_block(y, height);
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

    /// Plants sparse deterministic trees on dry grass.
    ///
    /// Every candidate is hashed from the world column, so the decision does not
    /// depend on which chunk is generated first. A tree is planted only when all
    /// of its trunk and canopy blocks are currently air and inside this chunk,
    /// which also guarantees that it never overwrites water or crosses a border.
    fn plant_trees(
        &self,
        chunk: &mut ChunkColumn,
        heights: &[i32; CHUNK_SIZE as usize * CHUNK_SIZE as usize],
        blocks: &TerrainBlocks,
    ) {
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
                let surface = heights[(z * CHUNK_SIZE + x) as usize];
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
    /// The trunk is 4–6 logs and the canopy is a small three-layer cap within
    /// [`TREE_MARGIN`] blocks of the trunk. All targets are validated before the
    /// first write so a rejected tree cannot leave a partial trunk behind.
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

        // Trunk plus at most 29 canopy blocks; 40 slots is enough for both.
        let mut planned = [(0i32, 0i32, 0i32, BlockId::AIR); 40];
        let mut count = 0u8;

        for trunk in 0..trunk_height {
            planned[count as usize] = (local_x, base_y + trunk, local_z, blocks.log);
            count += 1;
        }
        // Wide lower canopy: radius 2 without corners and without the trunk.
        for dz in -2i32..=2 {
            for dx in -2i32..=2 {
                if (dx == 0 && dz == 0) || (dx.abs() == 2 && dz.abs() == 2) {
                    continue;
                }
                planned[count as usize] = (local_x + dx, top_y - 1, local_z + dz, blocks.leaves);
                count += 1;
            }
        }
        // Narrow ring around the top log.
        for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            planned[count as usize] = (local_x + dx, top_y, local_z + dz, blocks.leaves);
            count += 1;
        }
        // Cap: centre plus the four adjacent blocks above the top log.
        for (dx, dz) in [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)] {
            planned[count as usize] = (local_x + dx, top_y + 1, local_z + dz, blocks.leaves);
            count += 1;
        }

        let (origin_x, origin_z) = chunk.pos().origin();
        let planned = &planned[..count as usize];
        for &(x, y, z, _) in planned {
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
        for &(x, y, z, block) in planned {
            chunk.set_block(BlockPos::new(origin_x + x, y, origin_z + z), block);
        }
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
/// 8 blocks; the continental term uses 512 and 256 block cells.
fn terrain_height(seed: u64, x: i32, z: i32) -> i32 {
    let hills = fbm(seed, x, z, 6, 4, SALT_HILLS);
    let continent = fbm(seed, x, z, 9, 2, SALT_CONTINENT);
    let base = i64::from(BASE_HEIGHT) * FX_ONE;
    let height = base
        + fx_mul(hills, i64::from(HILL_AMPLITUDE) * FX_ONE)
        + fx_mul(continent, i64::from(CONTINENT_AMPLITUDE) * FX_ONE);
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

    #[test]
    fn generation_is_deterministic() {
        let registry = Registry::base();
        let seed = 0x5EED_1234_ABCD_0001;
        for pos in [ChunkPos::new(0, 0), ChunkPos::new(-3, 7)] {
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
        let log = registry.id_or_air("openblock:log");
        let leaves = registry.id_or_air("openblock:leaves");
        let generator = Generator::new(42);
        let pos = ChunkPos::new(0, 0);
        let chunk = generator.generate_chunk(pos, &registry);
        let (origin_x, origin_z) = pos.origin();
        let mut checked = 0;
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let mut highest_terrain = None;
                let mut has_tree = false;
                for y in (WORLD_MIN_Y..WORLD_MAX_Y).rev() {
                    let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                    if block == log || block == leaves {
                        has_tree = true;
                    }
                    if highest_terrain.is_none() && block != BlockId::AIR && block != water {
                        highest_terrain = Some(y);
                    }
                }
                if has_tree {
                    continue;
                }
                assert_eq!(
                    highest_terrain,
                    Some(generator.height_at(world_x, world_z)),
                    "height_at disagrees at world XZ ({world_x}, {world_z})"
                );
                checked += 1;
            }
        }
        assert!(checked >= 64, "only {checked} tree-free columns checked");
    }

    #[test]
    fn water_is_confined_to_sea_level_and_below() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        let generator = Generator::new(0xC0FF_EE00_1234_5678);

        // Find a deterministic ocean chunk so the water rule is exercised.
        let mut ocean_chunk = None;
        'search: for cz in -4..=4 {
            for cx in -4..=4 {
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
        let ocean_chunk = ocean_chunk.expect("height field must contain an ocean within 9 chunks");

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
                    .any(|z| (0..CHUNK_SIZE).any(|x| generator.height_at(origin_x + x, origin_z + z) > SEA_LEVEL + 4));
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
                    surface > SEA_LEVEL + 4,
                    "tree trunk at {block_pos:?} has surface {surface}, not above sea+4"
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
}

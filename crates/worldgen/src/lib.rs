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
//! plateau term. Two independent low-frequency climate fields (temperature and
//! humidity) and a highland field split the world into five biomes — plains,
//! forest, desert, snowy taiga and mountains — whose terrain parameters are
//! blended continuously so biome borders never form a height seam.
//!
//! Columns are filled with stone, dirt and a biome-specific grass, sand or snow
//! surface, water up to [`ob_math::SEA_LEVEL`], deterministic broadleaf/spruce
//! trees, boulders and snow drifts. The finished [`ChunkColumn`] is finalised
//! for sky light and has a consistent heightmap.

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
const MAX_HEIGHT: i32 = 150;

/// Surface height at and above which the top block is snow.
const SNOW_LINE: i32 = 100;

/// Height difference to a neighbouring column that counts as a steep cliff.
const STEEP_SLOPE: i32 = 3;

/// Terraced plateaus quantise the lowland height field to this many blocks.
const PLATEAU_STEP: i32 = 4;

/// Mountain columns at or above this height are bare stone below the snow line.
const MOUNTAIN_ROCK_LINE: i32 = 90;

/// Mountain columns must be below this height to carry a tree.
const MOUNTAIN_TREE_LIMIT: i32 = 95;

/// Soil layers below a grass or snow surface (in addition to the top block).
const SOIL_DEPTH: i32 = 3;

/// Minimum total desert sand thickness, including the top block.
const DESERT_SAND_MIN: i32 = 3;

/// Maximum total desert sand thickness, including the top block.
const DESERT_SAND_MAX: i32 = 5;

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

/// Salt for the low-frequency temperature field.
const SALT_TEMPERATURE: i64 = 0x5445_4D50; // "TEMP"

/// Salt for the low-frequency humidity field.
const SALT_HUMIDITY: i64 = 0x4855_4D49; // "HUMI"

/// Salt for the broad highland field that selects mountain biomes.
const SALT_HIGHLAND: i64 = 0x4849_4748; // "HIGH"

/// Salt for the shallow-water surface patch noise.
const SALT_SHALLOW: i64 = 0x5348_414C; // "SHAL"

/// Salt for the desert sand-depth hash.
const SALT_DESERT_DEPTH: i64 = 0x4455_4E45; // "DUNE"

/// Salt for the per-column tree-placement hash.
const SALT_TREE: i64 = 0x5452_4545; // "TREE"

/// Salt for the per-column boulder-placement hash.
const SALT_BOULDER: i64 = 0x424F_554C; // "BOUL"

/// Salt for the per-column snow-drift hash.
const SALT_DRIFT: i64 = 0x4452_4946; // "DRIF"

/// Candidate hash rarity for broadleaf trees in forest columns.
///
/// The effective planted density is lower than this candidate rate: a tree
/// needs suitable ground, a two-block border margin and an empty canopy volume.
/// The tuned value yields roughly one trunk per 50 forest columns.
const TREE_RARITY_FOREST: u64 = 7;

/// Candidate hash rarity for spruce trees in snowy-taiga columns.
///
/// This yields roughly one trunk per 55 snowy-taiga columns after the border,
/// ground and non-overlap checks.
const TREE_RARITY_TAIGA: u64 = 20;

/// Candidate hash rarity for broadleaf trees in plains columns.
///
/// This yields roughly one trunk per 150 plains columns after the border,
/// ground and non-overlap checks.
const TREE_RARITY_PLAINS: u64 = 48;

/// Candidate hash rarity for spruce trees on lower mountain slopes.
///
/// Mountains stay sparse: roughly one trunk per 750 mountain columns.
const TREE_RARITY_MOUNTAIN: u64 = 180;

/// Expected number of boulder candidates per plains column.
const BOULDER_RARITY_PLAINS: u64 = 320;

/// Expected number of boulder candidates per lower mountain column.
const BOULDER_RARITY_MOUNTAIN: u64 = 220;

/// Expected number of snow-drift candidates per snowy-taiga column.
const SNOW_DRIFT_RARITY: u64 = 48;

/// Horizontal canopy radius in blocks; trees keep this margin from chunk edges.
const TREE_MARGIN: i32 = 2;

/// Horizontal boulder radius in blocks; boulders keep this margin.
const BOULDER_MARGIN: i32 = 1;

/// Horizontal snow-drift radius in blocks; drifts keep this margin.
const DRIFT_MARGIN: i32 = 1;

/// Edge length of the per-chunk column grid, including the one-block border.
const GRID_SIZE: usize = CHUNK_SIZE as usize + 2;

/// Generated column state for one chunk plus a one-block border (grid index 0
/// is local `-1`, index 17 is local `16`).
///
/// Computing this once per chunk makes slope, beach and biome decisions O(1)
/// per column instead of rescanning neighbouring columns, and it means the
/// climate fields are evaluated once per column per chunk.
type ColumnGrid = [[ColumnSample; GRID_SIZE]; GRID_SIZE];

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

/// A surface biome, chosen from the temperature, humidity and highland fields.
///
/// The dominant biome selects the surface material, tree kind and decoration
/// rules for a column. Terrain height is a continuous blend of all five, so the
/// enum flips at a border without producing a cliff there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Biome {
    /// Gentle rolling grass with sparse broadleaf trees and boulders.
    Plains,
    /// Dense broadleaf woodland on grass.
    Forest,
    /// Low rolling dunes of deep sand, with no trees or grass.
    Desert,
    /// Snow-covered ground with tall, narrow spruce trees.
    SnowyTaiga,
    /// High, steep stone and snow with sparse lower-slope vegetation.
    Mountains,
}

impl Biome {
    /// Every biome, in the deterministic tie-break order used by the classifier.
    pub const ALL: [Self; 5] = [
        Self::Plains,
        Self::Forest,
        Self::Desert,
        Self::SnowyTaiga,
        Self::Mountains,
    ];

    /// The stable lower-case name of this biome.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plains => "plains",
            Self::Forest => "forest",
            Self::Desert => "desert",
            Self::SnowyTaiga => "snowy_taiga",
            Self::Mountains => "mountains",
        }
    }
}

impl std::fmt::Display for Biome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Converts whole blocks to fixed point.
const fn fx_blocks(value: i32) -> i64 {
    (value as i64) * FX_ONE
}

/// Converts an integer percentage to fixed point.
const fn fx_percent(value: i32) -> i64 {
    (value as i64) * FX_ONE / 100
}

/// Terrain shaping parameters for one biome, blended by the biome weights.
#[derive(Clone, Copy)]
struct BiomeTerrain {
    /// Constant height offset in blocks.
    offset: i64,
    /// Multiplier for the rolling-hills term.
    hills: i64,
    /// Multiplier for the large-scale continental term.
    continent: i64,
    /// Multiplier for the terraced-plateau shaping.
    plateau: i64,
    /// Multiplier for the ridged mountain term.
    mountain: i64,
}

/// Per-biome terrain parameters, indexed by `Biome as usize`.
const BIOME_TERRAIN: [BiomeTerrain; 5] = [
    // Plains: low, gentle relief.
    BiomeTerrain {
        offset: fx_blocks(-1),
        hills: fx_percent(80),
        continent: fx_percent(100),
        plateau: fx_percent(100),
        mountain: fx_percent(90),
    },
    // Forest: slightly higher and rougher than plains.
    BiomeTerrain {
        offset: fx_blocks(1),
        hills: fx_percent(110),
        continent: fx_percent(100),
        plateau: fx_percent(70),
        mountain: fx_percent(80),
    },
    // Desert: low, flat dunes with no terraces.
    BiomeTerrain {
        offset: fx_blocks(-3),
        hills: fx_percent(45),
        continent: fx_percent(70),
        plateau: fx_percent(0),
        mountain: fx_percent(30),
    },
    // Snowy taiga: rolling cold lowland.
    BiomeTerrain {
        offset: fx_blocks(0),
        hills: fx_percent(90),
        continent: fx_percent(90),
        plateau: fx_percent(90),
        mountain: fx_percent(90),
    },
    // Mountains: high, rough and steep, with no terraces.
    BiomeTerrain {
        offset: fx_blocks(8),
        hills: fx_percent(135),
        continent: fx_percent(120),
        plateau: fx_percent(0),
        mountain: fx_percent(130),
    },
];

/// Extra lift applied to the mountain biome, independent of the ridge mask.
const MOUNTAIN_LIFT: i32 = 8;

/// Extra lift that follows the ridge field inside the mountain biome.
const MOUNTAIN_RIDGE_LIFT: i32 = 14;

/// Continuous biome membership weights for one column.
#[derive(Clone, Copy)]
struct BiomeWeights {
    /// Normalised fixed-point weights, indexed by `Biome as usize`.
    weights: [i64; 5],
    /// The biome with the largest weight; ties break by `Biome` order.
    dominant: Biome,
}

/// The generated state of one terrain column: its height and biome blend.
#[derive(Clone, Copy)]
struct ColumnSample {
    /// Terrain surface height before trees, boulders and drifts.
    height: i32,
    /// Dominant biome used for surface and decoration rules.
    biome: Biome,
}

impl ColumnSample {
    /// Placeholder for the fixed-size grid initialiser.
    const EMPTY: Self = Self {
        height: 0,
        biome: Biome::Plains,
    };
}

/// The two tree silhouettes the generator can place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeKind {
    /// A short broadleaf tree with a square, broadly tapered canopy.
    Broadleaf,
    /// A tall spruce with diamond tiers tapering into a narrow top.
    Spruce,
}

/// One tree candidate, resolved from its hashed world column.
#[derive(Clone, Copy)]
struct TreeSpec {
    /// Local X of the trunk inside the chunk.
    local_x: i32,
    /// Local Z of the trunk inside the chunk.
    local_z: i32,
    /// Terrain surface height at the trunk column.
    surface: i32,
    /// Per-column hash that seeds trunk height and canopy variation.
    hash: u64,
    /// Which silhouette to build.
    kind: TreeKind,
}

/// The top and soil block chosen for one generated column.
#[derive(Clone, Copy)]
struct ColumnSurface {
    /// Block at the terrain surface height.
    top: BlockId,
    /// Block for the soil layers directly below the surface.
    filler: BlockId,
    /// Number of filler layers below the top block.
    depth: i32,
}

impl ColumnSurface {
    /// Placeholder for the array initialiser; never used for real terrain.
    const EMPTY: Self = Self {
        top: BlockId::AIR,
        filler: BlockId::AIR,
        depth: SOIL_DEPTH,
    };

    /// One material all the way down from the surface, with the standard soil
    /// thickness.
    const fn of(block: BlockId) -> Self {
        Self {
            top: block,
            filler: block,
            depth: SOIL_DEPTH,
        }
    }

    /// One material with an explicit filler depth.
    const fn with_depth(block: BlockId, depth: i32) -> Self {
        Self {
            top: block,
            filler: block,
            depth,
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
    /// fits inside the chunk, occasional boulders and snow drifts, with sky
    /// light seeded and the heightmap rebuilt.
    pub fn generate_chunk(&self, pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        let blocks = TerrainBlocks::from_registry(registry);
        let mut chunk = ChunkColumn::new(pos);
        let (origin_x, origin_z) = pos.origin();
        let columns = self.column_grid(origin_x, origin_z);

        self.fill_terrain(&mut chunk, &columns, &blocks);
        self.plant_trees(&mut chunk, &columns, &blocks);
        self.plant_boulders(&mut chunk, &columns, &blocks);
        self.plant_snow_drifts(&mut chunk, &columns, &blocks);

        chunk.finalise(registry);
        chunk.recompute_heightmap();
        chunk
    }

    /// The dominant biome at world-space block XZ.
    ///
    /// This is a pure function of the seed and column derived from the same
    /// temperature, humidity and highland fields that shape the terrain, so it
    /// agrees with the surface material chosen by [`Self::generate_chunk`] for
    /// any column that is not a beach, an underwater column or a steep slope.
    pub fn biome_at(&self, x: i32, z: i32) -> Biome {
        biome_weights(self.seed, x, z).dominant
    }

    /// The generated terrain surface height at world-space block XZ.
    ///
    /// This is the Y of the topmost terrain block (the grass, sand, gravel,
    /// clay, stone or snow layer), independent of trees, boulders, drifts and
    /// water. It is the same integer height field used by
    /// [`Self::generate_chunk`].
    pub fn height_at(&self, x: i32, z: i32) -> i32 {
        column_sample(self.seed, x, z).height
    }

    /// Computes the column grid for a chunk including its one-block border.
    fn column_grid(&self, origin_x: i32, origin_z: i32) -> ColumnGrid {
        let mut grid = [[ColumnSample::EMPTY; GRID_SIZE]; GRID_SIZE];
        for (grid_z, row) in grid.iter_mut().enumerate() {
            let world_z = origin_z + grid_z as i32 - 1;
            for (grid_x, sample) in row.iter_mut().enumerate() {
                let world_x = origin_x + grid_x as i32 - 1;
                *sample = column_sample(self.seed, world_x, world_z);
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
    fn fill_terrain(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut surfaces = [ColumnSurface::EMPTY; CHUNK_SIZE as usize * CHUNK_SIZE as usize];
        let mut max_top = SEA_LEVEL;

        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let sample = grid_at(columns, x, z);
                let steep = is_steep(columns, x, z);
                let surface = self.column_surface(origin_x + x, origin_z + z, sample, steep, blocks);
                surfaces[(z * CHUNK_SIZE + x) as usize] = surface;
                max_top = max_top.max(sample.height);
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
                    let height = grid_at(columns, x, z).height;
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
    /// Water and the shore are global: water exists only at or below sea
    /// level, the shallow bed is sand with clay/gravel patches, and every dry
    /// column within two blocks of sea level is a sand beach regardless of
    /// biome. Above that band the dominant biome decides:
    ///
    /// - plains and forest: grass with dirt soil, stone on steep slopes and
    ///   snow above the snow line;
    /// - desert: deep sand over stone, never grass or snow;
    /// - snowy taiga: snow over dirt, with exposed stone on steep slopes;
    /// - mountains: grass on low gentle slopes, stone from the rock line or on
    ///   steep slopes, and snow above the snow line.
    fn column_surface(
        &self,
        world_x: i32,
        world_z: i32,
        sample: ColumnSample,
        steep: bool,
        blocks: &TerrainBlocks,
    ) -> ColumnSurface {
        let height = sample.height;
        if height < SEA_LEVEL {
            if SEA_LEVEL - height <= 3 {
                return ColumnSurface::of(self.shallow_surface(world_x, world_z, blocks));
            }
            return ColumnSurface::of(blocks.sand);
        }
        if height <= SEA_LEVEL + 2 {
            return ColumnSurface::of(blocks.sand);
        }

        match sample.biome {
            Biome::Desert => ColumnSurface::with_depth(blocks.sand, self.desert_sand_depth(world_x, world_z)),
            Biome::SnowyTaiga => {
                if steep {
                    ColumnSurface::of(blocks.stone)
                } else {
                    ColumnSurface {
                        top: blocks.snow,
                        filler: blocks.dirt,
                        depth: SOIL_DEPTH,
                    }
                }
            }
            Biome::Mountains => {
                if height >= SNOW_LINE {
                    ColumnSurface {
                        top: blocks.snow,
                        filler: blocks.stone,
                        depth: 2,
                    }
                } else if steep || height >= MOUNTAIN_ROCK_LINE {
                    ColumnSurface::of(blocks.stone)
                } else {
                    ColumnSurface {
                        top: blocks.grass,
                        filler: blocks.dirt,
                        depth: SOIL_DEPTH,
                    }
                }
            }
            Biome::Plains | Biome::Forest => {
                if height >= SNOW_LINE {
                    ColumnSurface {
                        top: blocks.snow,
                        filler: blocks.stone,
                        depth: 2,
                    }
                } else if steep {
                    ColumnSurface::of(blocks.stone)
                } else {
                    ColumnSurface {
                        top: blocks.grass,
                        filler: blocks.dirt,
                        depth: SOIL_DEPTH,
                    }
                }
            }
        }
    }

    /// The number of sand filler layers below a desert surface.
    ///
    /// The top block plus this filler makes a total sand thickness of
    /// [`DESERT_SAND_MIN`]..=[`DESERT_SAND_MAX`] blocks, with stone below.
    fn desert_sand_depth(&self, world_x: i32, world_z: i32) -> i32 {
        let range = (DESERT_SAND_MAX - DESERT_SAND_MIN + 1) as u64;
        let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_DESERT_DEPTH);
        DESERT_SAND_MIN - 1 + (hash % range) as i32
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

    /// Plants deterministic biome-specific trees on dry, suitable ground.
    ///
    /// Every candidate is hashed from the world column, so the decision does not
    /// depend on which chunk is generated first. The biome at the trunk column
    /// selects the tree kind and rarity: broadleaf in plains and forest, spruce
    /// in snowy taiga and on lower mountain slopes, none in deserts. A tree is
    /// planted only when all of its trunk and canopy blocks are currently air,
    /// inside this chunk and outside desert columns, which guarantees that it
    /// never overwrites water, crosses a chunk border or overhangs a dune.
    fn plant_trees(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) {
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
                let sample = grid_at(columns, x, z);
                if sample.height <= SEA_LEVEL + 2 {
                    continue;
                }
                let (kind, rarity, ground) = match sample.biome {
                    Biome::Desert => continue,
                    Biome::Plains => (TreeKind::Broadleaf, TREE_RARITY_PLAINS, blocks.grass),
                    Biome::Forest => (TreeKind::Broadleaf, TREE_RARITY_FOREST, blocks.grass),
                    Biome::SnowyTaiga => (TreeKind::Spruce, TREE_RARITY_TAIGA, blocks.snow),
                    Biome::Mountains => {
                        if sample.height >= MOUNTAIN_TREE_LIMIT {
                            continue;
                        }
                        (TreeKind::Spruce, TREE_RARITY_MOUNTAIN, blocks.grass)
                    }
                };
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_TREE);
                if !hash.is_multiple_of(rarity) {
                    continue;
                }
                if chunk.get_block(BlockPos::new(world_x, sample.height, world_z)) != ground {
                    continue;
                }
                // Cheap rejection before the full canopy plan: a blocked trunk
                // base would make plant_tree abort on its first air check.
                if chunk.get_block(BlockPos::new(world_x, sample.height + 1, world_z)) != BlockId::AIR {
                    continue;
                }
                self.plant_tree(
                    chunk,
                    columns,
                    TreeSpec {
                        local_x: x,
                        local_z: z,
                        surface: sample.height,
                        hash,
                        kind,
                    },
                    blocks,
                );
            }
        }
    }

    /// Writes one tree, or does nothing when any planned block is not air.
    ///
    /// Broadleaf trees have a 4–6 block trunk and a square canopy tapering from
    /// radius 2 to radius 1 with a five-block crown. Spruce trees have a 7–9
    /// block trunk and four diamond tiers of radii 2/2/1/1 plus a single top
    /// leaf, so they are taller and narrower. Every leaf lies within two
    /// blocks of the trunk column and touches a log or another leaf.
    fn plant_tree(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, tree: TreeSpec, blocks: &TerrainBlocks) {
        let TreeSpec {
            local_x,
            local_z,
            surface,
            hash,
            kind,
        } = tree;
        let mut rng = Rng::new(mix64(hash));
        let base_y = surface + 1;
        let mut planned: Vec<(i32, i32, i32, BlockId)> = Vec::with_capacity(96);

        match kind {
            TreeKind::Broadleaf => {
                let trunk_height = 4 + rng.next_below(3) as i32;
                let top_y = base_y + trunk_height - 1;
                for trunk in 0..trunk_height {
                    planned.push((local_x, base_y + trunk, local_z, blocks.log));
                }

                // Up to three square canopy layers tapering from radius 2 to
                // radius 1. A four-block trunk cannot fit its lowest layer at
                // `top_y - 2` without dropping below the surface + 3 rule, so
                // clamp the bottom and taper from there.
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

                // Crown directly above the top log, including the trunk column
                // itself, so the canopy always connects to the top of the trunk.
                for (dx, dz) in [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                    planned.push((local_x + dx, top_y + 1, local_z + dz, blocks.leaves));
                }
            }
            TreeKind::Spruce => {
                let trunk_height = 7 + rng.next_below(3) as i32;
                let top_y = base_y + trunk_height - 1;
                for trunk in 0..trunk_height {
                    planned.push((local_x, base_y + trunk, local_z, blocks.log));
                }

                // Four conical tiers: two diamond layers of radius 2 followed
                // by two plus-shaped layers of radius 1, then a single leaf on
                // top. The diamond cuts the corners of the square radius-2
                // layer, which makes the spruce visibly narrower than a
                // broadleaf of the same nominal radius.
                let canopy_bottom = top_y - 3;
                for (tier, radius) in [2i32, 2, 1, 1].into_iter().enumerate() {
                    let y = canopy_bottom + tier as i32;
                    for dz in -radius..=radius {
                        for dx in -radius..=radius {
                            if dx == 0 && dz == 0 {
                                continue; // the trunk occupies this cell
                            }
                            if dx.abs() + dz.abs() > radius {
                                continue;
                            }
                            planned.push((local_x + dx, y, local_z + dz, blocks.leaves));
                        }
                    }
                }
                planned.push((local_x, top_y + 1, local_z, blocks.leaves));
            }
        }

        let (origin_x, origin_z) = chunk.pos().origin();
        for &(x, y, z, _) in &planned {
            if !(0..CHUNK_SIZE).contains(&x) || !(0..CHUNK_SIZE).contains(&z) {
                return;
            }
            if y <= WORLD_MIN_Y || y >= WORLD_MAX_Y {
                return;
            }
            // Trees never grow into desert columns, even as an overhang.
            if grid_at(columns, x, z).biome == Biome::Desert {
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
    /// Boulders appear in plains and on the lower grassy slopes of mountains.
    /// All five blocks sit directly on grass at the same height, so a boulder
    /// is always supported and never floats over a slope.
    fn plant_boulders(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) {
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
                let sample = grid_at(columns, x, z);
                let rarity = match sample.biome {
                    Biome::Plains => BOULDER_RARITY_PLAINS,
                    Biome::Mountains => BOULDER_RARITY_MOUNTAIN,
                    _ => continue,
                };
                if sample.height <= SEA_LEVEL + 2 {
                    continue;
                }
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_BOULDER);
                if !hash.is_multiple_of(rarity) {
                    continue;
                }

                let offsets = [(0i32, 0i32), (1, 0), (-1, 0), (0, 1), (0, -1)];
                let mut supported = true;
                for &(dx, dz) in &offsets {
                    let neighbour = grid_at(columns, x + dx, z + dz);
                    if neighbour.height != sample.height
                        || neighbour.biome == Biome::Desert
                        || chunk.get_block(BlockPos::new(world_x + dx, sample.height, world_z + dz)) != blocks.grass
                        || chunk.get_block(BlockPos::new(world_x + dx, sample.height + 1, world_z + dz)) != BlockId::AIR
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
                    chunk.set_block(BlockPos::new(world_x + dx, sample.height + 1, world_z + dz), boulder);
                }
            }
        }
    }

    /// Raises small 2×2 snow drifts on flat snowy-taiga ground.
    ///
    /// A drift is only placed when all four columns share the same height, are
    /// snowy taiga and have an air block above their snow surface, so it always
    /// sits on solid snow and never overwrites a tree or a beach.
    fn plant_snow_drifts(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) {
        if blocks.snow == BlockId::AIR {
            return;
        }
        let (origin_x, origin_z) = chunk.pos().origin();
        for z in DRIFT_MARGIN..CHUNK_SIZE - DRIFT_MARGIN {
            for x in DRIFT_MARGIN..CHUNK_SIZE - DRIFT_MARGIN {
                let sample = grid_at(columns, x, z);
                if sample.biome != Biome::SnowyTaiga || sample.height <= SEA_LEVEL + 2 {
                    continue;
                }
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_DRIFT);
                if !hash.is_multiple_of(SNOW_DRIFT_RARITY) {
                    continue;
                }

                let offsets = [(0i32, 0i32), (1, 0), (0, 1), (1, 1)];
                let mut supported = true;
                for &(dx, dz) in &offsets {
                    let neighbour = grid_at(columns, x + dx, z + dz);
                    if neighbour.biome != Biome::SnowyTaiga
                        || neighbour.height != sample.height
                        || chunk.get_block(BlockPos::new(world_x + dx, sample.height, world_z + dz)) != blocks.snow
                        || chunk.get_block(BlockPos::new(world_x + dx, sample.height + 1, world_z + dz)) != BlockId::AIR
                    {
                        supported = false;
                        break;
                    }
                }
                if !supported {
                    continue;
                }
                for &(dx, dz) in &offsets {
                    chunk.set_block(
                        BlockPos::new(world_x + dx, sample.height + 1, world_z + dz),
                        blocks.snow,
                    );
                }
            }
        }
    }
}

/// Reads a local column from the padded column grid (`x`/`z` in `-1..=16`).
fn grid_at(columns: &ColumnGrid, x: i32, z: i32) -> ColumnSample {
    columns[(z + 1) as usize][(x + 1) as usize]
}

/// Whether any of the eight neighbouring columns differs by more than
/// [`STEEP_SLOPE`] blocks.
fn is_steep(columns: &ColumnGrid, x: i32, z: i32) -> bool {
    let height = grid_at(columns, x, z).height;
    for dz in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            if (grid_at(columns, x + dx, z + dz).height - height).abs() > STEEP_SLOPE {
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
    } else if y > height - 1 - surface.depth {
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

/// Terrain parameters blended from the dominant and neighbouring biomes.
#[derive(Clone, Copy)]
struct BlendedTerrain {
    /// Constant height offset in blocks.
    offset: i64,
    /// Multiplier for the rolling-hills term.
    hills: i64,
    /// Multiplier for the continental term.
    continent: i64,
    /// Multiplier for the plateau shaping.
    plateau: i64,
    /// Multiplier for the ridged mountain term.
    mountain: i64,
}

/// Blends the per-biome terrain parameters by the normalised biome weights.
fn blend_terrain(weights: &[i64; 5]) -> BlendedTerrain {
    let mut blended = BlendedTerrain {
        offset: 0,
        hills: 0,
        continent: 0,
        plateau: 0,
        mountain: 0,
    };
    for (weight, parameters) in weights.iter().zip(BIOME_TERRAIN.iter()) {
        let weight = *weight;
        blended.offset += fx_mul(weight, parameters.offset);
        blended.hills += fx_mul(weight, parameters.hills);
        blended.continent += fx_mul(weight, parameters.continent);
        blended.plateau += fx_mul(weight, parameters.plateau);
        blended.mountain += fx_mul(weight, parameters.mountain);
    }
    blended
}

/// Computes the generated state of one column.
///
/// The biome weights are evaluated once and reused for both the height blend
/// and the surface/tree rules, so [`Generator::height_at`] and
/// [`Generator::biome_at`] cannot disagree about the climate at a column.
fn column_sample(seed: u64, x: i32, z: i32) -> ColumnSample {
    let biome = biome_weights(seed, x, z);
    let height = terrain_height(seed, x, z, &biome.weights);
    ColumnSample {
        height,
        biome: biome.dominant,
    }
}

/// Smoothly ramps from 0 at `low` to 1 one quarter of a unit later.
///
/// The input and `low` are fixed point, so the ramp spans a fixed 0.25-wide
/// band. Values below the band map to 0 and values above it map to 1.
fn quarter_ramp(value: i64, low: i64) -> i64 {
    smoothstep(((value - low) * 4).clamp(0, FX_ONE))
}

/// Classifies one column into continuous biome weights.
///
/// Temperature and humidity are independent low-frequency fields (cell sizes
/// 128 and 64 blocks). Each weight is a smoothstep membership:
///
/// - cold (`temperature < -0.25`) selects snowy taiga;
/// - dry non-cold columns (`humidity < 0`) select desert;
/// - wet non-cold columns (`humidity > 0`) select forest, and the remaining
///   non-cold area near average humidity is plains;
/// - a broad highland field above roughly 0.06 selects mountains, blending
///   over a quarter-unit band.
///
/// The lowland weights are scaled by `1 - mountain`, so all five weights
/// always sum to approximately one. Ties break by [`Biome::ALL`] order, which
/// makes the dominant biome a pure, deterministic function of the column.
fn biome_weights(seed: u64, x: i32, z: i32) -> BiomeWeights {
    let temperature = fbm(seed, x, z, 7, 2, SALT_TEMPERATURE);
    let humidity = fbm(seed, x, z, 7, 2, SALT_HUMIDITY);
    let highland = fbm(seed, x, z, 7, 2, SALT_HIGHLAND);

    let cold = quarter_ramp(-temperature, FX_ONE / 4);
    let dry = quarter_ramp(-humidity, 0);
    let wet = quarter_ramp(humidity, 0);
    let mountain = quarter_ramp(highland, FX_ONE / 16);

    let warm = FX_ONE - cold;
    let desert = fx_mul(warm, dry);
    let forest = fx_mul(warm, wet);
    let plains = (warm - desert - forest).max(0);
    let lowland = FX_ONE - mountain;

    let mut weights = [0i64; 5];
    weights[Biome::Plains as usize] = fx_mul(plains, lowland);
    weights[Biome::Forest as usize] = fx_mul(forest, lowland);
    weights[Biome::Desert as usize] = fx_mul(desert, lowland);
    weights[Biome::SnowyTaiga as usize] = fx_mul(cold, lowland);
    weights[Biome::Mountains as usize] = mountain;

    let mut dominant = Biome::Plains;
    for index in 1..Biome::ALL.len() {
        if weights[index] > weights[dominant as usize] {
            dominant = Biome::ALL[index];
        }
    }
    BiomeWeights { weights, dominant }
}

/// The terrain surface height in world block coordinates for one XZ.
///
/// This is the single source of truth used by both [`Generator::height_at`] and
/// [`Generator::generate_chunk`]. The rolling hills are cells of 64, 32, 16 and
/// 8 blocks; the continental term uses 512 and 256 block cells. A low-frequency
/// mask gates a ridged-mountain term, while a separate plateau mask terraces
/// some lowlands into flat, quantised benches. Each term is scaled by the
/// blended biome parameters: deserts are flatter and lower, mountains are
/// higher and rougher, and the mountain biome adds a smooth highland lift.
fn terrain_height(seed: u64, x: i32, z: i32, weights: &[i64; 5]) -> i32 {
    let blended = blend_terrain(weights);
    let hills = fbm(seed, x, z, 6, 4, SALT_HILLS);
    let continent = fbm(seed, x, z, 9, 2, SALT_CONTINENT);
    let base = fx_blocks(BASE_HEIGHT)
        + blended.offset
        + fx_mul(fx_mul(hills, fx_blocks(HILL_AMPLITUDE)), blended.hills)
        + fx_mul(fx_mul(continent, fx_blocks(CONTINENT_AMPLITUDE)), blended.continent);

    // Flatten selected regions onto four-block benches. The gate keeps the
    // terraces occasional, and the smoothstep narrows the transition into a
    // short cliff band rather than smearing the step over the whole region.
    // Deserts and mountains switch the plateau shaping off.
    let plateau_noise = fbm(seed, x, z, 7, 2, SALT_PLATEAU);
    let plateau_gate = fx_mul(plateau_noise - FX_ONE / 4, 4 * FX_ONE).clamp(0, FX_ONE);
    let plateau_factor = fx_mul(smoothstep(plateau_gate), blended.plateau);
    let step = fx_blocks(PLATEAU_STEP);
    let terrace = (base + step / 2).div_euclid(step) * step;
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
    let mountain_term = fx_mul(fx_mul(mountain, fx_blocks(MOUNTAIN_AMPLITUDE)), blended.mountain);

    // The highland biome gets an additional smooth lift, strongest along its
    // ridges, so mountains are visibly higher than the climate around them.
    let mountain_biome = weights[Biome::Mountains as usize];
    let lift = fx_mul(
        mountain_biome,
        fx_blocks(MOUNTAIN_LIFT) + fx_mul(ridge, fx_blocks(MOUNTAIN_RIDGE_LIFT)),
    );

    let height = shaped + mountain_term + lift;
    let rounded = ((height + FX_HALF) >> 32) as i32;
    rounded.clamp(MIN_HEIGHT, MAX_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::CHUNK_SIZE;
    use std::collections::BTreeSet;

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

    /// Finds the first chunk in a positive grid that contains each biome.
    fn find_chunks_for_all_biomes(generator: &Generator, max_chunk: i32) -> [Option<ChunkPos>; 5] {
        let mut found = [None; 5];
        for cz in 0..max_chunk {
            for cx in 0..max_chunk {
                let pos = ChunkPos::new(cx, cz);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let biome = generator.biome_at(origin_x + x, origin_z + z);
                        found[biome as usize].get_or_insert(pos);
                    }
                }
                if found.iter().all(Option::is_some) {
                    return found;
                }
            }
        }
        found
    }

    /// Finds a chunk with at least `min_columns` columns of `wanted`.
    fn find_chunk_with_biome_share(
        generator: &Generator,
        max_chunk: i32,
        wanted: Biome,
        min_columns: usize,
    ) -> Option<ChunkPos> {
        for cz in 0..max_chunk {
            for cx in 0..max_chunk {
                let pos = ChunkPos::new(cx, cz);
                let (origin_x, origin_z) = pos.origin();
                let mut count = 0usize;
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        if generator.biome_at(origin_x + x, origin_z + z) == wanted {
                            count += 1;
                            if count >= min_columns {
                                return Some(pos);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Finds a chunk that contains both desert and dense-tree-biome columns.
    fn find_desert_tree_border_chunk(generator: &Generator, max_chunk: i32) -> Option<ChunkPos> {
        for cz in 0..max_chunk {
            for cx in 0..max_chunk {
                let pos = ChunkPos::new(cx, cz);
                let (origin_x, origin_z) = pos.origin();
                let mut desert = false;
                let mut trees = false;
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        match generator.biome_at(origin_x + x, origin_z + z) {
                            Biome::Desert => desert = true,
                            Biome::Forest | Biome::SnowyTaiga => trees = true,
                            _ => {}
                        }
                    }
                }
                if desert && trees {
                    return Some(pos);
                }
            }
        }
        None
    }

    /// One generated tree, measured from its bottom log block.
    #[derive(Clone, Copy)]
    struct TreeSample {
        x: i32,
        z: i32,
        base_y: i32,
        trunk_height: i32,
        kind: TreeKind,
        leaf_count: usize,
        leaf_columns: usize,
        max_extent: i32,
        leaves_connected: bool,
        ground: BlockId,
    }

    /// Measures every tree in a chunk from its bottom log.
    ///
    /// Leaves are collected in the tree's radius-two box and checked for 26-neighbour
    /// connectivity as they are read. Trees never overlap, because placement aborts
    /// when any planned block is already solid.
    fn collect_trees(chunk: &ChunkColumn, log: BlockId, leaves: BlockId) -> Vec<TreeSample> {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut samples = Vec::new();
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let mut y = WORLD_MIN_Y;
                while y < WORLD_MAX_Y {
                    if chunk.get_block(BlockPos::new(world_x, y, world_z)) != log {
                        y += 1;
                        continue;
                    }
                    if block_in_chunk(chunk, world_x, y - 1, world_z) == log {
                        y += 1; // inside a trunk; the bottom log starts the sample
                        continue;
                    }
                    let base_y = y;
                    let mut top_y = y;
                    while top_y + 1 < WORLD_MAX_Y && chunk.get_block(BlockPos::new(world_x, top_y + 1, world_z)) == log
                    {
                        top_y += 1;
                    }
                    let trunk_height = top_y - base_y + 1;

                    let mut columns = BTreeSet::new();
                    let mut leaf_count = 0usize;
                    let mut max_extent = 0i32;
                    let mut leaves_connected = true;
                    for leaf_y in base_y..=(top_y + 2).min(WORLD_MAX_Y - 1) {
                        for dz in -2..=2 {
                            for dx in -2..=2 {
                                let bx = world_x + dx;
                                let bz = world_z + dz;
                                if chunk.get_block(BlockPos::new(bx, leaf_y, bz)) != leaves {
                                    continue;
                                }
                                leaf_count += 1;
                                max_extent = max_extent.max(dx.abs().max(dz.abs()));
                                columns.insert((dx, dz));
                                let mut connected = false;
                                'neighbours: for ny in -1..=1 {
                                    for nz in -1..=1 {
                                        for nx in -1..=1 {
                                            if nx == 0 && ny == 0 && nz == 0 {
                                                continue;
                                            }
                                            let neighbour = block_in_chunk(chunk, bx + nx, leaf_y + ny, bz + nz);
                                            if neighbour == log || neighbour == leaves {
                                                connected = true;
                                                break 'neighbours;
                                            }
                                        }
                                    }
                                }
                                leaves_connected &= connected;
                            }
                        }
                    }
                    let kind = if trunk_height >= 7 {
                        TreeKind::Spruce
                    } else {
                        TreeKind::Broadleaf
                    };
                    samples.push(TreeSample {
                        x: world_x,
                        z: world_z,
                        base_y,
                        trunk_height,
                        kind,
                        leaf_count,
                        leaf_columns: columns.len(),
                        max_extent,
                        leaves_connected,
                        ground: block_in_chunk(chunk, world_x, base_y - 1, world_z),
                    });
                    y = top_y + 1;
                }
            }
        }
        samples
    }

    /// Checks one column's terrain surface against its dominant biome.
    fn check_surface_column(
        generator: &Generator,
        chunk: &ChunkColumn,
        x: i32,
        z: i32,
        blocks: &TerrainBlocks,
    ) -> Biome {
        let height = generator.height_at(x, z);
        let biome = generator.biome_at(x, z);
        let steep = world_is_steep(generator, x, z);
        let top = chunk.get_block(BlockPos::new(x, height, z));
        assert_ne!(top, BlockId::AIR, "air at the terrain surface ({x}, {height}, {z})");
        assert_ne!(top, blocks.water, "water at the terrain surface ({x}, {height}, {z})");

        if height < SEA_LEVEL {
            assert!(
                top == blocks.sand || top == blocks.gravel || top == blocks.clay,
                "underwater surface at ({x}, {height}, {z}) is neither sand, gravel nor clay"
            );
            return biome;
        }
        if height <= SEA_LEVEL + 2 {
            assert_eq!(top, blocks.sand, "beach column ({x}, {height}, {z}) is not sand");
            return biome;
        }

        match biome {
            Biome::Desert => {
                assert_eq!(top, blocks.sand, "desert surface ({x}, {height}, {z}) is not sand");
                let mut thickness = 0;
                let mut y = height;
                while y >= WORLD_MIN_Y && chunk.get_block(BlockPos::new(x, y, z)) == blocks.sand {
                    thickness += 1;
                    y -= 1;
                }
                assert!(
                    (DESERT_SAND_MIN..=DESERT_SAND_MAX).contains(&thickness),
                    "desert sand at ({x}, {height}, {z}) is {thickness} blocks thick"
                );
                assert_eq!(
                    chunk.get_block(BlockPos::new(x, y, z)),
                    blocks.stone,
                    "desert sand at ({x}, {height}, {z}) does not sit on stone"
                );
            }
            Biome::SnowyTaiga => {
                if steep {
                    assert_eq!(
                        top, blocks.stone,
                        "steep taiga surface ({x}, {height}, {z}) is not stone"
                    );
                } else {
                    assert_eq!(top, blocks.snow, "taiga surface ({x}, {height}, {z}) is not snow");
                }
            }
            Biome::Mountains => {
                if height >= SNOW_LINE {
                    assert_eq!(
                        top, blocks.snow,
                        "high mountain surface ({x}, {height}, {z}) is not snow"
                    );
                } else if steep || height >= MOUNTAIN_ROCK_LINE {
                    assert_eq!(
                        top, blocks.stone,
                        "mountain rock surface ({x}, {height}, {z}) is not stone"
                    );
                } else {
                    assert_eq!(
                        top, blocks.grass,
                        "low mountain surface ({x}, {height}, {z}) is not grass"
                    );
                }
            }
            Biome::Plains | Biome::Forest => {
                if height >= SNOW_LINE {
                    assert_eq!(
                        top, blocks.snow,
                        "high forest/plains surface ({x}, {height}, {z}) is not snow"
                    );
                } else if steep {
                    assert_eq!(
                        top, blocks.stone,
                        "steep forest/plains surface ({x}, {height}, {z}) is not stone"
                    );
                } else {
                    assert_eq!(
                        top, blocks.grass,
                        "forest/plains surface ({x}, {height}, {z}) is not grass"
                    );
                }
            }
        }
        biome
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
    fn biome_names_are_stable() {
        let mut names = BTreeSet::new();
        for biome in Biome::ALL {
            assert!(names.insert(biome.name()), "duplicate biome name {}", biome.name());
            assert_eq!(biome.to_string(), biome.name());
        }
        assert_eq!(Biome::ALL.len(), 5);
        assert_eq!(Biome::SnowyTaiga.name(), "snowy_taiga");
    }

    #[test]
    fn biome_distribution_is_plausible() {
        // The climate fields have 128/64-block cells, so 512x512 blocks cover several
        // biome regions; a 256x256 window can legitimately sit inside one region.
        const SIZE: i32 = 512;
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut counts = [0usize; 5];
            for z in 0..SIZE {
                for x in 0..SIZE {
                    counts[generator.biome_at(x, z) as usize] += 1;
                    if x % 32 == 0 && z % 32 == 0 {
                        let height = generator.height_at(x, z);
                        assert!(
                            (MIN_HEIGHT..=MAX_HEIGHT).contains(&height),
                            "height {height} at ({x}, {z}) is outside {MIN_HEIGHT}..={MAX_HEIGHT}"
                        );
                    }
                }
            }
            let total: usize = counts.iter().sum();
            for (index, count) in counts.iter().enumerate() {
                let biome = Biome::ALL[index];
                assert!(
                    count * 100 >= total * 2,
                    "seed {seed:#x}: biome {biome} has only {:.2}% of {SIZE}x{SIZE}",
                    *count as f64 * 100.0 / total as f64
                );
                assert!(
                    count * 100 <= total * 70,
                    "seed {seed:#x}: biome {biome} covers {:.2}% of {SIZE}x{SIZE}",
                    *count as f64 * 100.0 / total as f64
                );
            }
            println!(
                "seed {seed:#x} biome shares over {SIZE}x{SIZE}: {}",
                counts
                    .map(|count| format!("{:.2}%", count as f64 * 100.0 / total as f64))
                    .join(" ")
            );
        }

        let first = Generator::new(1);
        let second = Generator::new(2);
        let mut differences = 0usize;
        for z in -128..128 {
            for x in -128..128 {
                differences += usize::from(first.biome_at(x, z) != second.biome_at(x, z));
            }
        }
        assert!(
            differences > 512,
            "different seeds produced almost identical biome maps"
        );
    }

    #[test]
    fn biome_surfaces_and_biome_at_agree() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let found = find_chunks_for_all_biomes(&generator, 32);
            let mut seen = [false; 5];
            for biome in Biome::ALL {
                let pos =
                    found[biome as usize].unwrap_or_else(|| panic!("seed {seed:#x}: no {biome} chunk in 32x32 chunks"));
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let column_biome =
                            check_surface_column(&generator, &chunk, origin_x + x, origin_z + z, &blocks);
                        seen[column_biome as usize] = true;
                    }
                }
            }
            assert!(
                seen.iter().all(|seen| *seen),
                "seed {seed:#x}: surface sample missed a biome"
            );
        }
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
                    continue;
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
    fn water_is_confined_to_sea_level_and_below() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        let generator = Generator::new(0xC0FF_EE00_1234_5678);

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
    fn snow_is_biome_appropriate() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        let generator = Generator::new(42);
        let taiga =
            find_chunk_with_biome_share(&generator, 32, Biome::SnowyTaiga, 48).expect("expected a snowy-taiga chunk");
        let desert = find_chunk_with_biome_share(&generator, 32, Biome::Desert, 48).expect("expected a desert chunk");

        let mut terrain_snow = 0usize;
        let mut drift_snow = 0usize;
        let mut taiga_columns = 0usize;
        for dz in -1..=1 {
            for dx in -1..=1 {
                let pos = ChunkPos::new(taiga.x + dx, taiga.z + dz);
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let height = generator.height_at(world_x, world_z);
                        let biome = generator.biome_at(world_x, world_z);
                        let steep = world_is_steep(&generator, world_x, world_z);
                        if biome == Biome::SnowyTaiga && height > SEA_LEVEL + 2 && !steep {
                            taiga_columns += 1;
                            assert_eq!(
                                chunk.get_block(BlockPos::new(world_x, height, world_z)),
                                blocks.snow,
                                "non-steep taiga column ({world_x}, {world_z}) has no snow"
                            );
                        }
                        for y in height..WORLD_MAX_Y {
                            if chunk.get_block(BlockPos::new(world_x, y, world_z)) != blocks.snow {
                                continue;
                            }
                            if y == height {
                                terrain_snow += 1;
                                assert_ne!(biome, Biome::Desert, "snow on a desert surface");
                                assert!(
                                    biome == Biome::SnowyTaiga || height >= SNOW_LINE,
                                    "unexpected snow at surface {height} in {biome}"
                                );
                            } else {
                                drift_snow += 1;
                                assert_eq!(biome, Biome::SnowyTaiga, "snow drift outside the taiga");
                                assert_eq!(y, height + 1, "snow drift is not one block thick");
                                assert_eq!(
                                    chunk.get_block(BlockPos::new(world_x, y - 1, world_z)),
                                    blocks.snow,
                                    "snow drift at ({world_x}, {y}, {world_z}) is not supported"
                                );
                                assert!(height > SEA_LEVEL + 2, "snow drift on a beach or underwater");
                            }
                        }
                    }
                }
            }
        }

        let desert_chunk = generator.generate_chunk(desert, &registry);
        let (desert_x, desert_z) = desert.origin();
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let world_x = desert_x + x;
                let world_z = desert_z + z;
                let height = generator.height_at(world_x, world_z);
                if generator.biome_at(world_x, world_z) != Biome::Desert {
                    continue;
                }
                for y in height..WORLD_MAX_Y {
                    assert_ne!(
                        desert_chunk.get_block(BlockPos::new(world_x, y, world_z)),
                        blocks.snow,
                        "snow in the desert at ({world_x}, {y}, {world_z})"
                    );
                }
            }
        }

        assert!(terrain_snow > 0, "expected snowy terrain");
        assert!(taiga_columns > 0, "expected snowy-taiga columns");
        assert!(drift_snow > 0, "expected at least one raised snow drift");
    }

    #[test]
    fn trees_avoid_deserts_and_water() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        let mut found = None;
        for seed in [1u64, 42, 0] {
            let generator = Generator::new(seed);
            if let Some(pos) = find_desert_tree_border_chunk(&generator, 32) {
                found = Some((generator, pos));
                break;
            }
        }
        let (generator, mixed) = found.expect("expected a desert/forest or desert/taiga border");

        let mut tree_blocks = 0usize;
        let mut desert_columns = 0usize;
        for dz in -1..=1 {
            for dx in -1..=1 {
                let chunk = generator.generate_chunk(ChunkPos::new(mixed.x + dx, mixed.z + dz), &registry);
                let (origin_x, origin_z) = chunk.pos().origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let height = generator.height_at(world_x, world_z);
                        let biome = generator.biome_at(world_x, world_z);
                        if biome == Biome::Desert {
                            desert_columns += 1;
                        }
                        for y in height..WORLD_MAX_Y {
                            let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                            if block != blocks.log && block != blocks.leaves {
                                continue;
                            }
                            tree_blocks += 1;
                            assert_ne!(
                                biome,
                                Biome::Desert,
                                "tree block in the desert at ({world_x}, {y}, {world_z})"
                            );
                            assert!(y > SEA_LEVEL, "tree block at or below sea level");
                            if block == blocks.log && block_in_chunk(&chunk, world_x, y - 1, world_z) != blocks.log {
                                assert_eq!(
                                    y,
                                    height + 1,
                                    "tree trunk at ({world_x}, {y}, {world_z}) does not start on the surface"
                                );
                                assert!(
                                    height > SEA_LEVEL + 2,
                                    "tree trunk at ({world_x}, {world_z}) grows on a beach or underwater"
                                );
                                let ground = chunk.get_block(BlockPos::new(world_x, height, world_z));
                                assert!(
                                    ground == blocks.grass || ground == blocks.snow,
                                    "tree trunk at ({world_x}, {world_z}) stands on {ground:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(tree_blocks > 0, "mixed biome sample contained no trees");
        assert!(desert_columns > 0, "mixed biome sample contained no desert columns");
    }

    #[test]
    fn boulders_rest_on_grass_in_plains_and_mountains() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        let generator = Generator::new(42);
        let plains = find_chunk_with_biome_share(&generator, 32, Biome::Plains, 32).expect("expected a plains chunk");
        let mountains =
            find_chunk_with_biome_share(&generator, 32, Biome::Mountains, 32).expect("expected a mountain chunk");

        let mut boulder_blocks = 0usize;
        for center in [plains, mountains] {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let chunk = generator.generate_chunk(ChunkPos::new(center.x + dx, center.z + dz), &registry);
                    let (origin_x, origin_z) = chunk.pos().origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            let height = generator.height_at(world_x, world_z);
                            let above = chunk.get_block(BlockPos::new(world_x, height + 1, world_z));
                            if above != blocks.stone && above != blocks.gravel {
                                continue;
                            }
                            if chunk.get_block(BlockPos::new(world_x, height, world_z)) != blocks.grass {
                                continue;
                            }
                            boulder_blocks += 1;
                            let biome = generator.biome_at(world_x, world_z);
                            assert!(
                                matches!(biome, Biome::Plains | Biome::Mountains),
                                "boulder in {biome} at ({world_x}, {world_z})"
                            );
                            assert!(height > SEA_LEVEL + 2, "boulder above water at ({world_x}, {world_z})");
                        }
                    }
                }
            }
        }
        assert!(
            boulder_blocks > 0,
            "expected at least one boulder in the plains/mountain sample"
        );
    }

    #[test]
    fn tree_shapes_are_connected_and_biome_specific() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        let generator = Generator::new(42);
        let forest = find_chunk_with_biome_share(&generator, 32, Biome::Forest, 48).expect("expected a forest chunk");
        let taiga =
            find_chunk_with_biome_share(&generator, 32, Biome::SnowyTaiga, 48).expect("expected a snowy-taiga chunk");

        let mut samples = Vec::new();
        for center in [forest, taiga] {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let chunk = generator.generate_chunk(ChunkPos::new(center.x + dx, center.z + dz), &registry);
                    samples.extend(collect_trees(&chunk, blocks.log, blocks.leaves));
                }
            }
        }

        // Keep only trees with no other trunk within four blocks; a neighbouring
        // canopy cannot reach into an isolated tree's radius-two measuring box, so
        // the measured shape is not contaminated by a second tree.
        let mut isolated = Vec::new();
        for (index, sample) in samples.iter().enumerate() {
            let close_neighbour = samples.iter().enumerate().any(|(other_index, other)| {
                other_index != index && (other.x - sample.x).abs() <= 4 && (other.z - sample.z).abs() <= 4
            });
            if !close_neighbour {
                isolated.push(*sample);
            }
        }

        let mut broadleaf = Vec::new();
        let mut spruce = Vec::new();
        for sample in isolated {
            let biome = generator.biome_at(sample.x, sample.z);
            if !matches!(
                biome,
                Biome::Plains | Biome::Forest | Biome::SnowyTaiga | Biome::Mountains
            ) {
                continue;
            }
            assert!(
                sample.leaf_count > 0,
                "tree at ({}, {}) has no canopy",
                sample.x,
                sample.z
            );
            assert!(
                sample.leaves_connected,
                "tree at ({}, {}) has a floating leaf",
                sample.x, sample.z
            );
            assert!(
                sample.max_extent <= 2,
                "tree at ({}, {}) is wider than radius 2",
                sample.x,
                sample.z
            );
            assert_eq!(
                sample.base_y,
                generator.height_at(sample.x, sample.z) + 1,
                "tree at ({}, {}) does not start on the terrain surface",
                sample.x,
                sample.z
            );
            match sample.kind {
                TreeKind::Broadleaf => {
                    assert!(
                        matches!(biome, Biome::Plains | Biome::Forest),
                        "broadleaf tree in {biome}"
                    );
                    assert!(
                        (4..=6).contains(&sample.trunk_height),
                        "broadleaf trunk is not 4..=6 blocks"
                    );
                    assert_eq!(sample.ground, blocks.grass, "broadleaf tree does not stand on grass");
                    assert!(sample.leaf_columns >= 20, "broadleaf canopy is unexpectedly narrow");
                    broadleaf.push(sample);
                }
                TreeKind::Spruce => {
                    assert!(
                        matches!(biome, Biome::SnowyTaiga | Biome::Mountains),
                        "spruce tree in {biome}"
                    );
                    assert!(
                        (7..=9).contains(&sample.trunk_height),
                        "spruce trunk is not 7..=9 blocks"
                    );
                    if biome == Biome::SnowyTaiga {
                        assert_eq!(sample.ground, blocks.snow, "taiga spruce does not stand on snow");
                    } else {
                        assert_eq!(sample.ground, blocks.grass, "mountain spruce does not stand on grass");
                        assert!(
                            sample.base_y - 1 < MOUNTAIN_TREE_LIMIT,
                            "mountain spruce above the tree limit"
                        );
                    }
                    assert!(sample.leaf_columns <= 15, "spruce canopy is unexpectedly wide");
                    spruce.push(sample);
                }
            }
        }

        assert!(broadleaf.len() >= 4, "only {} broadleaf trees sampled", broadleaf.len());
        assert!(spruce.len() >= 4, "only {} spruce trees sampled", spruce.len());

        let broadleaf_height: i32 = broadleaf.iter().map(|tree| tree.trunk_height).sum();
        let spruce_height: i32 = spruce.iter().map(|tree| tree.trunk_height).sum();
        let broadleaf_width: i32 = broadleaf.iter().map(|tree| tree.leaf_columns as i32).sum();
        let spruce_width: i32 = spruce.iter().map(|tree| tree.leaf_columns as i32).sum();
        assert!(
            spruce_height * (broadleaf.len() as i32) > broadleaf_height * (spruce.len() as i32),
            "spruce trees are not taller on average"
        );
        assert!(
            spruce_width * (broadleaf.len() as i32) < broadleaf_width * (spruce.len() as i32),
            "spruce trees are not narrower on average"
        );
    }

    #[test]
    #[ignore = "throughput benchmark; run with --ignored --nocapture"]
    fn throughput_and_tree_density_report() {
        let registry = Registry::base();
        let blocks = TerrainBlocks::from_registry(&registry);
        let generator = Generator::new(42);

        // Warm up code paths and caches before timing.
        let _ = std::hint::black_box(generator.generate_chunk(ChunkPos::new(0, 0), &registry));

        let chunks = 300i32;
        let start = std::time::Instant::now();
        let mut worst = std::time::Duration::ZERO;
        for index in 0..chunks {
            let chunk_start = std::time::Instant::now();
            let _ = std::hint::black_box(generator.generate_chunk(ChunkPos::new(index % 20, index / 20), &registry));
            worst = worst.max(chunk_start.elapsed());
        }
        let elapsed = start.elapsed();
        let millis = elapsed.as_secs_f64() * 1000.0 / f64::from(chunks);
        println!(
            "throughput: {chunks} chunks in {elapsed:?} ({millis:.3} ms/chunk, {:.0} chunks/s, worst chunk {worst:?})",
            1000.0 / millis
        );
        assert!(millis < 5.0, "generation averaged {millis:.3} ms/chunk");

        // Tree density over 16x16 chunks (256x256 blocks).
        let mut biome_columns = [0u64; 5];
        let mut tree_trunks = [0u64; 5];
        let mut snow_drifts = 0u64;
        for cz in 0..16 {
            for cx in 0..16 {
                let chunk = generator.generate_chunk(ChunkPos::new(cx, cz), &registry);
                let (origin_x, origin_z) = chunk.pos().origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let height = generator.height_at(world_x, world_z);
                        let biome = generator.biome_at(world_x, world_z) as usize;
                        biome_columns[biome] += 1;
                        for y in height..WORLD_MAX_Y {
                            let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                            if block == blocks.log && block_in_chunk(&chunk, world_x, y - 1, world_z) != blocks.log {
                                tree_trunks[biome] += 1;
                            } else if block == blocks.snow && y > height {
                                snow_drifts += 1;
                            }
                        }
                    }
                }
            }
        }
        for (index, biome) in Biome::ALL.into_iter().enumerate() {
            let columns = biome_columns[index];
            let trunks = tree_trunks[index];
            let density = if trunks == 0 {
                "none".to_owned()
            } else {
                format!("1 per {:.1} columns", columns as f64 / trunks as f64)
            };
            println!(
                "biome {biome}: {columns} columns, {trunks} tree trunks ({density}), {} drifts",
                if index == Biome::SnowyTaiga as usize {
                    snow_drifts
                } else {
                    0
                }
            );
        }
    }
}

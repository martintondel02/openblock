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
//!
//! After the terrain is filled, integer 3D value-noise carvers cut spaghetti
//! tunnels and low-Y caverns through the stone layer, rare hashed surface
//! entrances connect those caves to dry ground, and rare region-hashed ravines
//! open stepped canyons. Carving never touches water, bedrock, the top of the
//! surface (except at a real entrance or ravine) or the protected spawn crust;
//! features are planted afterwards on whatever solid surface remains.
//!
//! Ore veins are grown last, after the surface features. Each chunk derives an
//! independent vein stream from its world origin and the world seed, grows
//! connected six-neighbour blobs of coal, iron and copper inside the tuned Y
//! bands, and replaces a block only when it is still stone. Ores therefore never
//! overwrite logs, leaves, water, bedrock, dirt, cave air or the terrain
//! surface, and generating chunks in any order produces the same veins.

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

/// Salt for the first 3D spaghetti-tunnel field.
const SALT_CAVE_TUNNEL_A: i64 = 0x4341_5645; // "CAVE"

/// Salt for the second 3D spaghetti-tunnel field.
const SALT_CAVE_TUNNEL_B: i64 = 0x5455_4E4C; // "TUNL"

/// Salt for the 3D cavern field.
const SALT_CAVERN: i64 = 0x4341_564E; // "CAVN"

/// Salt for the rare per-column surface-entrance hash.
const SALT_CAVE_ENTRANCE: i64 = 0x454E_5452; // "ENTR"

/// Salt for the per-region ravine candidate hash.
const SALT_RAVINE_REGION: i64 = 0x5241_5649; // "RAVI"

/// Horizontal and vertical spacing of the coarse 3D cave sample grid.
///
/// Each section samples 5×5×5 lattice points at four-block spacing; the
/// per-block value is a trilinear blend of the eight surrounding samples.
const CAVE_GRID_STEP: i32 = 4;

/// Number of coarse samples along one section axis (spacing four blocks).
const CAVE_GRID_SIZE: usize = (SECTION_SIZE / CAVE_GRID_STEP as usize) + 1;

/// Total samples in one coarse 3D cave grid.
const CAVE_GRID_CELLS: usize = CAVE_GRID_SIZE * CAVE_GRID_SIZE * CAVE_GRID_SIZE;

/// Half-width threshold of each spaghetti-tunnel field, in fixed point.
///
/// A block is a tunnel when both independent fields are within this band. The
/// two-sheet intersection produces one-dimensional tunnels; the threshold
/// controls their roughly 2–8 block diameter.
const CAVE_TUNNEL_THRESHOLD: i64 = fx_percent(9);

/// Highest Y at which cavern noise may carve chambers.
const CAVE_CAVERN_MAX_Y: i32 = 32;

/// Cavern threshold at the top of the cavern band (rarer).
const CAVE_CAVERN_THRESHOLD_HIGH: i64 = fx_percent(52);

/// Cavern threshold at the bottom of the world (deeper = larger chambers).
const CAVE_CAVERN_THRESHOLD_LOW: i64 = fx_percent(42);

/// Vertical span over which the cavern threshold tapers downwards.
const CAVE_CAVERN_TAPER: i32 = 96;

/// Solid blocks kept between a normal cave and the terrain surface.
const CAVE_SURFACE_SEAL: i32 = 3;

/// Solid blocks kept between a cave and an underwater surface.
///
/// The extra seal means carving can never leave a water block suspended above
/// cave air at the sea floor.
const CAVE_WATER_SEAL: i32 = 5;

/// Block radius around the world origin where caves keep the top eight blocks.
const SPAWN_PROTECT_CHUNKS: i32 = 2;

/// Solid blocks kept below the surface inside the spawn-protection square.
const SPAWN_PROTECT_DEPTH: i32 = 8;

/// One surface-entrance candidate per this many columns.
const CAVE_ENTRANCE_RARITY: u64 = 4096;

/// How far below the surface an entrance shaft searches for a cave.
const CAVE_ENTRANCE_SCAN_DEPTH: i32 = 24;

/// Ravine candidate regions are this many chunks on a side (128 blocks).
const RAVINE_REGION_CHUNKS: i32 = 8;

/// One ravine candidate per this many regions (1 in 8).
const RAVINE_RARITY: u64 = 8;

/// Vertical depth of one stepped ravine wall band.
const RAVINE_STEP: i32 = 6;

/// Direction table for ravines, in primitive integer vectors.
///
/// Comparing squared distances against squared integer vector lengths avoids
/// any floating-point trigonometry while still giving sixteen orientations.
const RAVINE_DIRECTIONS: [(i32, i32); 16] = [
    (1, 0),
    (2, 1),
    (1, 1),
    (1, 2),
    (0, 1),
    (-1, 2),
    (-1, 1),
    (-2, 1),
    (-1, 0),
    (-2, -1),
    (-1, -1),
    (-1, -2),
    (0, -1),
    (1, -2),
    (1, -1),
    (2, -1),
];

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

/// Salt for the coal-ore vein stream.
const SALT_ORE_COAL: i64 = 0x434F_414C; // "COAL"

/// Salt for the iron-ore vein stream.
const SALT_ORE_IRON: i64 = 0x4952_4F4E; // "IRON"

/// Salt for the copper-ore vein stream.
const SALT_ORE_COPPER: i64 = 0x434F_5052; // "COPR"

/// Lowest world Y any v1 ore block may occupy (inclusive).
const ORE_MIN_Y: i32 = 5;

/// Highest world Y any v1 ore block may occupy (inclusive).
const ORE_MAX_Y: i32 = 96;

/// Largest vein the grower can build, in blocks.
///
/// This bounds the fixed cell buffer used by [`grow_ore_vein`]; the tuning table
/// is checked against it at compile time.
const MAX_ORE_VEIN: usize = 14;

/// Candidate stone cells tried before a vein seed is abandoned.
///
/// Most of the terrain band is stone, so four tries make a lost vein rare while
/// keeping the worst-case per-vein cost tiny.
const ORE_START_ATTEMPTS: u32 = 4;

/// Growth attempts allowed per requested vein block.
///
/// A vein normally fills its target size; the cap only matters when a cave or
/// the chunk border encloses the blob.
const ORE_GROWTH_ATTEMPTS_PER_BLOCK: usize = 12;

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

/// One deterministic ravine candidate generated by a region hash.
#[derive(Debug, Clone, Copy)]
struct Ravine {
    /// World-space X of the segment centre.
    center_x: i32,
    /// World-space Z of the segment centre.
    center_z: i32,
    /// Integer X component of the ravine's long axis.
    dir_x: i32,
    /// Integer Z component of the ravine's long axis.
    dir_z: i32,
    /// Half-length of the ellipsoidal floor plan, in blocks.
    half_length: i32,
    /// Half-width at the surface, in blocks.
    half_width: i32,
    /// Depth below each column's surface, in blocks.
    depth: i32,
}

impl Ravine {
    /// Conservative world-space radius the candidate can touch.
    fn max_radius(self) -> i32 {
        self.half_length + self.half_width + 2
    }

    /// Whether the candidate's conservative footprint touches a chunk.
    fn intersects_chunk(self, origin_x: i32, origin_z: i32) -> bool {
        let reach = self.max_radius();
        self.center_x + reach >= origin_x
            && self.center_x - reach < origin_x + CHUNK_SIZE
            && self.center_z + reach >= origin_z
            && self.center_z - reach < origin_z + CHUNK_SIZE
    }

    /// Whether suppressing this ravine is required for a safe spawn.
    fn intersects_spawn_protection(self) -> bool {
        let reach = self.max_radius();
        let min = -SPAWN_PROTECT_CHUNKS * CHUNK_SIZE;
        let max = (SPAWN_PROTECT_CHUNKS + 1) * CHUNK_SIZE - 1;
        self.center_x + reach >= min
            && self.center_x - reach <= max
            && self.center_z + reach >= min
            && self.center_z - reach <= max
    }
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

/// One of the three v1 ore types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OreKind {
    /// Common fuel ore, shallow to mid-depth.
    Coal,
    /// Tool-tier ore in the lower half of the stone band.
    Iron,
    /// Tool-tier ore in the mid band, overlapping coal and iron.
    Copper,
}

impl OreKind {
    /// Every ore, in the stable order used by generation and statistics.
    #[cfg(test)]
    const ALL: [Self; 3] = [Self::Coal, Self::Iron, Self::Copper];

    /// Index into tuning and statistics arrays.
    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Self::Coal => 0,
            Self::Iron => 1,
            Self::Copper => 2,
        }
    }

    /// The stable lower-case name used by diagnostics.
    #[cfg(test)]
    const fn name(self) -> &'static str {
        match self {
            Self::Coal => "coal",
            Self::Iron => "iron",
            Self::Copper => "copper",
        }
    }
}

/// Tuning for one ore type: Y band, vein size and target density.
#[derive(Debug, Clone, Copy)]
struct OreSpec {
    /// Which ore this entry configures.
    kind: OreKind,
    /// Salt that gives this ore an independent, order-free stream.
    salt: i64,
    /// Lowest world Y this ore may occupy (inclusive).
    min_y: i32,
    /// Highest world Y this ore may occupy (inclusive).
    max_y: i32,
    /// Smallest vein, in blocks.
    min_size: u32,
    /// Largest vein, in blocks.
    max_size: u32,
    /// Target vein blocks per thousand cells of the estimated stone band.
    ///
    /// The estimate comes from the height grid and therefore includes the
    /// surface soil and cave volume that a vein cannot replace. This factor is
    /// tuned (see `docs/plan/08-measurements.md`) so the measured share of
    /// remaining stone in the band lands in the documented density range.
    target_per_mille: u64,
}

/// The v1 ore tuning table.
///
/// Ranges and sizes are the survival-progression contract; density targets are
/// verified by `ore_density_is_bounded`. Salts must be unique so adding or
/// reordering an ore cannot shift another ore's stream (REQ-GEN-004).
const ORE_SPECS: [OreSpec; 3] = [
    OreSpec {
        kind: OreKind::Coal,
        salt: SALT_ORE_COAL,
        min_y: ORE_MIN_Y,
        max_y: ORE_MAX_Y,
        min_size: 6,
        max_size: 14,
        target_per_mille: 9,
    },
    OreSpec {
        kind: OreKind::Iron,
        salt: SALT_ORE_IRON,
        min_y: ORE_MIN_Y,
        max_y: 64,
        min_size: 3,
        max_size: 8,
        target_per_mille: 5,
    },
    OreSpec {
        kind: OreKind::Copper,
        salt: SALT_ORE_COPPER,
        min_y: 24,
        max_y: ORE_MAX_Y,
        min_size: 4,
        max_size: 10,
        target_per_mille: 4,
    },
];

// The vein grower uses a fixed-size cell buffer; keep the table in step.
const _: () = assert!(ORE_SPECS[0].max_size <= MAX_ORE_VEIN as u32);
const _: () = assert!(ORE_SPECS[1].max_size <= MAX_ORE_VEIN as u32);
const _: () = assert!(ORE_SPECS[2].max_size <= MAX_ORE_VEIN as u32);

/// Ore block ids resolved from one registry.
#[derive(Debug, Clone, Copy)]
struct OreBlocks {
    /// `openblock:coal_ore`.
    coal: BlockId,
    /// `openblock:iron_ore`.
    iron: BlockId,
    /// `openblock:copper_ore`.
    copper: BlockId,
}

impl OreBlocks {
    /// Resolves the base ore palette, degrading to air when an ore is absent.
    fn from_registry(registry: &Registry) -> Self {
        Self {
            coal: registry.id_or_air("openblock:coal_ore"),
            iron: registry.id_or_air("openblock:iron_ore"),
            copper: registry.id_or_air("openblock:copper_ore"),
        }
    }

    /// The block id for one ore kind.
    const fn get(self, kind: OreKind) -> BlockId {
        match kind {
            OreKind::Coal => self.coal,
            OreKind::Iron => self.iron,
            OreKind::Copper => self.copper,
        }
    }
}

/// Counts of ore blocks and veins placed in one generated chunk.
///
/// Returned by [`Generator::ore_debug_stats`]. A vein is counted once it has
/// placed at least one block; `blocks` counts the ore blocks actually written
/// after checking that each cell was stone. This is pure diagnostic data and
/// does not participate in generation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OreDebugStats {
    /// Coal-ore blocks written.
    pub coal_blocks: u32,
    /// Iron-ore blocks written.
    pub iron_blocks: u32,
    /// Copper-ore blocks written.
    pub copper_blocks: u32,
    /// Coal veins that placed at least one block.
    pub coal_veins: u32,
    /// Iron veins that placed at least one block.
    pub iron_veins: u32,
    /// Copper veins that placed at least one block.
    pub copper_veins: u32,
}

impl OreDebugStats {
    /// Records one grown vein and the number of blocks it placed.
    fn record(&mut self, kind: OreKind, placed: u32) {
        match kind {
            OreKind::Coal => {
                self.coal_blocks += placed;
                self.coal_veins += 1;
            }
            OreKind::Iron => {
                self.iron_blocks += placed;
                self.iron_veins += 1;
            }
            OreKind::Copper => {
                self.copper_blocks += placed;
                self.copper_veins += 1;
            }
        }
    }
}

/// Internal generation switches.
///
/// The public API always enables ores. Tests use [`Self::WITHOUT_ORES`] to
/// inspect the pre-ore chunk and prove that every ore block replaced stone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GenerationOptions {
    /// Whether to run the ore-vein pass.
    ores: bool,
}

impl GenerationOptions {
    /// Normal generation, including ores.
    const WITH_ORES: Self = Self { ores: true };
    /// Terrain, caves and features only, for differential tests and reports.
    #[allow(dead_code)] // used by worldgen tests and the ore reports only
    const WITHOUT_ORES: Self = Self { ores: false };
}

/// Counts of blocks removed by the cave, entrance and ravine carvers.
///
/// Returned by [`Generator::cave_debug_stats`]; the three categories are
/// disjoint because the entrance and ravine stages only remove blocks that the
/// cave stage left solid.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaveDebugStats {
    /// Solid blocks replaced by air by the noise-driven cave carver.
    pub cave_blocks: u32,
    /// Surface columns where a shaft was opened into an existing cave.
    pub entrance_columns: u32,
    /// Solid blocks replaced by air by ravine carving.
    pub ravine_blocks: u32,
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
    /// The result contains terrain, sea water, deterministic cave and ravine
    /// carving, any trees whose full canopy fits inside the chunk, occasional
    /// boulders and snow drifts, deterministic ore veins and sky light, with a
    /// consistent heightmap.
    pub fn generate_chunk(&self, pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        self.generate_chunk_with_options(pos, registry, GenerationOptions::WITH_ORES)
            .0
    }

    /// Generates one chunk and reports how many blocks the carvers removed.
    ///
    /// This is the diagnostic entry point used by the generation tests and
    /// benchmarks; [`Self::generate_chunk`] discards the report. It is a pure
    /// function of the seed, position and registry like generation itself.
    pub fn cave_debug_stats(&self, pos: ChunkPos, registry: &Registry) -> CaveDebugStats {
        self.generate_chunk_with_options(pos, registry, GenerationOptions::WITH_ORES)
            .1
    }

    /// Generates one chunk and reports how many ore blocks and veins it placed.
    ///
    /// Diagnostic counterpart to [`Self::cave_debug_stats`] and equally pure.
    /// Ore placement only replaces stone below the terrain surface, so this
    /// counts every ore block in the finished chunk.
    pub fn ore_debug_stats(&self, pos: ChunkPos, registry: &Registry) -> OreDebugStats {
        self.generate_chunk_with_options(pos, registry, GenerationOptions::WITH_ORES)
            .2
    }

    /// Generates one chunk plus the cave/ravine carving report.
    #[cfg(test)]
    fn generate_chunk_with_report(&self, pos: ChunkPos, registry: &Registry) -> (ChunkColumn, CaveDebugStats) {
        let (chunk, cave_stats, _) = self.generate_chunk_with_options(pos, registry, GenerationOptions::WITH_ORES);
        (chunk, cave_stats)
    }

    /// Generates one chunk with explicit internal switches.
    ///
    /// Returns the chunk, cave/ravine carving counts and ore placement counts.
    /// Ore insertion runs after every feature stage and only ever replaces a
    /// stone block, so generating with [`GenerationOptions::WITHOUT_ORES`] and
    /// generating normally differ in exactly the ore cells.
    fn generate_chunk_with_options(
        &self,
        pos: ChunkPos,
        registry: &Registry,
        options: GenerationOptions,
    ) -> (ChunkColumn, CaveDebugStats, OreDebugStats) {
        let blocks = TerrainBlocks::from_registry(registry);
        let mut chunk = ChunkColumn::new(pos);
        let (origin_x, origin_z) = pos.origin();
        let columns = self.column_grid(origin_x, origin_z);

        let max_top = self.fill_terrain(&mut chunk, &columns, &blocks);
        let stats = CaveDebugStats {
            cave_blocks: self.carve_caves(&mut chunk, &columns, &blocks, max_top),
            entrance_columns: self.carve_surface_entrances(&mut chunk, &columns, &blocks),
            ravine_blocks: self.carve_ravines(&mut chunk, &columns, &blocks),
        };

        self.plant_trees(&mut chunk, &columns, &blocks);
        self.plant_boulders(&mut chunk, &columns, &blocks);
        self.plant_snow_drifts(&mut chunk, &columns, &blocks);

        let ore_stats = if options.ores {
            let ores = OreBlocks::from_registry(registry);
            self.generate_ores(&mut chunk, &columns, &blocks, &ores)
        } else {
            OreDebugStats::default()
        };

        chunk.finalise(registry);
        chunk.recompute_heightmap();
        (chunk, stats, ore_stats)
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
    fn fill_terrain(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) -> i32 {
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
        max_top
    }

    /// Carves the deterministic 3D cave system.
    ///
    /// The carver samples three noise fields on a 5×5×5 lattice per section
    /// (four-block spacing) and trilinearly interpolates them per block:
    ///
    /// - two independent spaghetti fields whose thin intersection forms
    ///   one-dimensional tunnels through the whole stone layer;
    /// - a lower-frequency cavern field that only carves below
    ///   [`CAVE_CAVERN_MAX_Y`], with a threshold that lowers with depth so the
    ///   low world has larger chambers.
    ///
    /// Carving is capped below the surface (`CAVE_SURFACE_SEAL`, five blocks
    /// under water and eight inside the spawn area), never touches water,
    /// bedrock or the bottom world row, and only removes solid blocks. The
    /// coarse grid is aligned to world coordinates, so chunk borders are
    /// seamless and order-independent.
    fn carve_caves(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks, max_top: i32) -> u32 {
        let (origin_x, origin_z) = chunk.pos().origin();
        let section_size = SECTION_SIZE as i32;
        let first_section_y = WORLD_MIN_Y / section_size;
        let last_section_offset = ((max_top - WORLD_MIN_Y) / section_size).min(N_SECTIONS as i32 - 1);
        let mut heights = [0i32; CHUNK_SIZE as usize * CHUNK_SIZE as usize];
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                heights[(z * CHUNK_SIZE + x) as usize] = grid_at(columns, x, z).height;
            }
        }

        let mut carved = 0u32;
        for offset in 0..=last_section_offset {
            let section_y = first_section_y + offset;
            let base_y = section_y * section_size;
            let mut tunnel_a = [0i64; CAVE_GRID_CELLS];
            let mut tunnel_b = [0i64; CAVE_GRID_CELLS];
            let mut cavern = [0i64; CAVE_GRID_CELLS];
            for gy in 0..CAVE_GRID_SIZE {
                let world_y = base_y + (gy as i32) * CAVE_GRID_STEP;
                for gz in 0..CAVE_GRID_SIZE {
                    let world_z = origin_z + (gz as i32) * CAVE_GRID_STEP;
                    for gx in 0..CAVE_GRID_SIZE {
                        let world_x = origin_x + (gx as i32) * CAVE_GRID_STEP;
                        let index = cave_grid_index(gx, gy, gz);
                        tunnel_a[index] = fbm_3d(self.seed, world_x, world_y, world_z, 5, 2, SALT_CAVE_TUNNEL_A);
                        tunnel_b[index] = fbm_3d(self.seed, world_x, world_y, world_z, 5, 2, SALT_CAVE_TUNNEL_B);
                        // The cavern field is only used up to one grid step
                        // above its Y limit; skipping higher samples saves a
                        // third of the cave noise cost in the upper sections.
                        if world_y <= CAVE_CAVERN_MAX_Y + CAVE_GRID_STEP {
                            cavern[index] = fbm_3d(self.seed, world_x, world_y, world_z, 6, 2, SALT_CAVERN);
                        }
                    }
                }
            }

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
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    let seal = if spawn_protected(world_x, world_z) {
                        SPAWN_PROTECT_DEPTH
                    } else if height < SEA_LEVEL {
                        CAVE_WATER_SEAL
                    } else {
                        CAVE_SURFACE_SEAL
                    };
                    let top = height - seal;
                    if top < base_y {
                        continue;
                    }
                    let gx = (x / CAVE_GRID_STEP) as usize;
                    let fx = i64::from(x % CAVE_GRID_STEP);
                    let gz = (z / CAVE_GRID_STEP) as usize;
                    let fz = i64::from(z % CAVE_GRID_STEP);

                    for local_y in 0..section_size {
                        let y = base_y + local_y;
                        if y > top {
                            break;
                        }
                        if y <= WORLD_MIN_Y {
                            continue;
                        }
                        let gy = (local_y / CAVE_GRID_STEP) as usize;
                        let fy = i64::from(local_y % CAVE_GRID_STEP);
                        let tunnel = cave_trilinear(&tunnel_a, gx, fx, gy, fy, gz, fz).abs() < CAVE_TUNNEL_THRESHOLD
                            && cave_trilinear(&tunnel_b, gx, fx, gy, fy, gz, fz).abs() < CAVE_TUNNEL_THRESHOLD;
                        let cavern_carve = if y <= CAVE_CAVERN_MAX_Y {
                            cave_trilinear(&cavern, gx, fx, gy, fy, gz, fz) > cavern_threshold(y)
                        } else {
                            false
                        };
                        if !tunnel && !cavern_carve {
                            continue;
                        }
                        let local = LocalPos {
                            x: x as u8,
                            y: local_y as u8,
                            z: z as u8,
                        };
                        let current = section.get(local);
                        if current == BlockId::AIR || current == blocks.water || current == blocks.bedrock {
                            continue;
                        }
                        section.set(local, BlockId::AIR);
                        carved += 1;
                    }
                }
            }
        }
        carved
    }

    /// Opens rare one-column shafts from dry surfaces into existing caves.
    ///
    /// A per-column hash picks one candidate per [`CAVE_ENTRANCE_RARITY`]
    /// columns. The candidate only becomes an entrance when a cave air block
    /// exists within [`CAVE_ENTRANCE_SCAN_DEPTH`] blocks below the normal cave
    /// seal; the shaft then removes the intervening solid blocks. This keeps
    /// the surface sealed almost everywhere while guaranteeing that every
    /// entrance actually leads to a cave.
    fn carve_surface_entrances(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) -> u32 {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut entrances = 0u32;
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let height = grid_at(columns, x, z).height;
                if height <= SEA_LEVEL + 2 {
                    continue;
                }
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                if spawn_protected(world_x, world_z) {
                    continue;
                }
                let hash = derive_seed(self.seed, i64::from(world_x), i64::from(world_z), SALT_CAVE_ENTRANCE);
                if !hash.is_multiple_of(CAVE_ENTRANCE_RARITY) {
                    continue;
                }
                if chunk.get_block(BlockPos::new(world_x, height, world_z)) == BlockId::AIR {
                    continue;
                }

                let scan_bottom = (height - CAVE_ENTRANCE_SCAN_DEPTH).max(WORLD_MIN_Y + 2);
                let mut target = None;
                for y in (scan_bottom..=height - CAVE_SURFACE_SEAL).rev() {
                    if chunk.get_block(BlockPos::new(world_x, y, world_z)) == BlockId::AIR
                        && cave_has_volume(chunk, world_x, y, world_z)
                    {
                        target = Some(y);
                        break;
                    }
                }
                let Some(target) = target else {
                    continue;
                };

                let mut blocked = false;
                for y in target..=height {
                    let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                    if block == blocks.water || block == blocks.bedrock {
                        blocked = true;
                        break;
                    }
                }
                if blocked {
                    continue;
                }
                for y in target..=height {
                    chunk.set_block(BlockPos::new(world_x, y, world_z), BlockId::AIR);
                }
                entrances += 1;
            }
        }
        entrances
    }

    /// Carves rare ravine candidates from neighbouring 128-block regions.
    ///
    /// Ravines are discovered by region hash rather than accumulated state, so
    /// every chunk that a ravine touches independently decides to carve its
    /// own columns. The one-region search margin is sufficient because a
    /// candidate's maximum reach (half-length plus half-width) is well under
    /// the 128-block region size.
    fn carve_ravines(&self, chunk: &mut ChunkColumn, columns: &ColumnGrid, blocks: &TerrainBlocks) -> u32 {
        let (origin_x, origin_z) = chunk.pos().origin();
        let chunk_region_x = chunk.pos().x.div_euclid(RAVINE_REGION_CHUNKS);
        let chunk_region_z = chunk.pos().z.div_euclid(RAVINE_REGION_CHUNKS);
        let mut carved = 0u32;
        for region_z in (chunk_region_z - 1)..=(chunk_region_z + 1) {
            for region_x in (chunk_region_x - 1)..=(chunk_region_x + 1) {
                let Some(ravine) = ravine_for_region(self.seed, region_x, region_z) else {
                    continue;
                };
                if ravine.intersects_spawn_protection() || !ravine.intersects_chunk(origin_x, origin_z) {
                    continue;
                }
                carved += self.carve_ravine(chunk, columns, blocks, ravine);
            }
        }
        carved
    }

    /// Carves one ravine's columns inside the current chunk.
    fn carve_ravine(
        &self,
        chunk: &mut ChunkColumn,
        columns: &ColumnGrid,
        blocks: &TerrainBlocks,
        ravine: Ravine,
    ) -> u32 {
        let (origin_x, origin_z) = chunk.pos().origin();
        let len_sq = i64::from(ravine.dir_x * ravine.dir_x + ravine.dir_z * ravine.dir_z);
        let half_len_sq = i64::from(ravine.half_length * ravine.half_length);
        let mut carved = 0u32;

        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let world_x = origin_x + x;
                let world_z = origin_z + z;
                let height = grid_at(columns, x, z).height;
                if height <= SEA_LEVEL + 2 || spawn_protected(world_x, world_z) {
                    continue;
                }
                let px = i64::from(world_x - ravine.center_x);
                let pz = i64::from(world_z - ravine.center_z);
                let along = px * i64::from(ravine.dir_x) + pz * i64::from(ravine.dir_z);
                let across = px * i64::from(ravine.dir_z) - pz * i64::from(ravine.dir_x);
                let along_sq = along * along;
                let across_sq = across * across;
                let bottom = (height - ravine.depth).max(WORLD_MIN_Y + 1);

                for y in (bottom..=height).rev() {
                    let depth_below = height - y;
                    let half_width = ravine_half_width(ravine, depth_below);
                    let width_sq = i64::from(half_width * half_width);
                    let limit = half_len_sq * width_sq * len_sq;
                    if along_sq * width_sq + across_sq * half_len_sq > limit {
                        continue;
                    }
                    let pos = BlockPos::new(world_x, y, world_z);
                    let current = chunk.get_block(pos);
                    if current == BlockId::AIR || current == blocks.water || current == blocks.bedrock {
                        continue;
                    }
                    chunk.set_block(pos, BlockId::AIR);
                    carved += 1;
                }
            }
        }
        carved
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

    /// Grows deterministic ore veins through stone left by the carvers.
    ///
    /// Each ore type gets its own counter-based stream derived from the world
    /// seed, the chunk's world origin and a per-ore salt. Veins are connected
    /// six-neighbour blobs seeded at a hashed stone cell inside the ore's Y
    /// band; every write first checks that the target is still stone, so the
    /// pass can never overwrite a log, leaf, water, bedrock, dirt, gravel, snow
    /// or cave-air block. It also never writes at or above the terrain surface,
    /// keeping the spawn crust and every other exposed surface intact.
    ///
    /// The vein count follows the chunk's estimated stone volume in each band
    /// instead of being fixed per chunk, so mountains and lowlands keep a
    /// comparable density. The pass runs after all features and reads no shared
    /// state, making it independent of generation order (REQ-GEN-001/004).
    fn generate_ores(
        &self,
        chunk: &mut ChunkColumn,
        columns: &ColumnGrid,
        blocks: &TerrainBlocks,
        ores: &OreBlocks,
    ) -> OreDebugStats {
        let mut stats = OreDebugStats::default();
        if blocks.stone == BlockId::AIR {
            return stats;
        }
        let (origin_x, origin_z) = chunk.pos().origin();
        for spec in &ORE_SPECS {
            let ore = ores.get(spec.kind);
            if ore == BlockId::AIR || ore == blocks.stone {
                continue;
            }
            let veins = self.ore_vein_count(spec, columns);
            if veins == 0 {
                continue;
            }
            let mut rng = Rng::new(derive_seed(
                self.seed,
                i64::from(origin_x),
                i64::from(origin_z),
                spec.salt,
            ));
            for _ in 0..veins {
                let placed = grow_ore_vein(chunk, columns, blocks.stone, ore, spec, &mut rng);
                if placed > 0 {
                    stats.record(spec.kind, placed);
                }
            }
        }
        stats
    }

    /// Number of veins to attempt for one ore in one chunk.
    ///
    /// Sums the height-grid cells inside the ore's Y band (below the terrain
    /// surface), scales by the tuned per-mille target and divides by the mean
    /// vein size. Soil and cave losses are part of the tuned factor, so a
    /// mountain chunk with twice the stone band receives roughly twice the ore
    /// blocks and the measured share of stone stays in its documented range.
    fn ore_vein_count(&self, spec: &OreSpec, columns: &ColumnGrid) -> u32 {
        let mut band_cells = 0u64;
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let height = grid_at(columns, x, z).height;
                let highest = (height - 1).min(spec.max_y);
                if highest >= spec.min_y {
                    band_cells += (highest - spec.min_y + 1) as u64;
                }
            }
        }
        let mean_size = u64::from(spec.min_size + spec.max_size) / 2;
        let target_blocks = band_cells * spec.target_per_mille / 1000;
        target_blocks.div_ceil(mean_size) as u32
    }
}

/// Grows one connected ore blob and returns the number of blocks it placed.
///
/// The first cell is drawn from the chunk's local XZ and the ore's Y band,
/// retrying [`ORE_START_ATTEMPTS`] times when the candidate is not stone (a
/// cave, water, soil or the terrain surface). Growth then repeatedly picks a
/// random placed cell, steps to one of its six neighbours, and writes the
/// neighbour only when it is inside the same chunk, inside the ore's Y band and
/// still stone. This produces a compact, always-connected vein of exactly the
/// requested size unless caves or the chunk border enclose it first; the
/// attempt cap guarantees termination. All randomness comes from the caller's
/// per-ore, per-chunk stream.
fn grow_ore_vein(
    chunk: &mut ChunkColumn,
    columns: &ColumnGrid,
    stone: BlockId,
    ore: BlockId,
    spec: &OreSpec,
    rng: &mut Rng,
) -> u32 {
    let (origin_x, origin_z) = chunk.pos().origin();
    let mut cells = [BlockPos::default(); MAX_ORE_VEIN];
    let mut placed = 0usize;

    for _ in 0..ORE_START_ATTEMPTS {
        let local_x = rng.next_below(CHUNK_SIZE as u32) as i32;
        let local_z = rng.next_below(CHUNK_SIZE as u32) as i32;
        let height = grid_at(columns, local_x, local_z).height;
        let highest = (height - 1).min(spec.max_y);
        if highest < spec.min_y {
            continue;
        }
        let span = (highest - spec.min_y + 1) as u32;
        let y = spec.min_y + rng.next_below(span) as i32;
        let seed = BlockPos::new(origin_x + local_x, y, origin_z + local_z);
        if chunk.get_block(seed) != stone {
            continue;
        }
        chunk.set_block(seed, ore);
        cells[0] = seed;
        placed = 1;
        break;
    }
    if placed == 0 {
        return 0;
    }

    let target = (spec.min_size + rng.next_below(spec.max_size - spec.min_size + 1)) as usize;
    let max_attempts = target * ORE_GROWTH_ATTEMPTS_PER_BLOCK;
    let mut attempts = 0;
    while placed < target && attempts < max_attempts {
        attempts += 1;
        let parent = cells[rng.next_below(placed as u32) as usize];
        let next = match rng.next_below(6) {
            0 => parent.offset(1, 0, 0),
            1 => parent.offset(-1, 0, 0),
            2 => parent.offset(0, 1, 0),
            3 => parent.offset(0, -1, 0),
            4 => parent.offset(0, 0, 1),
            _ => parent.offset(0, 0, -1),
        };
        if !(origin_x..origin_x + CHUNK_SIZE).contains(&next.x)
            || !(origin_z..origin_z + CHUNK_SIZE).contains(&next.z)
            || !(spec.min_y..=spec.max_y).contains(&next.y)
            || chunk.get_block(next) != stone
        {
            continue;
        }
        chunk.set_block(next, ore);
        cells[placed] = next;
        placed += 1;
    }
    placed as u32
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

/// Hashes one 3D lattice point into a fixed-point value in `[-FX_ONE, FX_ONE]`.
///
/// The three coordinate multipliers are distinct odd constants derived from
/// the splitmix64 mix, so neighbouring lattice points decorrelate and the three
/// cave fields can share the same primitive with different salts.
#[inline]
fn lattice_value_3d(seed: u64, ix: i64, iy: i64, iz: i64, salt: i64) -> i64 {
    let hash = mix64(
        seed ^ (salt as u64)
            ^ (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ (iz as u64).wrapping_mul(0x1656_67B1_9E37_79F9),
    );
    let signed = (hash >> 32) as u32 as i32;
    i64::from(signed) << 1
}

/// Trilinear smoothstep value noise at world XYZ for a lattice cell of
/// `2^shift` blocks.
fn value_noise_3d(seed: u64, x: i32, y: i32, z: i32, shift: u32, salt: i64) -> i64 {
    let cell = 1i32 << shift;
    let ix = (x >> shift) as i64;
    let iy = (y >> shift) as i64;
    let iz = (z >> shift) as i64;
    let fx = i64::from(x & (cell - 1)) << (32 - shift);
    let fy = i64::from(y & (cell - 1)) << (32 - shift);
    let fz = i64::from(z & (cell - 1)) << (32 - shift);
    let sx = smoothstep(fx);
    let sy = smoothstep(fy);
    let sz = smoothstep(fz);

    let v000 = lattice_value_3d(seed, ix, iy, iz, salt);
    let v100 = lattice_value_3d(seed, ix + 1, iy, iz, salt);
    let v010 = lattice_value_3d(seed, ix, iy + 1, iz, salt);
    let v110 = lattice_value_3d(seed, ix + 1, iy + 1, iz, salt);
    let v001 = lattice_value_3d(seed, ix, iy, iz + 1, salt);
    let v101 = lattice_value_3d(seed, ix + 1, iy, iz + 1, salt);
    let v011 = lattice_value_3d(seed, ix, iy + 1, iz + 1, salt);
    let v111 = lattice_value_3d(seed, ix + 1, iy + 1, iz + 1, salt);

    let bottom = fx_lerp(fx_lerp(v000, v100, sx), fx_lerp(v010, v110, sx), sz);
    let top = fx_lerp(fx_lerp(v001, v101, sx), fx_lerp(v011, v111, sx), sz);
    fx_lerp(bottom, top, sy)
}

/// Dyadic 3D value-noise FBM in `[-FX_ONE, FX_ONE]`.
fn fbm_3d(seed: u64, x: i32, y: i32, z: i32, base_shift: u32, octaves: u32, salt: i64) -> i64 {
    let mut sum = 0i64;
    let mut total = 0i64;
    let mut amplitude = 1i64 << (octaves - 1);
    for octave in 0..octaves {
        sum += value_noise_3d(seed, x, y, z, base_shift + octave, salt + i64::from(octave)) * amplitude;
        total += amplitude;
        amplitude >>= 1;
    }
    sum / total
}

/// Index into one coarse 3D cave grid.
#[inline]
fn cave_grid_index(x: usize, y: usize, z: usize) -> usize {
    (y * CAVE_GRID_SIZE + z) * CAVE_GRID_SIZE + x
}

/// Trilinearly blends one coarse cave grid at integer weights `0..=3`.
///
/// The weights are quarter-block offsets, so the blend is
/// `(a·(4−w) + b·w) / 4` with no `i128` intermediate and no rounding mode
/// dependence.
#[inline]
fn cave_trilinear(values: &[i64; CAVE_GRID_CELLS], x: usize, fx: i64, y: usize, fy: i64, z: usize, fz: i64) -> i64 {
    #[inline]
    fn lerp(a: i64, b: i64, weight: i64) -> i64 {
        (a * (4 - weight) + b * weight) >> 2
    }
    let c00 = lerp(
        values[cave_grid_index(x, y, z)],
        values[cave_grid_index(x + 1, y, z)],
        fx,
    );
    let c10 = lerp(
        values[cave_grid_index(x, y + 1, z)],
        values[cave_grid_index(x + 1, y + 1, z)],
        fx,
    );
    let c01 = lerp(
        values[cave_grid_index(x, y, z + 1)],
        values[cave_grid_index(x + 1, y, z + 1)],
        fx,
    );
    let c11 = lerp(
        values[cave_grid_index(x, y + 1, z + 1)],
        values[cave_grid_index(x + 1, y + 1, z + 1)],
        fx,
    );
    let bottom = lerp(c00, c01, fz);
    let top = lerp(c10, c11, fz);
    lerp(bottom, top, fy)
}

/// The depth-dependent cavern-noise threshold.
///
/// Caverns are rare near their ceiling and become progressively larger toward
/// the bottom of the world.
fn cavern_threshold(y: i32) -> i64 {
    let depth = (CAVE_CAVERN_MAX_Y - y).clamp(0, CAVE_CAVERN_TAPER);
    let range = CAVE_CAVERN_THRESHOLD_HIGH - CAVE_CAVERN_THRESHOLD_LOW;
    CAVE_CAVERN_THRESHOLD_HIGH - range * i64::from(depth) / i64::from(CAVE_CAVERN_TAPER)
}

/// Whether a cave air block is part of a usable cavity rather than a one-block
/// pocket.
///
/// A horizontal tunnel qualifies through an air neighbour at the same Y; a
/// vertical or cavern-like opening qualifies through air directly below. A
/// surface entrance is only opened into a qualifying cell.
fn cave_has_volume(chunk: &ChunkColumn, x: i32, y: i32, z: i32) -> bool {
    if y > WORLD_MIN_Y && chunk.get_block(BlockPos::new(x, y - 1, z)) == BlockId::AIR {
        return true;
    }
    let (origin_x, origin_z) = chunk.pos().origin();
    [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)].into_iter().any(|(dx, dz)| {
        let nx = x + dx;
        let nz = z + dz;
        (origin_x..origin_x + CHUNK_SIZE).contains(&nx)
            && (origin_z..origin_z + CHUNK_SIZE).contains(&nz)
            && chunk.get_block(BlockPos::new(nx, y, nz)) == BlockId::AIR
    })
}

/// Whether a world column lies inside the cave-free spawn protection square.
fn spawn_protected(x: i32, z: i32) -> bool {
    let chunk_x = x.div_euclid(CHUNK_SIZE);
    let chunk_z = z.div_euclid(CHUNK_SIZE);
    (-SPAWN_PROTECT_CHUNKS..=SPAWN_PROTECT_CHUNKS).contains(&chunk_x)
        && (-SPAWN_PROTECT_CHUNKS..=SPAWN_PROTECT_CHUNKS).contains(&chunk_z)
}

/// Derives one ravine candidate from its region coordinate, or `None` when the
/// region's hash does not select a ravine.
fn ravine_for_region(seed: u64, region_x: i32, region_z: i32) -> Option<Ravine> {
    let hash = derive_seed(seed, i64::from(region_x), i64::from(region_z), SALT_RAVINE_REGION);
    if !hash.is_multiple_of(RAVINE_RARITY) {
        return None;
    }
    let region_size = RAVINE_REGION_CHUNKS * CHUNK_SIZE;
    let origin_x = region_x * region_size;
    let origin_z = region_z * region_size;
    let direction = RAVINE_DIRECTIONS[((hash >> 24) % RAVINE_DIRECTIONS.len() as u64) as usize];
    Some(Ravine {
        center_x: origin_x + 20 + ((hash >> 8) % 88) as i32,
        center_z: origin_z + 20 + ((hash >> 16) % 88) as i32,
        dir_x: direction.0,
        dir_z: direction.1,
        half_length: 20 + ((hash >> 32) % 31) as i32,
        half_width: 3 + ((hash >> 40) % 5) as i32,
        depth: 24 + ((hash >> 48) % 29) as i32,
    })
}

/// The stepped half-width of a ravine at `depth_below` the local surface.
///
/// The width drops by one block every [`RAVINE_STEP`] blocks of depth, leaving
/// horizontal ledges between vertical wall sections.
fn ravine_half_width(ravine: Ravine, depth_below: i32) -> i32 {
    let drop = (depth_below / RAVINE_STEP).min(ravine.half_width - 1);
    (ravine.half_width - drop).max(1)
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

    /// The ore kind of a block, or `None` for every other block.
    fn ore_kind(block: BlockId, ores: &OreBlocks) -> Option<OreKind> {
        if block == ores.coal {
            Some(OreKind::Coal)
        } else if block == ores.iron {
            Some(OreKind::Iron)
        } else if block == ores.copper {
            Some(OreKind::Copper)
        } else {
            None
        }
    }

    /// Collects every ore block of a chunk in Y/Z/X scan order.
    ///
    /// The scan covers exactly the union of the three ore Y bands, so a block
    /// outside its band can never be missed by an ore test.
    fn ore_blocks_in(chunk: &ChunkColumn, ores: &OreBlocks) -> Vec<(i32, i32, i32, OreKind)> {
        let (origin_x, origin_z) = chunk.pos().origin();
        let mut blocks = Vec::new();
        for y in ORE_MIN_Y..=ORE_MAX_Y {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let block = chunk.get_block(BlockPos::new(origin_x + x, y, origin_z + z));
                    if let Some(kind) = ore_kind(block, ores) {
                        blocks.push((origin_x + x, y, origin_z + z, kind));
                    }
                }
            }
        }
        blocks
    }

    /// Generates a chunk with the ore pass disabled.
    fn generate_without_ores(generator: &Generator, pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        generator
            .generate_chunk_with_options(pos, registry, GenerationOptions::WITHOUT_ORES)
            .0
    }

    /// Times `chunks` generations with one option set.
    fn measure_generation(
        generator: &Generator,
        registry: &Registry,
        options: GenerationOptions,
        chunks: i32,
    ) -> std::time::Duration {
        let start = std::time::Instant::now();
        for index in 0..chunks {
            let result =
                generator.generate_chunk_with_options(ChunkPos::new(index % 20, index / 20), registry, options);
            std::hint::black_box(result);
        }
        start.elapsed()
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
        ores: &OreBlocks,
    ) -> Biome {
        let height = generator.height_at(x, z);
        let biome = generator.biome_at(x, z);
        let steep = world_is_steep(generator, x, z);
        let top = chunk.get_block(BlockPos::new(x, height, z));
        if top == BlockId::AIR {
            // A cave entrance or a ravine intentionally opens the surface.
            let has_cave = (height - CAVE_ENTRANCE_SCAN_DEPTH..=height - 1)
                .any(|y| chunk.get_block(BlockPos::new(x, y, z)) == BlockId::AIR);
            assert!(has_cave, "surface air at ({x}, {height}, {z}) is not above a cave");
            return biome;
        }
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
                let under = chunk.get_block(BlockPos::new(x, y, z));
                assert!(
                    under == blocks.stone || ore_kind(under, ores).is_some(),
                    "desert sand at ({x}, {height}, {z}) does not sit on stone or ore"
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
        let ores = OreBlocks::from_registry(&registry);
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
                            check_surface_column(&generator, &chunk, origin_x + x, origin_z + z, &blocks, &ores);
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
                    // A cave entrance or ravine legitimately removes the top
                    // block; the height field intentionally still reports the
                    // pre-carving terrain surface.
                    if chunk.get_block(BlockPos::new(world_x, surface, world_z)) == BlockId::AIR {
                        continue;
                    }

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
                            if chunk.get_block(BlockPos::new(world_x, height, world_z)) != BlockId::AIR {
                                assert_eq!(
                                    chunk.get_block(BlockPos::new(world_x, height, world_z)),
                                    blocks.snow,
                                    "non-steep taiga column ({world_x}, {world_z}) has no snow"
                                );
                            }
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
    fn caves_are_deterministic_and_order_independent() {
        let registry = Registry::base();
        let seed = 0x0CA7_5EED_1234_ABCD;
        let positions = [
            ChunkPos::new(0, 0),
            ChunkPos::new(1, 0),
            ChunkPos::new(-1, 2),
            ChunkPos::new(5, -3),
            ChunkPos::new(12, 7),
            ChunkPos::new(0, 1),
        ];

        let mut forward = Vec::new();
        for &pos in &positions {
            forward.push((pos, chunk_hash(&Generator::new(seed).generate_chunk(pos, &registry))));
        }

        let mut backward = Vec::new();
        for &pos in positions.iter().rev() {
            // Generate an unrelated neighbour first so any hidden shared state
            // (there must be none) has a chance to leak into the result.
            let _ = Generator::new(seed).generate_chunk(ChunkPos::new(pos.x + 9, pos.z - 4), &registry);
            backward.push((pos, chunk_hash(&Generator::new(seed).generate_chunk(pos, &registry))));
        }
        backward.reverse();
        assert_eq!(forward, backward, "generation order changed the carved blocks");

        let pos = ChunkPos::new(3, 4);
        let first = chunk_hash(&Generator::new(seed).generate_chunk(pos, &registry));
        let _ = Generator::new(seed).generate_chunk(ChunkPos::new(4, 4), &registry);
        let second = chunk_hash(&Generator::new(seed).generate_chunk(pos, &registry));
        assert_eq!(first, second, "regenerating the same chunk changed its blocks");
    }

    #[test]
    fn cave_density_is_bounded_and_bedrock_survives() {
        let registry = Registry::base();
        let bedrock = registry.id_or_air("openblock:bedrock");
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut air = 0u64;
            let mut cells = 0u64;
            let mut deep_air = 0u64;
            let mut deep_cells = 0u64;
            let mut carved_chunks = 0usize;

            for cz in -2..2 {
                for cx in -2..2 {
                    let pos = ChunkPos::new(cx, cz);
                    let (chunk, report) = generator.generate_chunk_with_report(pos, &registry);
                    if report.cave_blocks > 0 {
                        carved_chunks += 1;
                    }
                    let (origin_x, origin_z) = pos.origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            let height = generator.height_at(world_x, world_z);
                            assert_eq!(
                                chunk.get_block(BlockPos::new(world_x, WORLD_MIN_Y, world_z)),
                                bedrock,
                                "seed {seed:#x}: bedrock was carved at ({world_x}, {world_z})"
                            );

                            let top = chunk.get_block(BlockPos::new(world_x, height, world_z));
                            if top != BlockId::AIR {
                                assert_ne!(
                                    chunk.get_block(BlockPos::new(world_x, height - 1, world_z)),
                                    BlockId::AIR,
                                    "seed {seed:#x}: cave undercut the top block at ({world_x}, {world_z})"
                                );
                                assert_ne!(
                                    chunk.get_block(BlockPos::new(world_x, height - 2, world_z)),
                                    BlockId::AIR,
                                    "seed {seed:#x}: cave undercut the second block at ({world_x}, {world_z})"
                                );
                            }

                            for y in (WORLD_MIN_Y + 2)..=(height - 4) {
                                cells += 1;
                                if chunk.get_block(BlockPos::new(world_x, y, world_z)) == BlockId::AIR {
                                    air += 1;
                                    if y <= 0 {
                                        deep_air += 1;
                                    }
                                }
                                if y <= 0 {
                                    deep_cells += 1;
                                }
                            }
                        }
                    }
                }
            }

            assert_eq!(carved_chunks, 16, "seed {seed:#x}: a sampled chunk had no caves");
            assert!(air * 100 > cells, "seed {seed:#x}: cave air is under 1%");
            assert!(air * 100 < cells * 40, "seed {seed:#x}: cave air is over 40%");
            assert!(deep_cells > 0 && deep_air > 0, "seed {seed:#x}: no deep cave air");
            assert!(
                deep_air * 100 < deep_cells * 40,
                "seed {seed:#x}: deep cave air is over 40%"
            );
            println!(
                "seed {seed:#x}: cave air {:.2}% of the stone band, {:.2}% below y=0",
                air as f64 * 100.0 / cells as f64,
                deep_air as f64 * 100.0 / deep_cells as f64
            );
        }
    }

    #[test]
    fn surface_entrances_connect_to_caves() {
        let registry = Registry::base();
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut found = None;
            'search: for cz in -16..16 {
                for cx in -16..16 {
                    let pos = ChunkPos::new(cx, cz);
                    let (chunk, report) = generator.generate_chunk_with_report(pos, &registry);
                    if report.entrance_columns > 0 {
                        found = Some((pos, chunk, report));
                        break 'search;
                    }
                }
            }

            let (pos, chunk, report) =
                found.unwrap_or_else(|| panic!("seed {seed:#x}: no surface entrance in 32x32 chunks"));
            let (origin_x, origin_z) = pos.origin();
            let mut verified = 0usize;
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    let height = generator.height_at(world_x, world_z);
                    if height <= SEA_LEVEL + 2
                        || chunk.get_block(BlockPos::new(world_x, height, world_z)) != BlockId::AIR
                    {
                        continue;
                    }
                    let connected = (height - CAVE_ENTRANCE_SCAN_DEPTH..=height - 1)
                        .any(|y| chunk.get_block(BlockPos::new(world_x, y, world_z)) == BlockId::AIR);
                    if connected {
                        verified += 1;
                    }
                }
            }
            assert!(
                verified > 0,
                "seed {seed:#x}: entrance report at {pos:?} has no connected surface hole"
            );
            println!(
                "seed {seed:#x}: {} entrance columns, {verified} verified in {pos:?}",
                report.entrance_columns
            );
        }
    }

    #[test]
    fn ravines_are_findable_deep_and_stepped() {
        let registry = Registry::base();
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut chosen = None;
            'search: for region_z in -16..16 {
                for region_x in -16..16 {
                    let Some(ravine) = ravine_for_region(seed, region_x, region_z) else {
                        continue;
                    };
                    if ravine.intersects_spawn_protection() {
                        continue;
                    }
                    let height = generator.height_at(ravine.center_x, ravine.center_z);
                    if height <= SEA_LEVEL + 2 {
                        continue;
                    }
                    chosen = Some((ravine, height));
                    break 'search;
                }
            }

            let (ravine, height) =
                chosen.unwrap_or_else(|| panic!("seed {seed:#x}: no dry ravine candidate in 1024 regions"));
            assert!(
                ravine_half_width(ravine, 0) > ravine_half_width(ravine, 12),
                "seed {seed:#x}: ravine walls are not stepped"
            );
            assert!(
                ravine_half_width(ravine, 12) >= ravine_half_width(ravine, 36),
                "seed {seed:#x}: ravine walls widen with depth"
            );

            let pos = ChunkPos::new(
                ravine.center_x.div_euclid(CHUNK_SIZE),
                ravine.center_z.div_euclid(CHUNK_SIZE),
            );
            let (chunk, report) = generator.generate_chunk_with_report(pos, &registry);
            assert!(
                report.ravine_blocks > 0,
                "seed {seed:#x}: candidate chunk reported no ravine blocks"
            );
            assert_eq!(
                chunk.get_block(BlockPos::new(ravine.center_x, height, ravine.center_z)),
                BlockId::AIR,
                "seed {seed:#x}: ravine is not open to the sky"
            );
            assert_eq!(
                chunk.get_block(BlockPos::new(ravine.center_x, height - 10, ravine.center_z)),
                BlockId::AIR,
                "seed {seed:#x}: ravine is not deep"
            );
            println!(
                "seed {seed:#x}: ravine at ({}, {}) depth {} half-width {}->{}, {} blocks carved in {pos:?}",
                ravine.center_x,
                ravine.center_z,
                ravine.depth,
                ravine_half_width(ravine, 0),
                ravine_half_width(ravine, 36),
                report.ravine_blocks
            );
        }
    }

    #[test]
    fn carving_never_leaves_water_over_air() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        let generator = Generator::new(0xC0FF_EE00_1234_5678);
        let ocean = find_chunk_with_height(&generator, 8, |height| height < SEA_LEVEL)
            .expect("expected an ocean chunk within 17 chunks");
        let mut checked = 0usize;

        for dz in -1..=1 {
            for dx in -1..=1 {
                let pos = ChunkPos::new(ocean.x + dx, ocean.z + dz);
                let chunk = generator.generate_chunk(pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let height = generator.height_at(world_x, world_z);
                        if height >= SEA_LEVEL {
                            continue;
                        }
                        for y in (height + 1)..=SEA_LEVEL {
                            if chunk.get_block(BlockPos::new(world_x, y, world_z)) != water {
                                continue;
                            }
                            checked += 1;
                            assert_ne!(
                                chunk.get_block(BlockPos::new(world_x, y - 1, world_z)),
                                BlockId::AIR,
                                "water at ({world_x}, {y}, {world_z}) floats over carved air"
                            );
                        }
                    }
                }
            }
        }
        assert!(checked > 0, "ocean sample contained no water");
    }

    #[test]
    fn spawn_area_keeps_a_solid_crust() {
        let registry = Registry::base();
        for seed in [1u64, 42, 0xC0FF_EE00_1234_5678] {
            let generator = Generator::new(seed);
            for cz in -SPAWN_PROTECT_CHUNKS..=SPAWN_PROTECT_CHUNKS {
                for cx in -SPAWN_PROTECT_CHUNKS..=SPAWN_PROTECT_CHUNKS {
                    let pos = ChunkPos::new(cx, cz);
                    let (chunk, report) = generator.generate_chunk_with_report(pos, &registry);
                    assert_eq!(
                        report.entrance_columns, 0,
                        "seed {seed:#x}: cave entrance inside spawn protection at {pos:?}"
                    );
                    assert_eq!(
                        report.ravine_blocks, 0,
                        "seed {seed:#x}: ravine inside spawn protection at {pos:?}"
                    );

                    let (origin_x, origin_z) = pos.origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            let height = generator.height_at(world_x, world_z);
                            for y in (height - SPAWN_PROTECT_DEPTH + 1)..=height {
                                assert_ne!(
                                    chunk.get_block(BlockPos::new(world_x, y, world_z)),
                                    BlockId::AIR,
                                    "seed {seed:#x}: spawn crust at ({world_x}, {y}, {world_z}) is air"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ores_are_deterministic_and_order_independent() {
        let registry = Registry::base();
        let ores = OreBlocks::from_registry(&registry);
        let seed = 0x0E5E_1234_ABCD_9876;
        let positions = [
            ChunkPos::new(0, 0),
            ChunkPos::new(1, 0),
            ChunkPos::new(-1, 2),
            ChunkPos::new(5, -3),
            ChunkPos::new(12, 7),
            ChunkPos::new(0, 1),
        ];

        let forward: Vec<_> = positions
            .iter()
            .map(|&pos| {
                let chunk = Generator::new(seed).generate_chunk(pos, &registry);
                let found = ore_blocks_in(&chunk, &ores);
                assert!(!found.is_empty(), "seed {seed:#x}: {pos:?} contains no ore");
                (pos, found)
            })
            .collect();

        // Generate in reverse order, with an unrelated neighbour in between, so
        // any hidden shared state would show up as a different vein layout.
        let mut backward: Vec<_> = positions
            .iter()
            .rev()
            .map(|&pos| {
                let _ = Generator::new(seed).generate_chunk(ChunkPos::new(pos.x + 9, pos.z - 4), &registry);
                let chunk = Generator::new(seed).generate_chunk(pos, &registry);
                (pos, ore_blocks_in(&chunk, &ores))
            })
            .collect();
        backward.reverse();
        assert_eq!(forward, backward, "generation order changed ore veins");

        let pos = ChunkPos::new(3, 4);
        let first = ore_blocks_in(&Generator::new(seed).generate_chunk(pos, &registry), &ores);
        let _ = Generator::new(seed).generate_chunk(ChunkPos::new(4, 4), &registry);
        let second = ore_blocks_in(&Generator::new(seed).generate_chunk(pos, &registry), &ores);
        assert_eq!(first, second, "regenerating a chunk changed its ore veins");

        let other = ore_blocks_in(
            &Generator::new(seed ^ 0xA5A5_A5A5_A5A5_A5A5).generate_chunk(pos, &registry),
            &ores,
        );
        assert_ne!(first, other, "different seeds produced identical ore veins");

        // The public diagnostic helper must agree with a fresh scan.
        let stats = Generator::new(seed).ore_debug_stats(positions[0], &registry);
        let count = |kind: OreKind| forward[0].1.iter().filter(|(_, _, _, found)| *found == kind).count() as u32;
        assert_eq!(stats.coal_blocks, count(OreKind::Coal));
        assert_eq!(stats.iron_blocks, count(OreKind::Iron));
        assert_eq!(stats.copper_blocks, count(OreKind::Copper));
        assert!(stats.coal_veins > 0 && stats.iron_veins > 0 && stats.copper_veins > 0);

        println!(
            "seed {seed:#x}: {} ore blocks checked over {} chunks, stats {stats:?}",
            forward.iter().map(|(_, found)| found.len()).sum::<usize>(),
            positions.len()
        );
    }

    #[test]
    fn all_three_ores_appear_with_expected_depth_ranges() {
        const SIZE: i32 = 24;
        let registry = Registry::base();
        let ores = OreBlocks::from_registry(&registry);
        let generator = Generator::new(42);
        let mut counts = [0u64; 3];
        let mut min_seen = [i32::MAX; 3];
        let mut max_seen = [i32::MIN; 3];
        let mut chunks_with_ore = [0u64; 3];
        let mut chunks = 0u64;

        for cz in 0..SIZE {
            for cx in 0..SIZE {
                chunks += 1;
                let chunk = generator.generate_chunk(ChunkPos::new(cx, cz), &registry);
                let mut present = [false; 3];
                for (x, y, z, kind) in ore_blocks_in(&chunk, &ores) {
                    let index = kind.index();
                    let spec = &ORE_SPECS[index];
                    assert!(
                        (spec.min_y..=spec.max_y).contains(&y),
                        "{} ore at ({x}, {y}, {z}) is outside its {}..={} band",
                        kind.name(),
                        spec.min_y,
                        spec.max_y
                    );
                    counts[index] += 1;
                    present[index] = true;
                    min_seen[index] = min_seen[index].min(y);
                    max_seen[index] = max_seen[index].max(y);
                }
                for (index, found) in present.into_iter().enumerate() {
                    if found {
                        chunks_with_ore[index] += 1;
                    }
                }
            }
        }

        assert!(chunks > 0);
        for kind in OreKind::ALL {
            let index = kind.index();
            assert!(
                counts[index] > 0,
                "{} ore never appeared in {SIZE}x{SIZE} chunks",
                kind.name()
            );
            assert!(
                chunks_with_ore[index] * 10 >= chunks * 9,
                "{} ore appeared in only {} of {chunks} chunks",
                kind.name(),
                chunks_with_ore[index]
            );
            println!(
                "{}: {} blocks in {} of {chunks} chunks, observed Y {}..={} (configured {}..={})",
                kind.name(),
                counts[index],
                chunks_with_ore[index],
                min_seen[index],
                max_seen[index],
                ORE_SPECS[index].min_y,
                ORE_SPECS[index].max_y
            );
        }
    }

    #[test]
    fn no_ore_replaces_non_stone_or_floats_in_caves() {
        let registry = Registry::base();
        let ores = OreBlocks::from_registry(&registry);
        let blocks = TerrainBlocks::from_registry(&registry);
        let generator = Generator::new(0xC0FF_EE00_1234_5678);
        let mut checked = 0u64;

        for cz in -2..2 {
            for cx in -2..2 {
                let pos = ChunkPos::new(cx, cz);
                let with_ores = generator.generate_chunk(pos, &registry);
                let without_ores = generate_without_ores(&generator, pos, &registry);
                let (origin_x, origin_z) = pos.origin();
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        let world_x = origin_x + x;
                        let world_z = origin_z + z;
                        let height = generator.height_at(world_x, world_z);
                        let top = BlockPos::new(world_x, height, world_z);
                        assert_eq!(
                            with_ores.get_block(top),
                            without_ores.get_block(top),
                            "ore changed the terrain surface at ({world_x}, {height}, {world_z})"
                        );
                        for y in ORE_MIN_Y..=ORE_MAX_Y {
                            let block_pos = BlockPos::new(world_x, y, world_z);
                            let block = with_ores.get_block(block_pos);
                            if ore_kind(block, &ores).is_none() {
                                continue;
                            }
                            checked += 1;
                            assert_eq!(
                                without_ores.get_block(block_pos),
                                blocks.stone,
                                "ore at ({world_x}, {y}, {world_z}) did not replace stone"
                            );
                            assert!(
                                y < height,
                                "ore at ({world_x}, {y}, {world_z}) is not below the {} surface",
                                height
                            );
                        }
                    }
                }
            }
        }
        assert!(checked > 1_000, "only {checked} ore blocks were inspected");
        println!("checked {checked} ore blocks against a no-ore generation pass");
    }

    #[test]
    fn ore_density_is_bounded() {
        const SIZE: i32 = 12;
        // Documented density ranges, in blocks per thousand pre-ore stone.
        const BOUNDS: [(u64, u64); 3] = [(8, 12), (4, 7), (3, 6)];
        let registry = Registry::base();
        let ores = OreBlocks::from_registry(&registry);
        let blocks = TerrainBlocks::from_registry(&registry);

        for seed in [1u64, 42, 0xC0FF_EE00_1234_5678] {
            let generator = Generator::new(seed);
            let mut ore_blocks = [0u64; 3];
            let mut stone_blocks = [0u64; 3];
            let mut band_ore_blocks = [0u64; 3];
            for cz in -(SIZE / 2)..(SIZE / 2) {
                for cx in -(SIZE / 2)..(SIZE / 2) {
                    let chunk = generator.generate_chunk(ChunkPos::new(cx, cz), &registry);
                    let (origin_x, origin_z) = chunk.pos().origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            for y in ORE_MIN_Y..=ORE_MAX_Y {
                                let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                                if let Some(kind) = ore_kind(block, &ores) {
                                    ore_blocks[kind.index()] += 1;
                                    for (index, spec) in ORE_SPECS.iter().enumerate() {
                                        if (spec.min_y..=spec.max_y).contains(&y) {
                                            band_ore_blocks[index] += 1;
                                        }
                                    }
                                } else if block == blocks.stone {
                                    for (index, spec) in ORE_SPECS.iter().enumerate() {
                                        if (spec.min_y..=spec.max_y).contains(&y) {
                                            stone_blocks[index] += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for kind in OreKind::ALL {
                let index = kind.index();
                let pre_ore_stone = stone_blocks[index] + band_ore_blocks[index];
                assert!(
                    pre_ore_stone > 0,
                    "seed {seed:#x}: no stone in the {} band",
                    kind.name()
                );
                let permille = ore_blocks[index] * 1000 / pre_ore_stone;
                assert!(
                    (BOUNDS[index].0..=BOUNDS[index].1).contains(&permille),
                    "seed {seed:#x}: {} density is {permille} per mille \
                     ({} ore / {pre_ore_stone} pre-ore stone), expected {}..={}",
                    kind.name(),
                    ore_blocks[index],
                    BOUNDS[index].0,
                    BOUNDS[index].1
                );
                println!(
                    "seed {seed:#x}: {} {permille} per mille ({:.2}%), {} ore blocks in {}x{SIZE} chunks",
                    kind.name(),
                    permille as f64 / 10.0,
                    ore_blocks[index],
                    SIZE
                );
            }
        }
    }

    #[test]
    #[ignore = "ore throughput benchmark; run with --ignored --nocapture"]
    fn ore_throughput_report() {
        let registry = Registry::base();
        let chunks = 200i32;
        for seed in [1u64, 42, 0xC0FF_EE00_1234_5678] {
            let generator = Generator::new(seed);
            // Warm both paths before timing.
            let _ = std::hint::black_box(generator.generate_chunk(ChunkPos::new(0, 0), &registry));
            let _ = std::hint::black_box(generator.generate_chunk_with_options(
                ChunkPos::new(0, 0),
                &registry,
                GenerationOptions::WITHOUT_ORES,
            ));

            let mut with = std::time::Duration::ZERO;
            let mut without = std::time::Duration::ZERO;
            for round in 0..2 {
                if round == 0 {
                    without += measure_generation(&generator, &registry, GenerationOptions::WITHOUT_ORES, chunks);
                    with += measure_generation(&generator, &registry, GenerationOptions::WITH_ORES, chunks);
                } else {
                    with += measure_generation(&generator, &registry, GenerationOptions::WITH_ORES, chunks);
                    without += measure_generation(&generator, &registry, GenerationOptions::WITHOUT_ORES, chunks);
                }
            }
            let samples = f64::from(chunks) * 2.0;
            let with_ms = with.as_secs_f64() * 1000.0 / samples;
            let without_ms = without.as_secs_f64() * 1000.0 / samples;
            let added_ms = with_ms - without_ms;
            println!(
                "seed {seed:#x}: {chunks} chunks, with ores {with_ms:.3} ms/chunk, \
                 without {without_ms:.3} ms/chunk, added {added_ms:+.3} ms/chunk"
            );
            assert!(
                added_ms <= 0.35,
                "seed {seed:#x}: ore generation added {added_ms:.3} ms/chunk (target <= 0.35)"
            );
            assert!(
                with_ms < 5.0,
                "seed {seed:#x}: generation averaged {with_ms:.3} ms/chunk"
            );
        }
    }

    #[test]
    #[ignore = "ore distribution report; run with --ignored --nocapture"]
    fn ore_distribution_report() {
        const SIZE: i32 = 24;
        let registry = Registry::base();
        let ores = OreBlocks::from_registry(&registry);
        let blocks = TerrainBlocks::from_registry(&registry);
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut ore_blocks = [0u64; 3];
            let mut stone_blocks = [0u64; 3];
            let mut band_ore_blocks = [0u64; 3];
            let mut veins = [0u64; 3];
            let mut min_seen = [i32::MAX; 3];
            let mut max_seen = [i32::MIN; 3];
            let mut chunks_with_ore = [0u64; 3];
            let mut sampled_hash = 0u64;
            let mut chunks = 0u64;

            for cz in -(SIZE / 2)..(SIZE / 2) {
                for cx in -(SIZE / 2)..(SIZE / 2) {
                    let pos = ChunkPos::new(cx, cz);
                    let (chunk, _, stats) =
                        generator.generate_chunk_with_options(pos, &registry, GenerationOptions::WITH_ORES);
                    chunks += 1;
                    if pos == ChunkPos::new(0, 0) {
                        sampled_hash = chunk_hash(&chunk);
                    }
                    veins[0] += u64::from(stats.coal_veins);
                    veins[1] += u64::from(stats.iron_veins);
                    veins[2] += u64::from(stats.copper_veins);
                    let mut present = [false; 3];
                    let (origin_x, origin_z) = pos.origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            for y in ORE_MIN_Y..=ORE_MAX_Y {
                                let block = chunk.get_block(BlockPos::new(world_x, y, world_z));
                                if let Some(kind) = ore_kind(block, &ores) {
                                    let index = kind.index();
                                    ore_blocks[index] += 1;
                                    present[index] = true;
                                    min_seen[index] = min_seen[index].min(y);
                                    max_seen[index] = max_seen[index].max(y);
                                    for (band, spec) in ORE_SPECS.iter().enumerate() {
                                        if (spec.min_y..=spec.max_y).contains(&y) {
                                            band_ore_blocks[band] += 1;
                                        }
                                    }
                                } else if block == blocks.stone {
                                    for (index, spec) in ORE_SPECS.iter().enumerate() {
                                        if (spec.min_y..=spec.max_y).contains(&y) {
                                            stone_blocks[index] += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for (index, found) in present.into_iter().enumerate() {
                        if found {
                            chunks_with_ore[index] += 1;
                        }
                    }
                }
            }

            for kind in OreKind::ALL {
                let index = kind.index();
                let pre_ore_stone = stone_blocks[index] + band_ore_blocks[index];
                let permille = ore_blocks[index] * 1000 / pre_ore_stone;
                let density = ore_blocks[index] as f64 * 100.0 / pre_ore_stone as f64;
                let mean_vein = ore_blocks[index] as f64 / veins[index].max(1) as f64;
                println!(
                    "seed {seed:#x}: {:>6} {density:.2}% ({permille} per mille), \
                     {:.1} blocks/chunk, {:.1} veins/chunk, {mean_vein:.1} blocks/vein, \
                     {} of {chunks} chunks, Y {}..={}",
                    kind.name(),
                    ore_blocks[index] as f64 / chunks as f64,
                    veins[index] as f64 / chunks as f64,
                    chunks_with_ore[index],
                    min_seen[index],
                    max_seen[index],
                );
            }
            println!(
                "seed {seed:#x}: hash(0,0)={sampled_hash:#018x}, {} ore blocks from {} veins in {chunks} chunks",
                ore_blocks.iter().sum::<u64>(),
                veins.iter().sum::<u64>()
            );
        }
    }

    #[test]
    #[ignore = "throughput benchmark; run with --ignored --nocapture"]
    fn cave_throughput_report() {
        let registry = Registry::base();
        for seed in [1u64, 42, 0xC0FF_EE00_1234_5678] {
            let generator = Generator::new(seed);
            let warm = generator.generate_chunk(ChunkPos::new(0, 0), &registry);
            let hash = chunk_hash(&warm);
            let _ = std::hint::black_box(warm);

            let chunks = 200i32;
            let start = std::time::Instant::now();
            let mut worst = std::time::Duration::ZERO;
            for index in 0..chunks {
                let chunk_start = std::time::Instant::now();
                let _ =
                    std::hint::black_box(generator.generate_chunk(ChunkPos::new(index % 20, index / 20), &registry));
                worst = worst.max(chunk_start.elapsed());
            }
            let elapsed = start.elapsed();
            let millis = elapsed.as_secs_f64() * 1000.0 / f64::from(chunks);
            println!(
                "seed {seed:#x}: {chunks} chunks in {elapsed:?} ({millis:.3} ms/chunk, worst {worst:?}), \
                 hash(0,0)={hash:#018x}, stats={:?}",
                generator.cave_debug_stats(ChunkPos::new(0, 0), &registry)
            );
            assert!(
                millis < 5.0,
                "cave generation averaged {millis:.3} ms/chunk for seed {seed:#x}"
            );
        }
    }

    /// FNV-1a hash of a chunk's blocks, used by determinism tests and reports.
    fn chunk_hash(chunk: &ChunkColumn) -> u64 {
        let mut hash = 0xCBF2_9CE4_8422_2325u64;
        for block in collect_blocks(chunk) {
            hash ^= u64::from(block.0);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }

    #[test]
    #[ignore = "cave distribution report; run with --ignored --nocapture"]
    fn cave_distribution_report() {
        let registry = Registry::base();
        let water = registry.id_or_air("openblock:water");
        for seed in [1u64, 42] {
            let generator = Generator::new(seed);
            let mut stats = CaveDebugStats::default();
            let mut air = 0u64;
            let mut cells = 0u64;
            let mut deep_air = 0u64;
            let mut deep_cells = 0u64;
            let mut upper_air = 0u64;
            let mut upper_cells = 0u64;
            let mut bedrock_air = 0u64;
            let mut surface_holes = 0u64;
            let mut undercut_tops = 0u64;
            let mut unsupported_water = 0u64;
            let mut ravine_chunks = 0u64;
            for cz in -12..12 {
                for cx in -12..12 {
                    let pos = ChunkPos::new(cx, cz);
                    let (chunk, report) = generator.generate_chunk_with_report(pos, &registry);
                    stats.cave_blocks += report.cave_blocks;
                    stats.entrance_columns += report.entrance_columns;
                    stats.ravine_blocks += report.ravine_blocks;
                    if report.ravine_blocks > 0 {
                        ravine_chunks += 1;
                    }
                    let (origin_x, origin_z) = pos.origin();
                    for z in 0..CHUNK_SIZE {
                        for x in 0..CHUNK_SIZE {
                            let world_x = origin_x + x;
                            let world_z = origin_z + z;
                            let height = generator.height_at(world_x, world_z);
                            if chunk.get_block(BlockPos::new(world_x, height, world_z)) == BlockId::AIR {
                                surface_holes += 1;
                            }
                            if chunk.get_block(BlockPos::new(world_x, height, world_z)) != BlockId::AIR
                                && (chunk.get_block(BlockPos::new(world_x, height - 1, world_z)) == BlockId::AIR
                                    || chunk.get_block(BlockPos::new(world_x, height - 2, world_z)) == BlockId::AIR)
                            {
                                undercut_tops += 1;
                            }
                            for y in (WORLD_MIN_Y + 2)..=(height - 4) {
                                cells += 1;
                                if chunk.get_block(BlockPos::new(world_x, y, world_z)) == BlockId::AIR {
                                    air += 1;
                                    if y <= 0 {
                                        deep_air += 1;
                                    }
                                }
                                if y <= 0 {
                                    deep_cells += 1;
                                } else if y > CAVE_CAVERN_MAX_Y {
                                    upper_cells += 1;
                                    if chunk.get_block(BlockPos::new(world_x, y, world_z)) == BlockId::AIR {
                                        upper_air += 1;
                                    }
                                }
                            }
                            if chunk.get_block(BlockPos::new(world_x, WORLD_MIN_Y, world_z)) == BlockId::AIR {
                                bedrock_air += 1;
                            }
                            if height < SEA_LEVEL {
                                for y in (height + 1)..=SEA_LEVEL {
                                    if chunk.get_block(BlockPos::new(world_x, y, world_z)) == water
                                        && chunk.get_block(BlockPos::new(world_x, y - 1, world_z)) == BlockId::AIR
                                    {
                                        unsupported_water += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            println!(
                "seed {seed:#x}: air {:.2}% ({air}/{cells}), tunnels-above-y{CAVE_CAVERN_MAX_Y} {:.2}% \
                 ({upper_air}/{upper_cells}), deep y<=0 {:.2}% ({deep_air}/{deep_cells}), \
                 cave_blocks={}, entrances={}, ravine_blocks={} in {ravine_chunks} chunks, \
                 surface_holes={surface_holes}, undercut_tops={undercut_tops}, bedrock_air={bedrock_air}, \
                 unsupported_water={unsupported_water}, hash(0,0)={:#018x}",
                air as f64 * 100.0 / cells as f64,
                upper_air as f64 * 100.0 / upper_cells as f64,
                deep_air as f64 * 100.0 / deep_cells as f64,
                stats.cave_blocks,
                stats.entrance_columns,
                stats.ravine_blocks,
                chunk_hash(&generator.generate_chunk(ChunkPos::new(0, 0), &registry)),
            );
        }
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

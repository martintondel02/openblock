//! Visual-only far terrain: a bounded, deterministic heightfield mesh.
//!
//! The simulated and fully meshed world stops at the client's simulation
//! distance (at most 32 chunks / 512 blocks). This module extends the horizon
//! visually to up to 512 chunks (8,192 blocks) with a low-poly heightfield
//! built from a cheap surface sampler supplied by the caller. The renderer never
//! learns about worldgen types: the client (or a benchmark) hands over world
//! space vertices, indices and a surface colour, exactly like a regular mesh.
//!
//! The geometry is split into a fixed ladder of concentric tiers:
//!
//! | tier | nominal radius (blocks) | cell (blocks) |
//! |------|-------------------------|---------------|
//! | 0 | inner .. 512 | 8 |
//! | 1 | 512 .. 1,024 | 16 |
//! | 2 | 1,024 .. 2,048 | 32 |
//! | 3 | 2,048 .. 4,096 | 64 |
//! | 4 | 4,096 .. outer | 128 |
//!
//! `inner` is the simulation radius minus the recenter hysteresis so the
//! near world always overlaps the far mesh even while the camera moves between
//! rebuilds. Each tier is sampled on a world-aligned lattice and extended by
//! 1.5 cells past its nominal bounds: adjacent tiers overlap, which is what
//! guarantees there is no crack at a LOD boundary. Farther tiers are offset a
//! few centimetres lower so the finer tier wins the depth test in the overlap.
//!
//! The mesh is visual only. It is never consulted for collision, spawning,
//! streaming, persistence or anti-cheat, and it deliberately omits caves,
//! trees, structures and every other generated feature.

use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use ob_math::mix64;
use rayon::prelude::*;

/// Outer nominal radii of the fixed LOD ladder, in blocks. The last tier runs
/// from the final boundary to the configured view distance.
pub const FAR_TIER_BOUNDARIES: [f32; 4] = [512.0, 1024.0, 2048.0, 4096.0];

/// Heightfield cell size of each tier, in blocks.
pub const FAR_TIER_CELLS: [f32; 5] = [8.0, 16.0, 32.0, 64.0, 128.0];

/// Camera movement before the far mesh is recentered, in blocks.
///
/// This is deliberately large enough that ordinary walking does not rebuild
/// the mesh, and smaller than the near-world overlap so holes cannot appear
/// while the rebuild is in flight.
pub const FAR_RECENTER_HYSTERESIS: f32 = 96.0;

/// Extra overlap beyond a tier's nominal bounds, in tier cells.
pub const FAR_TIER_OVERLAP_CELLS: f32 = 1.5;

/// Constant down-offset of the first far tier, in blocks.
///
/// The near world owns the volume near the camera. Lowering the far surface
/// keeps it inside the terrain blocks and makes the fine tier win where two
/// far tiers overlap, without a visible step at the horizon.
pub const FAR_SURFACE_OFFSET: f32 = 0.65;

/// Additional down-offset per outer tier, in blocks (depth-test tie-break).
pub const FAR_TIER_OFFSET_STEP: f32 = 0.15;

/// Far-water surface rise above the near sea level, in blocks.
pub const FAR_WATER_RISE: f32 = 0.05;

/// One visual surface biome, deliberately independent of `ob_worldgen` types.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum FarBiome {
    /// Grass lowland.
    #[default]
    Plains,
    /// Grass lowland with darker woodland colour.
    Forest,
    /// Sand desert.
    Desert,
    /// Snowy taiga.
    SnowyTaiga,
    /// Rock and snow highland.
    Mountains,
}

/// One fast column sample supplied to the far-terrain builder.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FarSample {
    /// Y of the topmost terrain block (same convention as the real world).
    pub height: i32,
    /// Dominant surface biome.
    pub biome: FarBiome,
}

/// One far-terrain render vertex: world position, linear albedo and normal.
///
/// The normal is the heightfield normal baked at build time; the fragment
/// shader applies the current sun direction, so day/night changes do not
/// rebuild the mesh.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct FarTerrainVertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Linear RGB surface colour.
    pub color: [f32; 3],
    /// Unit heightfield normal.
    pub normal: [f32; 3],
}

// Keep the vertex layout and the far shader attribute offsets in sync.
const _: () = assert!(std::mem::size_of::<FarTerrainVertex>() == 36);

/// One tier of the far LOD ladder, in world blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FarTier {
    /// Inner nominal radius.
    pub inner: f32,
    /// Outer nominal radius.
    pub outer: f32,
    /// Grid cell size.
    pub cell: f32,
}

/// Configuration of one far-terrain build.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FarTerrainConfig {
    /// Visual radius in chunks (32..=512).
    pub view_distance_chunks: i32,
    /// Simulated/full-detail radius in chunks (2..=32).
    pub simulation_distance_chunks: i32,
    /// Camera movement before a rebuild, in blocks.
    pub hysteresis: f32,
    /// Water surface height in world blocks (the top face of the sea).
    pub sea_level: f32,
}

impl FarTerrainConfig {
    /// Creates a config with the standard hysteresis.
    pub fn new(view_distance_chunks: i32, simulation_distance_chunks: i32, sea_level: f32) -> Self {
        Self {
            view_distance_chunks,
            simulation_distance_chunks,
            hysteresis: FAR_RECENTER_HYSTERESIS,
            sea_level,
        }
    }

    /// Clamps radii and hysteresis to sane bounds.
    pub fn normalised(self) -> Self {
        let simulation = self.simulation_distance_chunks.clamp(1, 32);
        Self {
            view_distance_chunks: self.view_distance_chunks.clamp(32, 512).max(simulation),
            simulation_distance_chunks: simulation,
            hysteresis: if self.hysteresis.is_finite() {
                self.hysteresis.clamp(32.0, 192.0)
            } else {
                FAR_RECENTER_HYSTERESIS
            },
            sea_level: if self.sea_level.is_finite() {
                self.sea_level
            } else {
                63.0
            },
        }
    }

    /// Visual radius in blocks.
    pub fn view_distance_blocks(&self) -> f32 {
        self.view_distance_chunks as f32 * 16.0
    }

    /// Simulated radius in blocks.
    pub fn simulation_distance_blocks(&self) -> f32 {
        self.simulation_distance_chunks as f32 * 16.0
    }

    /// Inner radius of the far mesh. The near world overlaps this by roughly
    /// the hysteresis, so no rebuild-in-flight movement can expose a hole.
    pub fn inner_radius(&self) -> f32 {
        (self.simulation_distance_blocks() - self.hysteresis).max(0.0)
    }

    /// Outer radius of the far mesh. The extra hysteresis keeps the horizon
    /// covered while the mesh is one rebuild behind the camera.
    pub fn outer_radius(&self) -> f32 {
        self.view_distance_blocks() + self.hysteresis
    }

    /// The fixed LOD ladder truncated at the configured outer radius.
    pub fn tiers(&self) -> Vec<FarTier> {
        let config = self.normalised();
        let inner = config.inner_radius();
        let outer = config.outer_radius();
        let mut tiers = Vec::with_capacity(FAR_TIER_CELLS.len());
        let mut start = inner;
        for (index, cell) in FAR_TIER_CELLS.iter().copied().enumerate() {
            if start >= outer {
                break;
            }
            let boundary = FAR_TIER_BOUNDARIES.get(index).copied().unwrap_or(outer);
            let end = boundary.min(outer);
            if end > start {
                tiers.push(FarTier {
                    inner: start,
                    outer: end,
                    cell,
                });
            }
            if end >= outer {
                break;
            }
            start = end;
        }
        tiers
    }
}

/// Counters for one far-terrain build.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FarTerrainStats {
    /// Camera XZ the mesh was centred on.
    pub center: [f64; 2],
    /// Opaque quads emitted.
    pub opaque_quads: u64,
    /// Opaque vertices emitted.
    pub opaque_vertices: u64,
    /// Water triangles emitted.
    pub water_triangles: u64,
    /// Water vertices emitted.
    pub water_vertices: u64,
    /// Wall time spent building (sampling plus assembly).
    pub build_ms: f64,
    /// Per-tier counters, nearest first.
    pub tiers: Vec<FarTierStats>,
}

/// Counters for one LOD tier.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FarTierStats {
    /// Inner nominal radius.
    pub inner: f32,
    /// Outer nominal radius.
    pub outer: f32,
    /// Grid cell size.
    pub cell: f32,
    /// Opaque quads.
    pub opaque_quads: u64,
    /// Opaque vertices.
    pub opaque_vertices: u64,
    /// Water triangles.
    pub water_triangles: u64,
    /// Water vertices.
    pub water_vertices: u64,
}

impl FarTerrainStats {
    /// Total opaque triangles (two per quad).
    pub fn opaque_triangles(&self) -> u64 {
        self.opaque_quads.saturating_mul(2)
    }
}

/// A finished CPU-side far-terrain mesh and its build counters.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FarTerrainBuild {
    /// Renderable world-space geometry.
    pub mesh: FarTerrainMesh,
    /// Sampling and assembly counters.
    pub stats: FarTerrainStats,
}

/// Neutral, world-space far-terrain geometry handed to either renderer.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FarTerrainMesh {
    /// Camera XZ the mesh was centred on (for diagnostics; vertices are fixed
    /// world-space positions and the renderer does not recenter them).
    pub center: [f64; 2],
    /// Opaque heightfield vertices.
    pub vertices: Vec<FarTerrainVertex>,
    /// Opaque triangle indices.
    pub indices: Vec<u32>,
    /// Water surface vertices.
    pub water_vertices: Vec<FarTerrainVertex>,
    /// Water triangle indices.
    pub water_indices: Vec<u32>,
    /// World-space bounds used for frustum/distance culling.
    pub min: [f32; 3],
    /// World-space bounds used for frustum/distance culling.
    pub max: [f32; 3],
}

impl FarTerrainMesh {
    /// Number of opaque and water vertices.
    pub fn vertex_count(&self) -> usize {
        self.vertices.len() + self.water_vertices.len()
    }

    /// Number of opaque and water triangles.
    pub fn triangle_count(&self) -> usize {
        (self.indices.len() + self.water_indices.len()) / 3
    }

    /// Whether the mesh has no geometry.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty() && self.water_indices.is_empty()
    }

    /// Whether the mesh has opaque geometry.
    pub fn has_opaque(&self) -> bool {
        !self.indices.is_empty()
    }

    /// Whether the mesh has a water surface.
    pub fn has_water(&self) -> bool {
        !self.water_indices.is_empty()
    }

    /// Estimated CPU/GPU vertex bytes (opaque and water).
    pub fn vertex_bytes(&self) -> u64 {
        self.vertex_count() as u64 * std::mem::size_of::<FarTerrainVertex>() as u64
    }

    /// Estimated index bytes (opaque and water).
    pub fn index_bytes(&self) -> u64 {
        (self.indices.len() + self.water_indices.len()) as u64 * std::mem::size_of::<u32>() as u64
    }

    fn recompute_bounds(&mut self) {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for vertex in self.vertices.iter().chain(self.water_vertices.iter()) {
            for axis in 0..3 {
                min[axis] = min[axis].min(vertex.position[axis]);
                max[axis] = max[axis].max(vertex.position[axis]);
            }
        }
        if !min[0].is_finite() {
            min = [0.0; 3];
            max = [0.0; 3];
        }
        self.min = min;
        self.max = max;
    }
}

/// Builds the visual far-terrain mesh around `center` (world XZ).
///
/// `sampler` is called for every lattice point touched by an active tier. It
/// must be deterministic for a given world seed and coordinates; the client
/// passes `Generator::surface_sample`-backed samples, while the
/// benchmark can supply any equivalent pure function. Sampling rows run in
/// parallel on the current rayon pool.
pub fn build_far_terrain(
    seed: u64,
    config: FarTerrainConfig,
    sampler: impl Fn(i32, i32) -> FarSample + Sync,
    center: [f64; 2],
) -> FarTerrainBuild {
    let started = Instant::now();
    let config = config.normalised();
    let tiers = config.tiers();
    let sea_level = config.sea_level;
    let mut mesh = FarTerrainMesh {
        center,
        ..FarTerrainMesh::default()
    };
    let mut stats = FarTerrainStats {
        center,
        ..FarTerrainStats::default()
    };

    for (tier_index, tier) in tiers.iter().copied().enumerate() {
        let tier_build = build_tier(seed, center, tier, tier_index, sea_level, &sampler);
        stats.opaque_quads += tier_build.stats.opaque_quads;
        stats.opaque_vertices += tier_build.stats.opaque_vertices;
        stats.water_triangles += tier_build.stats.water_triangles;
        stats.water_vertices += tier_build.stats.water_vertices;
        stats.tiers.push(tier_build.stats);

        // Indices are per-mesh; offset the tier's vertices as they are appended.
        let vertex_base = mesh.vertices.len() as u32;
        mesh.vertices.extend_from_slice(&tier_build.vertices);
        mesh.indices
            .extend(tier_build.indices.iter().map(|index| index + vertex_base));
        let water_base = mesh.water_vertices.len() as u32;
        mesh.water_vertices.extend_from_slice(&tier_build.water_vertices);
        mesh.water_indices
            .extend(tier_build.water_indices.iter().map(|index| index + water_base));
    }

    mesh.recompute_bounds();
    stats.build_ms = started.elapsed().as_secs_f64() * 1000.0;
    FarTerrainBuild { mesh, stats }
}

struct TierBuild {
    vertices: Vec<FarTerrainVertex>,
    indices: Vec<u32>,
    water_vertices: Vec<FarTerrainVertex>,
    water_indices: Vec<u32>,
    stats: FarTierStats,
}

/// Inclusive lattice coverage of one tier, padded to overlap its neighbours.
fn tier_lattice_range(center: [f64; 2], tier: FarTier) -> (i32, i32, i32, i32) {
    let cell = tier.cell.max(1.0);
    let hi = tier.outer + cell * FAR_TIER_OVERLAP_CELLS;
    let first_x = ((center[0] - f64::from(hi)) / f64::from(cell)).floor() as i32;
    let last_x = ((center[0] + f64::from(hi)) / f64::from(cell)).ceil() as i32;
    let first_z = ((center[1] - f64::from(hi)) / f64::from(cell)).floor() as i32;
    let last_z = ((center[1] + f64::from(hi)) / f64::from(cell)).ceil() as i32;
    (first_x, last_x, first_z, last_z)
}

#[allow(clippy::too_many_arguments)]
fn build_tier(
    seed: u64,
    center: [f64; 2],
    tier: FarTier,
    tier_index: usize,
    sea_level: f32,
    sampler: &(impl Fn(i32, i32) -> FarSample + Sync),
) -> TierBuild {
    let cell = tier.cell.max(1.0);
    let cell_i = cell as i32;
    let overlap = cell * FAR_TIER_OVERLAP_CELLS;
    let include_lo = (tier.inner - overlap).max(0.0);
    let include_hi = tier.outer + overlap;
    let (first_x, last_x, first_z, last_z) = tier_lattice_range(center, tier);
    let columns = (last_x - first_x + 1).max(1) as usize;
    let rows = (last_z - first_z + 1).max(1) as usize;
    let mut samples = vec![FarSample::default(); columns * rows];
    samples.par_chunks_mut(columns).enumerate().for_each(|(row, out)| {
        let z = (first_z + row as i32).saturating_mul(cell_i);
        for (column, sample) in out.iter_mut().enumerate() {
            let x = (first_x + column as i32).saturating_mul(cell_i);
            *sample = sampler(x, z);
        }
    });

    let height_at = |column: usize, row: usize| -> i32 { samples[row * columns + column].height };
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut water_vertices = Vec::new();
    let mut water_indices = Vec::new();
    let mut vertex_map = vec![u32::MAX; columns * rows];
    let tier_offset = -(FAR_SURFACE_OFFSET + tier_index as f32 * FAR_TIER_OFFSET_STEP);
    let mut opaque_quads = 0u64;
    let mut water_triangles = 0u64;

    for row in 0..rows.saturating_sub(1) {
        for column in 0..columns.saturating_sub(1) {
            let world_x = (first_x + column as i32) as f64 * f64::from(cell_i);
            let world_z = (first_z + row as i32) as f64 * f64::from(cell_i);
            let cell_center_x = world_x + f64::from(cell) * 0.5;
            let cell_center_z = world_z + f64::from(cell) * 0.5;
            let dx = cell_center_x - center[0];
            let dz = cell_center_z - center[1];
            let radius = (dx * dx + dz * dz).sqrt();
            if radius < f64::from(include_lo) || radius > f64::from(include_hi) {
                continue;
            }

            let corners = [
                (column, row),
                (column + 1, row),
                (column + 1, row + 1),
                (column, row + 1),
            ];
            let corner_heights = [
                height_at(column, row),
                height_at(column + 1, row),
                height_at(column + 1, row + 1),
                height_at(column, row + 1),
            ];
            let water_depths = corner_heights.map(|height| sea_level - (height as f32 + 1.0));

            for (corner_index, (corner_column, corner_row)) in corners.iter().copied().enumerate() {
                let map_index = corner_row * columns + corner_column;
                if vertex_map[map_index] != u32::MAX {
                    continue;
                }
                let corner_world_x = (first_x + corner_column as i32).saturating_mul(cell_i);
                let corner_world_z = (first_z + corner_row as i32).saturating_mul(cell_i);
                let position_x = corner_world_x as f32;
                let position_z = corner_world_z as f32;
                let height = corner_heights[corner_index];
                let normal = heightfield_normal(&samples, columns, rows, corner_column, corner_row, cell);
                let slope = 1.0 - normal[1].clamp(0.0, 1.0);
                let color = surface_color(
                    seed,
                    corner_world_x,
                    corner_world_z,
                    height,
                    slope,
                    samples[corner_row * columns + corner_column].biome,
                    sea_level,
                    normal,
                );
                vertex_map[map_index] = vertices.len() as u32;
                vertices.push(FarTerrainVertex {
                    position: [position_x, height as f32 + 1.0 + tier_offset, position_z],
                    color,
                    normal,
                });
            }

            let [a, b, c, d] =
                corners.map(|(corner_column, corner_row)| vertex_map[corner_row * columns + corner_column]);
            indices.extend_from_slice(&[a, c, b, a, d, c]);
            opaque_quads += 1;

            if let Some(polygon) = clip_water_polygon(world_x, world_z, f64::from(cell), &water_depths) {
                let base = water_vertices.len() as u32;
                let water_y = sea_level + FAR_WATER_RISE - tier_index as f32 * 0.02;
                let average_depth = polygon.iter().map(|point| point.2).sum::<f32>() / polygon.len() as f32;
                for (point_x, point_z, depth) in &polygon {
                    let color = water_color(depth.max(0.0), average_depth);
                    water_vertices.push(FarTerrainVertex {
                        position: [*point_x as f32, water_y, *point_z as f32],
                        color,
                        normal: [0.0, 1.0, 0.0],
                    });
                }
                for index in 1..polygon.len() - 1 {
                    water_indices.extend_from_slice(&[base, base + index as u32, base + index as u32 + 1]);
                    water_triangles += 1;
                }
            }
        }
    }

    TierBuild {
        stats: FarTierStats {
            inner: tier.inner,
            outer: tier.outer,
            cell,
            opaque_quads,
            opaque_vertices: vertices.len() as u64,
            water_triangles,
            water_vertices: water_vertices.len() as u64,
        },
        vertices,
        indices,
        water_vertices,
        water_indices,
    }
}

fn heightfield_normal(
    samples: &[FarSample],
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
    cell: f32,
) -> [f32; 3] {
    let left = column.saturating_sub(1);
    let right = (column + 1).min(columns - 1);
    let up = row.saturating_sub(1);
    let down = (row + 1).min(rows - 1);
    let height = |column: usize, row: usize| samples[row * columns + column].height as f32;
    let dx_span = ((right - left).max(1)) as f32 * cell;
    let dz_span = ((down - up).max(1)) as f32 * cell;
    let dhdx = (height(right, row) - height(left, row)) / dx_span;
    let dhdz = (height(column, down) - height(column, up)) / dz_span;
    let normal = [-dhdx, 1.0, -dhdz];
    let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    if length <= f32::EPSILON {
        [0.0, 1.0, 0.0]
    } else {
        [normal[0] / length, normal[1] / length, normal[2] / length]
    }
}

/// A clipped water polygon vertex: world XZ plus local water depth.
type WaterPoint = (f64, f64, f32);

fn clip_water_polygon(world_x: f64, world_z: f64, cell: f64, depths: &[f32; 4]) -> Option<Vec<WaterPoint>> {
    if !depths.iter().any(|depth| *depth > 0.0) {
        return None;
    }
    let corners = [
        (world_x, world_z),
        (world_x + cell, world_z),
        (world_x + cell, world_z + cell),
        (world_x, world_z + cell),
    ];
    // Sutherland-Hodgman against the single plane `depth >= 0`.
    let mut polygon: Vec<WaterPoint> = corners
        .iter()
        .zip(depths.iter())
        .map(|((x, z), depth)| (*x, *z, *depth))
        .collect();
    let mut output = Vec::with_capacity(6);
    for index in 0..polygon.len() {
        let current = polygon[index];
        let next = polygon[(index + 1) % polygon.len()];
        let current_inside = current.2 >= 0.0;
        let next_inside = next.2 >= 0.0;
        if current_inside {
            output.push(current);
        }
        if current_inside != next_inside {
            let denominator = current.2 - next.2;
            if denominator.abs() > f32::EPSILON {
                let t = (current.2 / denominator).clamp(0.0, 1.0) as f64;
                output.push((
                    current.0 + (next.0 - current.0) * t,
                    current.1 + (next.1 - current.1) * t,
                    0.0,
                ));
            }
        }
    }
    polygon = output;
    if polygon.len() < 3 { None } else { Some(polygon) }
}

fn srgb_to_linear(color: [f32; 3]) -> [f32; 3] {
    crate::environment::srgb_to_linear_rgb(color)
}

fn hash01(seed: u64, x: i32, z: i32, salt: u64) -> f32 {
    let hash = mix64(
        seed ^ (x as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (z as i64 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ salt,
    );
    (hash >> 40) as f32 / (1u64 << 24) as f32
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

const GRASS: [f32; 3] = [0.34, 0.54, 0.26];
const FOREST_GRASS: [f32; 3] = [0.28, 0.46, 0.21];
const SAND: [f32; 3] = [0.84, 0.78, 0.58];
const SNOW: [f32; 3] = [0.84, 0.88, 0.93];
const STONE: [f32; 3] = [0.53, 0.54, 0.56];
const GRAVEL: [f32; 3] = [0.58, 0.56, 0.53];
const CLAY: [f32; 3] = [0.61, 0.63, 0.70];
const WATER_SHALLOW: [f32; 3] = [0.13, 0.36, 0.44];
const WATER_DEEP: [f32; 3] = [0.025, 0.10, 0.22];

/// Authored surface colour for one far vertex.
///
/// Mirrors the world generator's surface rules on the cheap sampler: sea and
/// beaches are sand, deserts sand, snowy taiga snow, steep slopes stone and
/// high mountains snow. A small deterministic tint variation and curvature
/// shading keep distant fields from reading as flat colour blobs.
#[allow(clippy::too_many_arguments)]
fn surface_color(
    seed: u64,
    x: i32,
    z: i32,
    height: i32,
    slope: f32,
    biome: FarBiome,
    sea_level: f32,
    normal: [f32; 3],
) -> [f32; 3] {
    let sea = sea_level.floor() as i32;
    let steep = slope > 0.45;
    let mut color = if height < sea {
        // Seabed: sand with occasional clay/gravel patches.
        let patch = hash01(seed, x, z, 0x5345_4142_4544);
        if patch > 0.72 {
            CLAY
        } else if patch < 0.20 {
            GRAVEL
        } else {
            [SAND[0] * 0.86, SAND[1] * 0.90, SAND[2] * 0.92]
        }
    } else if height <= sea + 2 {
        SAND
    } else {
        match biome {
            FarBiome::Desert => SAND,
            FarBiome::SnowyTaiga => {
                if steep {
                    STONE
                } else {
                    SNOW
                }
            }
            FarBiome::Mountains => {
                if height >= 100 {
                    SNOW
                } else if steep || height >= 90 {
                    STONE
                } else {
                    GRASS
                }
            }
            FarBiome::Plains | FarBiome::Forest => {
                if height >= 100 {
                    SNOW
                } else if steep {
                    STONE
                } else if biome == FarBiome::Forest {
                    FOREST_GRASS
                } else {
                    GRASS
                }
            }
        }
    };

    // Subtle deterministic variation and a gentle curvature term keep a
    // low-poly plain from reading as one flat colour block. The per-vertex hash
    // breaks large surfaces; the broad sine field adds metre-scale patches that
    // survive the tier's vertex spacing. Both are baked into the colour, so they
    // cost nothing per frame.
    let fine = 0.94 + 0.12 * hash01(seed, x, z, 0x0046_4152_5449_4E54);
    let broad = (x as f32 * 0.011 + z as f32 * 0.007).sin() * (z as f32 * 0.009 - x as f32 * 0.005).cos() * 0.055;
    let curvature = (1.0 - normal[1]).mul_add(-0.18, 1.0).clamp(0.88, 1.0);
    let shade = (fine + broad) * curvature;
    for channel in &mut color {
        *channel = (*channel * shade).clamp(0.0, 1.0);
    }
    srgb_to_linear(color)
}

fn water_color(depth: f32, average_depth: f32) -> [f32; 3] {
    let deep = ((average_depth.max(depth) - 1.0) / 18.0).clamp(0.0, 1.0);
    let mut color = lerp3(WATER_SHALLOW, WATER_DEEP, deep);
    // A little depth variation along the surface keeps wide oceans from
    // reading as one flat colour at the horizon.
    let shimmer = ((depth * 0.37).sin() * 0.02 + 1.0).clamp(0.95, 1.05);
    for channel in &mut color {
        *channel = (*channel * shimmer).clamp(0.0, 1.0);
    }
    srgb_to_linear(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_sampler(x: i32, z: i32) -> FarSample {
        let height = 62.0
            + 10.0 * (x as f32 * 0.013).sin()
            + 7.0 * (z as f32 * 0.017).cos()
            + ((x as i64 * 3 + z as i64 * 5).rem_euclid(7) as f32 - 3.0);
        let biome = match (x / 512 + z / 512).rem_euclid(5) {
            0 => FarBiome::Plains,
            1 => FarBiome::Forest,
            2 => FarBiome::Desert,
            3 => FarBiome::SnowyTaiga,
            _ => FarBiome::Mountains,
        };
        FarSample {
            height: height.round() as i32,
            biome,
        }
    }

    fn config(view: i32) -> FarTerrainConfig {
        FarTerrainConfig::new(view, 32, 63.0)
    }

    #[test]
    fn tiers_cover_the_visual_radius_without_a_gap() {
        for view in [32, 64, 128, 256, 512] {
            let config = config(view);
            let tiers = config.tiers();
            assert!(!tiers.is_empty(), "view {view} has no tiers");
            assert!(tiers.len() <= FAR_TIER_CELLS.len());
            assert!((tiers[0].inner - config.inner_radius()).abs() < 1e-4);
            for pair in tiers.windows(2) {
                assert!(
                    (pair[0].outer - pair[1].inner).abs() < 1e-4,
                    "view {view}: tier gap {:?} -> {:?}",
                    pair[0],
                    pair[1]
                );
            }
            let last = tiers.last().expect("tiers");
            assert!((last.outer - config.outer_radius()).abs() < 1e-4);
            assert!(config.outer_radius() >= config.view_distance_blocks());
            for tier in &tiers {
                assert!(tier.outer > tier.inner);
                assert!(tier.cell > 0.0);
            }
        }
    }

    #[test]
    fn mesh_quad_budget_is_bounded_at_512_chunks() {
        let mut config = config(512);
        config.simulation_distance_chunks = 32;
        let build = build_far_terrain(0x512, config, synthetic_sampler, [0.0, 0.0]);
        let stats = &build.stats;
        assert!(stats.opaque_quads > 20_000, "far mesh degenerated: {stats:?}");
        assert!(
            stats.opaque_quads <= 200_000,
            "far mesh exceeds the 200k-quad budget: {} quads",
            stats.opaque_quads
        );
        assert_eq!(stats.tiers.len(), 5);
        assert!(stats.opaque_vertices > 20_000);
        assert!(build.mesh.has_opaque());
        assert_eq!(build.mesh.indices.len(), stats.opaque_quads as usize * 6);
        assert!(stats.build_ms >= 0.0, "build time must be measured");
        assert_eq!(build.mesh.center, [0.0, 0.0]);
        assert!(build.mesh.min[1].is_finite() && build.mesh.max[1] > build.mesh.min[1]);
        // The mesh must extend to the view radius plus hysteresis, not merely
        // to the near world edge.
        assert!(build.mesh.max[0] >= 8000.0, "mesh x extent is {:?}", build.mesh.max);
        assert!(build.mesh.min[0] <= -8000.0, "mesh x extent is {:?}", build.mesh.min);
    }

    #[test]
    fn far_mesh_is_deterministic_for_the_same_seed_and_center() {
        let center = [37.5, -81.25];
        let first = build_far_terrain(7, config(128), synthetic_sampler, center);
        let second = build_far_terrain(7, config(128), synthetic_sampler, center);
        assert_eq!(first.mesh.vertices, second.mesh.vertices);
        assert_eq!(first.mesh.indices, second.mesh.indices);
        assert_eq!(first.mesh.water_vertices, second.mesh.water_vertices);
        assert_eq!(first.mesh.water_indices, second.mesh.water_indices);
        assert_eq!(first.mesh.min, second.mesh.min);
        assert_eq!(first.mesh.max, second.mesh.max);
        assert_eq!(first.stats.opaque_quads, second.stats.opaque_quads);
    }

    #[test]
    fn water_covers_submerged_cells_and_clips_the_shore() {
        let flat_ocean = |_x: i32, _z: i32| FarSample {
            height: 55,
            biome: FarBiome::Plains,
        };
        let build = build_far_terrain(1, config(32), flat_ocean, [0.0, 0.0]);
        assert!(build.mesh.has_water(), "a flat ocean must emit a water surface");
        assert!(build.stats.water_triangles > 0);
        let water_y = FAR_WATER_RISE + 63.0;
        for vertex in &build.mesh.water_vertices {
            assert!((vertex.position[1] - water_y).abs() < 0.05);
            assert_eq!(vertex.normal, [0.0, 1.0, 0.0]);
        }

        let flat_land = |_x: i32, _z: i32| FarSample {
            height: 80,
            biome: FarBiome::Plains,
        };
        let dry = build_far_terrain(1, config(32), flat_land, [0.0, 0.0]);
        assert!(!dry.mesh.has_water(), "land above sea level must not emit water");

        // A coastline cell keeps only the submerged part of the square.
        let shore = |x: i32, _z: i32| FarSample {
            height: if x < 0 { 55 } else { 80 },
            biome: FarBiome::Plains,
        };
        let clipped = build_far_terrain(1, config(32), shore, [0.0, 0.0]);
        assert!(clipped.mesh.has_water());
        for vertex in &clipped.mesh.water_vertices {
            assert!(
                vertex.position[0] <= 8.0 + 1e-3,
                "water leaked onto dry land at x={}",
                vertex.position[0]
            );
        }
    }

    #[test]
    fn every_vertex_uses_the_heightfield_surface_and_a_finite_normal() {
        let build = build_far_terrain(3, config(64), synthetic_sampler, [120.0, -40.0]);
        for vertex in build.mesh.vertices.iter().chain(build.mesh.water_vertices.iter()) {
            assert!(vertex.position[0].is_finite());
            assert!(vertex.position[1].is_finite());
            assert!(vertex.position[2].is_finite());
            assert!(vertex.normal[1] > 0.0, "heightfield normals must point up");
            let length = (vertex.normal[0].powi(2) + vertex.normal[1].powi(2) + vertex.normal[2].powi(2)).sqrt();
            assert!((length - 1.0).abs() < 1e-3, "non-unit normal {length}");
            for channel in vertex.color {
                assert!((0.0..=1.0).contains(&channel));
            }
        }
    }

    #[test]
    fn outer_tier_cells_are_coarser_than_near_tier_cells() {
        let tiers = config(512).tiers();
        for pair in tiers.windows(2) {
            assert!(pair[1].cell >= pair[0].cell);
        }
        assert_eq!(tiers[0].cell, 8.0);
        assert_eq!(tiers.last().expect("tier").cell, 128.0);
    }

    #[test]
    fn tier_lattice_padding_overlaps_the_neighbour() {
        let config = config(512);
        let tiers = config.tiers();
        for (index, tier) in tiers.iter().enumerate() {
            let (first_x, last_x, first_z, last_z) = tier_lattice_range([0.0, 0.0], *tier);
            assert!(last_x > first_x && last_z > first_z);
            if index > 0 {
                let previous = tiers[index - 1];
                let padding = tier.cell * FAR_TIER_OVERLAP_CELLS;
                assert!(
                    tier.inner - padding < previous.outer,
                    "tier {index} does not overlap its neighbour"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GPU resources and pipelines
// ---------------------------------------------------------------------------

/// GPU resources for the current far-terrain mesh.
pub(crate) struct GpuFarTerrain {
    pub(crate) vertex_buffer: wgpu::Buffer,
    pub(crate) index_buffer: wgpu::Buffer,
    pub(crate) index_count: u32,
    pub(crate) water_vertex_buffer: Option<wgpu::Buffer>,
    pub(crate) water_index_buffer: Option<wgpu::Buffer>,
    pub(crate) water_index_count: u32,
    pub(crate) min: [f32; 3],
    pub(crate) max: [f32; 3],
    pub(crate) vertex_count: u32,
    pub(crate) water_vertex_count: u32,
}

impl GpuFarTerrain {
    pub(crate) fn from_mesh(device: &wgpu::Device, queue: &wgpu::Queue, mesh: &FarTerrainMesh) -> Option<Self> {
        if mesh.is_empty() {
            return None;
        }
        let (vertex_buffer, index_buffer, index_count, vertex_count) = if mesh.indices.is_empty() {
            (
                upload_buffer(
                    device,
                    queue,
                    "far-terrain-vertices",
                    bytemuck::cast_slice(&mesh.vertices),
                    wgpu::BufferUsages::VERTEX,
                ),
                empty_index_buffer(device),
                0,
                0,
            )
        } else {
            let vertices = bytemuck::cast_slice(&mesh.vertices);
            let indices = bytemuck::cast_slice(&mesh.indices);
            (
                upload_buffer(
                    device,
                    queue,
                    "far-terrain-vertices",
                    vertices,
                    wgpu::BufferUsages::VERTEX,
                ),
                upload_buffer(device, queue, "far-terrain-indices", indices, wgpu::BufferUsages::INDEX),
                mesh.indices.len() as u32,
                mesh.vertices.len() as u32,
            )
        };
        let (water_vertex_buffer, water_index_buffer, water_index_count, water_vertex_count) =
            if mesh.water_indices.is_empty() {
                (None, None, 0, 0)
            } else {
                (
                    Some(upload_buffer(
                        device,
                        queue,
                        "far-terrain-water-vertices",
                        bytemuck::cast_slice(&mesh.water_vertices),
                        wgpu::BufferUsages::VERTEX,
                    )),
                    Some(upload_buffer(
                        device,
                        queue,
                        "far-terrain-water-indices",
                        bytemuck::cast_slice(&mesh.water_indices),
                        wgpu::BufferUsages::INDEX,
                    )),
                    mesh.water_indices.len() as u32,
                    mesh.water_vertices.len() as u32,
                )
            };
        Some(Self {
            vertex_buffer,
            index_buffer,
            index_count,
            water_vertex_buffer,
            water_index_buffer,
            water_index_count,
            min: mesh.min,
            max: mesh.max,
            vertex_count,
            water_vertex_count,
        })
    }
}

fn upload_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    data: &[u8],
    extra_usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    // Keep a non-zero size even when the caller only wants a placeholder.
    let size = (data.len() as wgpu::BufferAddress).max(4);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: extra_usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    if !data.is_empty() {
        queue.write_buffer(&buffer, 0, data);
    }
    buffer
}

fn empty_index_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("far-terrain-empty-indices"),
        size: 4,
        usage: wgpu::BufferUsages::INDEX,
        mapped_at_creation: false,
    })
}

fn far_vertex_buffers() -> [Option<wgpu::VertexBufferLayout<'static>>; 1] {
    [Some(wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<FarTerrainVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: std::mem::offset_of!(FarTerrainVertex, position) as wgpu::BufferAddress,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: std::mem::offset_of!(FarTerrainVertex, color) as wgpu::BufferAddress,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: std::mem::offset_of!(FarTerrainVertex, normal) as wgpu::BufferAddress,
                shader_location: 2,
            },
        ],
    })]
}

fn far_shader_module(device: &wgpu::Device, label: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(include_str!("far_terrain.wgsl").into()),
    })
}

/// Creates the opaque far-terrain pipeline: depth-tested and depth-writing,
/// with no culling so a heightfield triangle can never vanish from wind order.
pub(crate) fn create_far_terrain_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("far-terrain-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = far_shader_module(device, "far-terrain-shader");
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("far-terrain-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_far"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &far_vertex_buffers(),
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: crate::pipeline::DEPTH_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_far"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Creates the translucent far-water pipeline: alpha blended, `LessEqual`
/// depth test and no depth writes.
pub(crate) fn create_far_water_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("far-water-pipeline-layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    let shader = far_shader_module(device, "far-water-shader");
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("far-water-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_far"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &far_vertex_buffers(),
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: crate::pipeline::DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_far_water"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod gpu_tests {
    use super::*;
    use crate::{Camera, FarTerrainMesh, OffscreenRenderer, RenderError, RenderStats, SceneParams};
    use ob_math::WorldVec;

    /// Creates the offscreen renderer or returns `None` when the CI host has no
    /// GPU adapter, so the CPU test jobs pass without a software Vulkan driver.
    fn test_offscreen(width: u32, height: u32) -> Option<OffscreenRenderer> {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            eprintln!("skipping far-terrain GPU test: OPENBLOCK_NO_GPU_TESTS is set");
            return None;
        }
        match OffscreenRenderer::new(width, height) {
            Ok(renderer) => Some(renderer),
            Err(RenderError::NoAdapter(reason)) => {
                eprintln!("skipping far-terrain GPU test: no GPU adapter ({reason})");
                None
            }
            Err(error) => panic!("offscreen renderer: {error}"),
        }
    }

    fn plane_mesh() -> FarTerrainMesh {
        // A large red quad perpendicular to the initial view direction. It
        // covers the screen centre regardless of pitch and proves the far
        // pipeline rasterises and writes colour/depth into the shared HDR pass.
        let color = [0.8, 0.1, 0.05];
        let normal = [0.0, 1.0, 0.0];
        let mut mesh = FarTerrainMesh {
            center: [0.0, -2_000.0],
            vertices: Vec::new(),
            indices: vec![0, 2, 1, 0, 3, 2],
            water_vertices: Vec::new(),
            water_indices: Vec::new(),
            min: [0.0; 3],
            max: [0.0; 3],
        };
        for (x, y) in [
            (-2_000.0, -2_000.0),
            (2_000.0, -2_000.0),
            (2_000.0, 3_000.0),
            (-2_000.0, 3_000.0),
        ] {
            mesh.vertices.push(FarTerrainVertex {
                position: [x, y, -2_000.0],
                color,
                normal,
            });
        }
        mesh.recompute_bounds();
        mesh
    }

    #[test]
    fn far_terrain_pipeline_draws_into_the_offscreen_scene() {
        let Some(mut renderer) = test_offscreen(160, 96) else {
            return;
        };
        let mut camera = Camera::new(WorldVec::new(0.0, 100.0, 0.0), 0.0, 0.0);
        camera.far = 10_000.0;
        let scene = SceneParams::default();

        let empty = renderer.render_cached_scene(&camera, &scene).expect("empty frame");
        assert_eq!(empty.far_draw_calls, 0);
        let sky = renderer.read_rgba();

        renderer.set_far_terrain(plane_mesh());
        let bounds = renderer.far_terrain_bounds().expect("far bounds");
        assert!(bounds.0[0] <= -2_000.0 && bounds.1[0] >= 2_000.0);
        let stats: RenderStats = renderer.render_cached_scene(&camera, &scene).expect("far frame");
        assert_eq!(stats.far_draw_calls, 1, "the far plane should issue one draw");
        assert_eq!(stats.far_vertices, 4);
        assert_eq!(stats.far_triangles, 2);
        assert!(stats.draw_calls >= 1);
        let pixels = renderer.read_rgba();

        // The lower-centre pixel must now show the red heightfield rather than
        // the sky, proving the new pipeline, bind group and vertex layout passed
        // wgpu validation and rasterised into the HDR scene.
        let index = ((96 / 2) * 160 + 80) * 4;
        assert!(
            pixels[index] > sky[index] + 20,
            "far terrain did not brighten the lower centre: sky={:?} far={:?}",
            &sky[index..index + 4],
            &pixels[index..index + 4]
        );

        renderer.clear_far_terrain();
        let cleared = renderer.render_cached_scene(&camera, &scene).expect("cleared frame");
        assert_eq!(cleared.far_draw_calls, 0);
        assert!(renderer.far_terrain_bounds().is_none());
    }

    #[test]
    fn far_water_pipeline_draws_a_translucent_surface() {
        let Some(mut renderer) = test_offscreen(160, 96) else {
            return;
        };
        let mut camera = Camera::new(WorldVec::new(0.0, 100.0, 0.0), 0.0, 0.0);
        camera.far = 10_000.0;
        let scene = SceneParams::default();
        renderer.render_cached_scene(&camera, &scene).expect("sky frame");

        let color = [0.04, 0.15, 0.30];
        let mut mesh = FarTerrainMesh {
            center: [0.0, -2_000.0],
            water_vertices: vec![
                FarTerrainVertex {
                    position: [-2_000.0, -2_000.0, -2_000.0],
                    color,
                    normal: [0.0, 1.0, 0.0],
                },
                FarTerrainVertex {
                    position: [2_000.0, -2_000.0, -2_000.0],
                    color,
                    normal: [0.0, 1.0, 0.0],
                },
                FarTerrainVertex {
                    position: [2_000.0, 3_000.0, -2_000.0],
                    color,
                    normal: [0.0, 1.0, 0.0],
                },
                FarTerrainVertex {
                    position: [-2_000.0, 3_000.0, -2_000.0],
                    color,
                    normal: [0.0, 1.0, 0.0],
                },
            ],
            water_indices: vec![0, 2, 1, 0, 3, 2],
            ..FarTerrainMesh::default()
        };
        mesh.recompute_bounds();
        renderer.set_far_terrain(mesh);
        let stats = renderer.render_cached_scene(&camera, &scene).expect("far water frame");
        assert_eq!(stats.far_water_draw_calls, 1);
        assert_eq!(stats.far_water_vertices, 4);
        assert_eq!(
            stats.far_draw_calls, 0,
            "a water-only mesh must not issue an opaque draw"
        );
        assert_eq!(stats.water_draw_calls, 1);
    }
}

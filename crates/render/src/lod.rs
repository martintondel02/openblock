//! Distance level-of-detail policy and far-batch meshing.
//!
//! The v1.1 renderer keeps full 16³ section meshes inside a near radius and
//! merges the visible surface shell of every farther 2×2-chunk group into one
//! vertex buffer. A far batch is one draw call, so a 512-block radius does not
//! turn into thousands of per-section draws; the shell keeps the top
//! [`DEFAULT_SHELL_DEPTH_BLOCKS`] blocks of each column so caves, mines and
//! deep underground geometry are never submitted from far away.
//!
//! This is deliberately a *CPU/pragmatic* LOD, not the clustered GPU path in
//! `docs/spec/03-rendering.md`: it reuses the exact greedy mesher and only
//! concatenates its output. A batch therefore has no seams at LOD boundaries
//! and no second geometry format to keep valid; the tradeoff is that far
//! columns still cost one greedy mesh per shelled section on a worker thread.
//!
//! Tier decisions are group-based rather than per-column. That keeps a batch's
//! membership stable while the player moves inside one group and makes the
//! transition near ↔ far an explicit rebuild in the client.

use ob_math::{BlockPos, ChunkPos, N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y};
use ob_world::World;

use crate::MeshData;
use crate::mesh::{self, SectionSnapshot};

/// Full-detail radius used by the client at the 512-block setting.
///
/// Ten chunks (160 blocks) covers the terrain the player can inspect and edit
/// up close; beyond it the surface shell is visually indistinguishable at
/// 1280×720 while costing a fraction of the meshes and draws.
pub const DEFAULT_FULL_DETAIL_RADIUS_CHUNKS: i32 = 10;
/// Chunk columns per merged far batch (2 → one 32×32-block tile).
pub const DEFAULT_LOD_GROUP_CHUNKS: i32 = 2;
/// Blocks of surface kept below the highest block of a far column.
///
/// 48 blocks (three sections) preserves cliffs and hillsides; deeper caves and
/// underground geometry are hidden by the terrain above them anyway.
pub const DEFAULT_SHELL_DEPTH_BLOCKS: i32 = 48;

/// Distance LOD policy shared by the client and benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LodConfig {
    /// Column radius drawn with full per-section meshes, in chunks.
    pub full_detail_radius_chunks: i32,
    /// Chunk columns per merged far batch.
    pub group_chunks: i32,
    /// Blocks of surface retained in a far column.
    pub shell_depth_blocks: i32,
}

impl Default for LodConfig {
    fn default() -> Self {
        Self {
            full_detail_radius_chunks: DEFAULT_FULL_DETAIL_RADIUS_CHUNKS,
            group_chunks: DEFAULT_LOD_GROUP_CHUNKS,
            shell_depth_blocks: DEFAULT_SHELL_DEPTH_BLOCKS,
        }
    }
}

impl LodConfig {
    /// Policy used by the client for a render-distance setting.
    ///
    /// Render distances up to the full-detail radius keep every column at
    /// section detail; longer distances add merged surface-shell batches.
    pub fn for_render_distance(chunks: i32) -> Self {
        Self {
            full_detail_radius_chunks: chunks.clamp(1, 64).min(DEFAULT_FULL_DETAIL_RADIUS_CHUNKS),
            group_chunks: DEFAULT_LOD_GROUP_CHUNKS,
            shell_depth_blocks: DEFAULT_SHELL_DEPTH_BLOCKS,
        }
        .normalised()
    }

    /// Clamps the policy to values that keep group keys and heights valid.
    pub fn normalised(self) -> Self {
        Self {
            full_detail_radius_chunks: self.full_detail_radius_chunks.clamp(0, 64),
            group_chunks: self.group_chunks.clamp(1, 16),
            shell_depth_blocks: self.shell_depth_blocks.clamp(16, 256),
        }
    }

    /// Whether a camera chunk should use full section meshes for `chunk`.
    pub fn chunk_tier(&self, camera_chunk: ChunkPos, chunk: ChunkPos) -> LodTier {
        let config = self.normalised();
        let key = LodBatchKey::containing(chunk, config.group_chunks);
        tier_for_group(key, camera_chunk, &config)
    }
}

/// Geometry tier chosen for one 2×2-chunk group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LodTier {
    /// Per-section greedy meshes (near field).
    Full,
    /// One merged surface-shell batch for the whole group (far field).
    Shell,
}

/// Stable key of one far-batch group.
///
/// `x`/`z` are chunk coordinates divided by [`LodConfig::group_chunks`] with
/// Euclidean rounding, so negative coordinates group symmetrically around 0.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LodBatchKey {
    /// Group X coordinate.
    pub x: i32,
    /// Group Z coordinate.
    pub z: i32,
}

impl LodBatchKey {
    /// Group containing `chunk`.
    pub fn containing(chunk: ChunkPos, group_chunks: i32) -> Self {
        let group = group_chunks.max(1);
        Self {
            x: chunk.x.div_euclid(group),
            z: chunk.z.div_euclid(group),
        }
    }

    /// First chunk column (minimum X/Z) covered by this group.
    pub fn origin_chunk(self, group_chunks: i32) -> ChunkPos {
        let group = group_chunks.max(1);
        ChunkPos::new(self.x * group, self.z * group)
    }

    /// Chunk-space centre of the group, used for tier decisions.
    pub fn center_chunk(self, group_chunks: i32) -> [f64; 2] {
        let group = group_chunks.max(1) as f64;
        [
            self.x as f64 * group + (group - 1.0) * 0.5,
            self.z as f64 * group + (group - 1.0) * 0.5,
        ]
    }
}

/// Chooses a group's tier from its centre distance to the camera chunk.
///
/// The camera chunk is treated as a square cell, so the group centre is
/// compared against `full_radius` in chunk units. A group that straddles the
/// boundary is drawn one way or the other as a whole; keeping a group
/// internally consistent is what prevents holes at the LOD boundary.
pub fn tier_for_group(key: LodBatchKey, camera_chunk: ChunkPos, config: &LodConfig) -> LodTier {
    let config = config.normalised();
    let [cx, cz] = key.center_chunk(config.group_chunks);
    let dx = cx - f64::from(camera_chunk.x);
    let dz = cz - f64::from(camera_chunk.z);
    let radius = f64::from(config.full_detail_radius_chunks);
    if dx * dx + dz * dz <= radius * radius {
        LodTier::Full
    } else {
        LodTier::Shell
    }
}

/// Inclusive vertical section range retained for one far column.
///
/// The range starts `shell_depth_blocks` below the column's highest non-air
/// block and ends at the section containing that block. Returns `None` for a
/// column with no non-air blocks.
pub fn shell_section_range(world: &World, chunk: ChunkPos, shell_depth_blocks: i32) -> Option<(i32, i32)> {
    let column = world.chunk(chunk)?;
    let mut highest = None;
    for z in 0..SECTION_SIZE {
        for x in 0..SECTION_SIZE {
            if let Some(height) = column.height_at(x, z) {
                highest = Some(highest.map_or(height, |current: i32| current.max(height)));
            }
        }
    }
    let highest = highest?;
    let base_section = WORLD_MIN_Y / SECTION_SIZE as i32;
    let last_section = base_section + N_SECTIONS as i32 - 1;
    let low_block = (highest - shell_depth_blocks.max(0) + 1).max(WORLD_MIN_Y);
    let low_section = BlockPos::new(0, low_block, 0)
        .section()
        .y
        .clamp(base_section, last_section);
    let high_section = BlockPos::new(0, highest, 0)
        .section()
        .y
        .clamp(base_section, last_section);
    Some((low_section, high_section))
}

/// Number of vertical sections a far column does not submit.
pub fn skipped_sections_for_column(world: &World, chunk: ChunkPos, shell_depth_blocks: i32) -> usize {
    match shell_section_range(world, chunk, shell_depth_blocks) {
        Some((low, high)) => N_SECTIONS - (high - low + 1).max(0) as usize,
        None => 0,
    }
}

/// Geometry counts produced by one distance-LOD cache build.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LodCacheStats {
    /// Full-detail section meshes built.
    pub sections: u32,
    /// Vertices in those section meshes.
    pub section_vertices: u64,
    /// Merged far batches built.
    pub batches: u32,
    /// Shell sections represented by the batches.
    pub batch_sections: u64,
    /// Sections omitted from far columns by the surface shell.
    pub skipped_sections: u64,
    /// Vertices in the merged far batches.
    pub batch_vertices: u64,
}

/// Merged far-batch mesh plus the data the renderer needs to draw it.
#[derive(Debug, Clone, Default)]
pub struct LodBatchMesh {
    /// Concatenated shell vertices in world space.
    pub mesh: MeshData,
    /// Conservative world-space bounds of the emitted geometry.
    pub min: [f32; 3],
    /// Conservative world-space bounds of the emitted geometry.
    pub max: [f32; 3],
    /// Number of loaded shell sections represented.
    pub shell_sections: u32,
    /// Number of sections omitted by the surface shell.
    pub skipped_sections: u32,
    /// Number of loaded columns in the group.
    pub columns: u32,
}

impl LodBatchMesh {
    /// Whether the batch has no geometry and must not be uploaded.
    pub fn is_empty(&self) -> bool {
        self.mesh.vertices.is_empty()
    }
}

/// Builds one merged surface-shell batch for `key`.
///
/// `world` only needs to contain the group's columns and one surrounding
/// chunk border; the client passes a small cloned world so a worker can mesh
/// without touching the live world. The full world works equally well.
pub fn mesh_lod_batch(world: &World, key: LodBatchKey, config: &LodConfig) -> LodBatchMesh {
    let config = config.normalised();
    let group = config.group_chunks;
    let origin = key.origin_chunk(group);
    let mut batch = LodBatchMesh::default();
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let edge = SECTION_SIZE as f32;
    let mut any = false;
    for dz in 0..group {
        for dx in 0..group {
            let chunk = ChunkPos::new(origin.x + dx, origin.z + dz);
            let Some(column) = world.chunk(chunk) else {
                continue;
            };
            batch.columns += 1;
            let Some((low, high)) = shell_section_range(world, chunk, config.shell_depth_blocks) else {
                batch.skipped_sections += N_SECTIONS as u32;
                continue;
            };
            batch.shell_sections += (high - low + 1).max(0) as u32;
            batch.skipped_sections += (N_SECTIONS as i32 - (high - low + 1)).max(0) as u32;
            for section_y in low..=high {
                let pos = SectionPos::new(chunk.x, section_y, chunk.z);
                let Some(section) = column.section(pos) else {
                    continue;
                };
                if section.is_empty() {
                    continue;
                }
                let section_mesh = mesh::mesh_section(world, pos);
                if section_mesh.is_empty() {
                    continue;
                }
                let origin = pos.origin();
                let section_min = [origin.x as f32, origin.y as f32, origin.z as f32];
                let section_max = [section_min[0] + edge, section_min[1] + edge, section_min[2] + edge];
                min = [
                    min[0].min(section_min[0]),
                    min[1].min(section_min[1]),
                    min[2].min(section_min[2]),
                ];
                max = [
                    max[0].max(section_max[0]),
                    max[1].max(section_max[1]),
                    max[2].max(section_max[2]),
                ];
                batch.mesh.vertices.extend(section_mesh.vertices);
                any = true;
            }
        }
    }
    if !any {
        return batch;
    }
    // Tighten the vertical bounds with the emitted vertices when possible; a
    // batch may only use some sections of a group after an edit, and tighter
    // bounds make frustum rejection cheaper and more effective.
    for vertex in &batch.mesh.vertices {
        min[0] = min[0].min(vertex.position[0]);
        min[1] = min[1].min(vertex.position[1]);
        min[2] = min[2].min(vertex.position[2]);
        max[0] = max[0].max(vertex.position[0]);
        max[1] = max[1].max(vertex.position[1]);
        max[2] = max[2].max(vertex.position[2]);
    }
    batch.min = min;
    batch.max = max;
    batch
}

/// Captures a section snapshot for a far batch on the calling thread.
///
/// Used by tests and by callers that want to build a batch from immutable
/// snapshots instead of a cloned world.
pub fn snapshot_shell_sections(
    world: &World,
    key: LodBatchKey,
    config: &LodConfig,
) -> Vec<(SectionPos, SectionSnapshot)> {
    let config = config.normalised();
    let origin = key.origin_chunk(config.group_chunks);
    let mut snapshots = Vec::new();
    for dz in 0..config.group_chunks {
        for dx in 0..config.group_chunks {
            let chunk = ChunkPos::new(origin.x + dx, origin.z + dz);
            let Some((low, high)) = shell_section_range(world, chunk, config.shell_depth_blocks) else {
                continue;
            };
            for section_y in low..=high {
                let pos = SectionPos::new(chunk.x, section_y, chunk.z);
                if world.section(pos).is_some_and(|section| !section.is_empty()) {
                    snapshots.push((pos, SectionSnapshot::capture(world, pos)));
                }
            }
        }
    }
    snapshots
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos, WORLD_MIN_Y};
    use ob_world::{ChunkColumn, World};

    use super::*;

    fn world_with_columns(columns: &[(i32, i32)]) -> World {
        let registry = Arc::new(Registry::base());
        let mut world = World::new(registry, 0);
        for (x, z) in columns {
            world.insert_chunk(ChunkColumn::new(ChunkPos::new(*x, *z)));
        }
        world
    }

    fn stone(world: &World) -> BlockId {
        world.registry().id_or_air("openblock:stone")
    }

    #[test]
    fn group_keys_are_euclidean_and_stable() {
        assert_eq!(
            LodBatchKey::containing(ChunkPos::new(0, 0), 2),
            LodBatchKey { x: 0, z: 0 }
        );
        assert_eq!(
            LodBatchKey::containing(ChunkPos::new(1, 1), 2),
            LodBatchKey { x: 0, z: 0 }
        );
        assert_eq!(
            LodBatchKey::containing(ChunkPos::new(2, -1), 2),
            LodBatchKey { x: 1, z: -1 }
        );
        assert_eq!(
            LodBatchKey::containing(ChunkPos::new(-1, -1), 2),
            LodBatchKey { x: -1, z: -1 }
        );
        assert_eq!(LodBatchKey { x: 1, z: -1 }.origin_chunk(2), ChunkPos::new(2, -2));
        let [cx, cz] = LodBatchKey { x: 1, z: -1 }.center_chunk(2);
        assert_eq!((cx, cz), (2.5, -1.5));
    }

    #[test]
    fn tiers_switch_at_the_full_detail_radius() {
        let config = LodConfig {
            full_detail_radius_chunks: 10,
            group_chunks: 2,
            shell_depth_blocks: 48,
        };
        let camera = ChunkPos::new(0, 0);
        assert_eq!(
            tier_for_group(LodBatchKey::containing(ChunkPos::new(8, 0), 2), camera, &config),
            LodTier::Full
        );
        assert_eq!(
            tier_for_group(LodBatchKey::containing(ChunkPos::new(16, 0), 2), camera, &config),
            LodTier::Shell
        );
        assert_eq!(
            tier_for_group(LodBatchKey::containing(ChunkPos::new(32, 32), 2), camera, &config),
            LodTier::Shell
        );
        // The group policy applies to every column in the group.
        assert_eq!(
            config.chunk_tier(camera, ChunkPos::new(3, 3)),
            LodTier::Full,
            "small radii must stay full detail"
        );
    }

    #[test]
    fn shell_keeps_the_surface_and_drops_deep_sections() {
        let mut world = world_with_columns(&[(0, 0)]);
        let stone = stone(&world);
        let surface = WORLD_MIN_Y + 100;
        assert!(world.set_block(BlockPos::new(3, surface, 4), stone));
        let (low, high) = shell_section_range(&world, ChunkPos::new(0, 0), 48).expect("surface");
        // world Y 36 is section 2; 36 - 48 + 1 = -11 → section -1.
        assert_eq!((low, high), (-1, 2));
        assert_eq!(
            skipped_sections_for_column(&world, ChunkPos::new(0, 0), 48),
            N_SECTIONS - 4
        );

        // A deep floor far below the surface is not in the shell.
        assert!(low > BlockPos::new(0, surface - 60, 0).section().y);
        // No non-air columns means no shell at all.
        assert_eq!(shell_section_range(&world, ChunkPos::new(1, 0), 48), None);
    }

    #[test]
    fn far_batch_concatenates_shell_geometry_and_bounds() {
        let mut world = world_with_columns(&[(0, 0), (1, 0), (0, 1), (1, 1)]);
        let stone = stone(&world);
        let surface = WORLD_MIN_Y + 80;
        for z in 0..32 {
            for x in 0..32 {
                assert!(world.set_block(BlockPos::new(x, surface, z), stone));
            }
        }
        let config = LodConfig::default();
        let key = LodBatchKey { x: 0, z: 0 };
        let batch = mesh_lod_batch(&world, key, &config);
        assert!(!batch.is_empty());
        assert_eq!(batch.columns, 4);
        assert!(batch.shell_sections > 0);
        assert!(batch.skipped_sections > 0);
        assert!(batch.min[0] <= 0.0 && batch.max[0] >= 32.0);
        assert!(batch.min[1] <= surface as f32 && batch.max[1] >= surface as f32 + 1.0);
        assert!(batch.mesh.triangle_count() > 0);

        // A camera inside the full radius draws the same columns as sections,
        // not as a batch; the policy is the guard for that decision.
        assert_eq!(
            config.chunk_tier(ChunkPos::new(0, 0), ChunkPos::new(1, 1)),
            LodTier::Full
        );
        assert_eq!(
            config.chunk_tier(ChunkPos::new(100, 0), ChunkPos::new(1, 1)),
            LodTier::Shell
        );
    }

    #[test]
    fn snapshots_match_world_batch_geometry() {
        let mut world = world_with_columns(&[(0, 0), (1, 0), (0, 1), (1, 1)]);
        let stone = stone(&world);
        for z in 0..32 {
            for x in 0..32 {
                assert!(world.set_block(BlockPos::new(x, WORLD_MIN_Y + 40, z), stone));
            }
        }
        let config = LodConfig::default();
        let key = LodBatchKey { x: 0, z: 0 };
        let from_world = mesh_lod_batch(&world, key, &config);
        let snapshots = snapshot_shell_sections(&world, key, &config);
        assert!(!snapshots.is_empty());
        let mut vertices = Vec::new();
        for (_, snapshot) in snapshots {
            vertices.extend(snapshot.mesh(world.registry()).vertices);
        }
        assert_eq!(vertices, from_world.mesh.vertices);
        // Unloaded columns simply contribute nothing.
        let empty = mesh_lod_batch(&world, LodBatchKey { x: 5, z: 5 }, &config);
        assert!(empty.is_empty());
        assert!(empty.min.iter().all(|value| value.is_finite()));
    }
}

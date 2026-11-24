//! The world: a sparse map of chunk columns plus block queries and mutations.

use std::sync::Arc;

use ob_content::{BlockId, BlockType, Registry};
use ob_math::WorldVec as DVec3;
use ob_math::{BlockPos, ChunkPos, SectionPos, WORLD_MAX_Y, WORLD_MIN_Y};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::chunk::ChunkColumn;
use crate::section::Section;

/// Result of a block raycast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    /// The block that was hit.
    pub block: BlockPos,
    /// Exact world-space entry point.
    pub position: DVec3,
    /// Face normal pointing out of the hit block (integer offset).
    pub normal: BlockPos,
    /// Distance along the ray.
    pub distance: f64,
}

/// A sparse voxel world.
#[derive(Debug)]
pub struct World {
    chunks: FxHashMap<ChunkPos, ChunkColumn>,
    dirty: FxHashSet<SectionPos>,
    registry: Arc<Registry>,
    seed: u64,
    revision: u64,
}

impl World {
    /// Creates an empty world with the given registry and seed.
    pub fn new(registry: Arc<Registry>, seed: u64) -> Self {
        Self {
            chunks: FxHashMap::default(),
            dirty: FxHashSet::default(),
            registry,
            seed,
            revision: 0,
        }
    }

    /// The content registry used for block lookups.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The world seed (persisted with the world).
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Monotonic revision incremented on every block change.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Number of loaded chunk columns.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Number of loaded sections.
    pub fn section_count(&self) -> usize {
        self.chunks.values().map(|c| c.sections().count()).sum()
    }

    /// Immutable access to a chunk column.
    pub fn chunk(&self, pos: ChunkPos) -> Option<&ChunkColumn> {
        self.chunks.get(&pos)
    }

    /// Mutable access to a chunk column.
    pub fn chunk_mut(&mut self, pos: ChunkPos) -> Option<&mut ChunkColumn> {
        self.chunks.get_mut(&pos)
    }

    /// Iterates over all loaded chunk columns.
    pub fn chunks(&self) -> impl Iterator<Item = (&ChunkPos, &ChunkColumn)> + '_ {
        self.chunks.iter()
    }

    /// Inserts or replaces a chunk column.
    pub fn insert_chunk(&mut self, chunk: ChunkColumn) {
        let pos = chunk.pos();
        self.chunks.insert(pos, chunk);
        for section in 0..ob_math::N_SECTIONS {
            self.dirty.insert(SectionPos::new(
                pos.x,
                WORLD_MIN_Y / ob_math::SECTION_SIZE as i32 + section as i32,
                pos.z,
            ));
        }
    }

    /// Removes a chunk column (streaming unload).
    pub fn remove_chunk(&mut self, pos: ChunkPos) -> Option<ChunkColumn> {
        self.chunks.remove(&pos)
    }

    /// Reads a block; unloaded chunks and out-of-range Y read as air.
    pub fn get_block(&self, pos: BlockPos) -> BlockId {
        if !pos.in_world() {
            return BlockId::AIR;
        }
        match self.chunks.get(&pos.chunk()) {
            Some(chunk) => chunk.get_block(pos),
            None => BlockId::AIR,
        }
    }

    /// Writes a block. Returns `false` if the chunk is not loaded.
    pub fn set_block(&mut self, pos: BlockPos, value: BlockId) -> bool {
        if !pos.in_world() {
            return false;
        }
        let previous = self.get_block(pos);
        if previous == value {
            return true;
        }
        let chunk_pos = pos.chunk();
        let Some(chunk) = self.chunks.get_mut(&chunk_pos) else {
            return false;
        };
        chunk.set_block(pos, value);
        self.revision += 1;
        self.mark_dirty_with_neighbours(pos);
        true
    }

    /// Marks the section containing a block dirty, plus adjacent sections when
    /// the block touches a section border (their meshes and light are affected).
    pub fn mark_dirty_with_neighbours(&mut self, pos: BlockPos) {
        let section = pos.section();
        self.dirty.insert(section);
        let local = pos.local();
        let mut mark = |dx: i32, dy: i32, dz: i32| {
            let neighbour = SectionPos::new(section.x + dx, section.y + dy, section.z + dz);
            if neighbour.y >= WORLD_MIN_Y / ob_math::SECTION_SIZE as i32
                && neighbour.y < WORLD_MAX_Y / ob_math::SECTION_SIZE as i32
            {
                self.dirty.insert(neighbour);
            }
        };
        if local.x == 0 {
            mark(-1, 0, 0);
        } else if local.x == 15 {
            mark(1, 0, 0);
        }
        if local.y == 0 {
            mark(0, -1, 0);
        } else if local.y == 15 {
            mark(0, 1, 0);
        }
        if local.z == 0 {
            mark(0, 0, -1);
        } else if local.z == 15 {
            mark(0, 0, 1);
        }
    }

    /// Section lookup by absolute section position.
    pub fn section(&self, pos: SectionPos) -> Option<&Section> {
        self.chunks.get(&pos.chunk()).and_then(|c| c.section(pos))
    }

    /// Marks one section dirty (used when a frame's meshing budget runs out).
    pub fn mark_section_dirty(&mut self, pos: SectionPos) {
        self.dirty.insert(pos);
    }

    /// Takes the set of sections whose meshes are out of date.
    pub fn take_dirty_sections(&mut self) -> Vec<SectionPos> {
        let mut dirty: Vec<SectionPos> = self.dirty.drain().collect();
        // Deterministic order keeps meshing and replays reproducible.
        dirty.sort_by_key(|p| (p.y, p.z, p.x));
        dirty
    }

    /// Number of currently dirty sections.
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Block type definition for a block id.
    pub fn block_type(&self, id: BlockId) -> &BlockType {
        self.registry.block(id)
    }

    /// Whether a block is solid and blocks entity movement.
    pub fn is_solid(&self, pos: BlockPos) -> bool {
        self.block_type(self.get_block(pos)).solid
    }

    /// Whether a block fully occludes neighbouring faces.
    pub fn is_opaque(&self, pos: BlockPos) -> bool {
        self.block_type(self.get_block(pos)).opaque
    }

    /// Highest non-air block in a column, if the chunk is loaded.
    pub fn height_at(&self, x: i32, z: i32) -> Option<i32> {
        let chunk = self.chunks.get(&BlockPos::new(x, 0, z).chunk())?;
        chunk.height_at(x.rem_euclid(16) as usize, z.rem_euclid(16) as usize)
    }

    /// Casts a ray through the voxel grid, returning the first solid block hit.
    ///
    /// Uses an Amanatides–Woo DDA traversal; the direction need not be
    /// normalised but should be non-zero.
    pub fn raycast(&self, origin: DVec3, direction: DVec3, max_distance: f64) -> Option<RayHit> {
        if direction.length_squared() < 1e-12 {
            return None;
        }
        let dir = direction.normalize();
        let mut block = BlockPos::new(
            origin.x.floor() as i32,
            origin.y.floor() as i32,
            origin.z.floor() as i32,
        );
        let step = BlockPos::new(
            if dir.x > 0.0 { 1 } else { -1 },
            if dir.y > 0.0 { 1 } else { -1 },
            if dir.z > 0.0 { 1 } else { -1 },
        );
        let delta = DVec3::new(
            if dir.x.abs() < 1e-12 {
                f64::INFINITY
            } else {
                (1.0f64 / dir.x).abs()
            },
            if dir.y.abs() < 1e-12 {
                f64::INFINITY
            } else {
                (1.0f64 / dir.y).abs()
            },
            if dir.z.abs() < 1e-12 {
                f64::INFINITY
            } else {
                (1.0f64 / dir.z).abs()
            },
        );
        let mut t_max = DVec3::new(
            axis_distance(origin.x, dir.x, step.x),
            axis_distance(origin.y, dir.y, step.y),
            axis_distance(origin.z, dir.z, step.z),
        );

        let mut distance = 0.0;
        let mut normal = BlockPos::new(0, 0, 0);
        loop {
            if self.is_solid(block) {
                return Some(RayHit {
                    block,
                    position: origin + dir * distance,
                    normal,
                    distance,
                });
            }
            if t_max.x <= t_max.y && t_max.x <= t_max.z {
                distance = t_max.x;
                t_max.x += delta.x;
                block.x += step.x;
                normal = BlockPos::new(-step.x, 0, 0);
            } else if t_max.y <= t_max.z {
                distance = t_max.y;
                t_max.y += delta.y;
                block.y += step.y;
                normal = BlockPos::new(0, -step.y, 0);
            } else {
                distance = t_max.z;
                t_max.z += delta.z;
                block.z += step.z;
                normal = BlockPos::new(0, 0, -step.z);
            }
            if distance > max_distance {
                return None;
            }
        }
    }

    /// Approximate heap bytes of block and light data.
    pub fn heap_bytes(&self) -> usize {
        self.chunks.values().map(|c| c.heap_bytes()).sum()
    }
}

fn axis_distance(origin: f64, direction: f64, step: i32) -> f64 {
    if direction.abs() < 1e-12 {
        return f64::INFINITY;
    }
    let edge = if step > 0 { origin.floor() + 1.0 } else { origin.floor() };
    ((edge - origin) / direction).abs()
}

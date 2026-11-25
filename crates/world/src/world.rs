//! The world: a sparse map of chunk columns plus block queries and mutations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ob_content::{BlockId, BlockType, Registry};
use ob_math::WorldVec as DVec3;
use ob_math::{BlockPos, ChunkPos, LocalPos, SECTION_SIZE, SectionPos, WORLD_MAX_Y, WORLD_MIN_Y};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::chunk::ChunkColumn;
use crate::light::{LightSample, unpack_sky, with_block, with_sky};
use crate::lighting;
use crate::save::WorldSnapshot;
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
///
/// Columns are stored behind `Arc` so [`World::snapshot`] can freeze the loaded
/// set for a background save in O(number of columns) pointer copies. Mutating a
/// chunk after a snapshot has been taken pays one copy of that single column
/// (copy-on-write in [`World::chunk_mut`]); the snapshot itself never sees the
/// change.
#[derive(Debug)]
pub struct World {
    chunks: FxHashMap<ChunkPos, Arc<ChunkColumn>>,
    dirty: FxHashSet<SectionPos>,
    registry: Arc<Registry>,
    seed: u64,
    revision: u64,
    /// Cached rolling checksum of all stored light words.
    light_hash_cache: AtomicU64,
    light_hash_dirty: AtomicBool,
    /// Current light semantics version; 2 is the 1.2 engine.
    light_version: u32,
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
            light_hash_cache: AtomicU64::new(0),
            light_hash_dirty: AtomicBool::new(true),
            light_version: crate::save::LIGHT_VERSION,
        }
    }

    /// Builds a world from existing columns without marking them dirty.
    ///
    /// Used to hand a small neighbourhood to a background LOD mesher: the
    /// caller clones the target group plus one chunk of border, and the worker
    /// meshes against the same block data (and registry) as the live world
    /// without producing spurious dirty sections in the main world.
    pub fn from_columns(registry: Arc<Registry>, seed: u64, columns: impl IntoIterator<Item = ChunkColumn>) -> Self {
        let mut world = Self::new(registry, seed);
        for column in columns {
            world.insert_loaded_chunk(column);
        }
        world
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
        self.chunks.get(&pos).map(Arc::as_ref)
    }

    /// Mutable access to a chunk column.
    ///
    /// Copies the column if a background save still holds a snapshot of it.
    pub fn chunk_mut(&mut self, pos: ChunkPos) -> Option<&mut ChunkColumn> {
        self.chunks.get_mut(&pos).map(Arc::make_mut)
    }

    /// Iterates over all loaded chunk columns.
    pub fn chunks(&self) -> impl Iterator<Item = (&ChunkPos, &ChunkColumn)> + '_ {
        self.chunks.iter().map(|(pos, chunk)| (pos, chunk.as_ref()))
    }

    /// Captures an immutable, cheaply shareable view of every loaded column.
    ///
    /// The snapshot holds one `Arc` per column and the seed and registry
    /// fingerprint at capture time. Encoding it on a worker thread therefore
    /// does not block the simulation or copy the world; later edits
    /// copy-on-write only the columns they touch. Chunks are sorted by
    /// `(cx, cz)` at capture so the encoded stream is deterministic regardless
    /// of hash-map iteration order.
    pub fn snapshot(&self) -> WorldSnapshot {
        let mut chunks: Vec<Arc<ChunkColumn>> = self.chunks.values().cloned().collect();
        chunks.sort_by_key(|chunk| (chunk.pos().x, chunk.pos().z));
        WorldSnapshot::new(self.seed, self.registry(), chunks)
    }

    /// Inserts or replaces a chunk column and reconciles light with resident
    /// neighbours.
    ///
    /// A freshly built column that was not finalised is seeded here once, so
    /// hand-built worlds and tests get the same dark caves as generated ones.
    pub fn insert_chunk(&mut self, mut chunk: ChunkColumn) {
        if !chunk.is_lit() {
            chunk.finalise(&self.registry);
        }
        let pos = chunk.pos();
        self.insert_loaded_chunk(chunk);
        lighting::reconcile_chunk_borders(self, pos);
        self.invalidate_light_hash();
    }

    /// Inserts a column whose light is already authoritative, without border
    /// reconciliation. Used by the save loader and by LOD world clones.
    pub(crate) fn insert_loaded_chunk(&mut self, chunk: ChunkColumn) {
        let pos = chunk.pos();
        self.chunks.insert(pos, Arc::new(chunk));
        for section in 0..ob_math::N_SECTIONS {
            self.dirty.insert(SectionPos::new(
                pos.x,
                WORLD_MIN_Y / ob_math::SECTION_SIZE as i32 + section as i32,
                pos.z,
            ));
        }
        self.invalidate_light_hash();
    }

    /// Removes a chunk column (streaming unload).
    ///
    /// Returns an owned copy because a background save may still reference the
    /// removed column through its snapshot.
    #[allow(clippy::map_unwrap_or)]
    pub fn remove_chunk(&mut self, pos: ChunkPos) -> Option<ChunkColumn> {
        let removed = self
            .chunks
            .remove(&pos)
            .map(|column| Arc::try_unwrap(column).unwrap_or_else(|shared| (*shared).clone()));
        if removed.is_some() {
            // Stored light in surviving neighbours may include light that
            // entered from this chunk. It is not recomputed here: unloading is
            // a streaming event and the next reload reconciles the border.
            self.invalidate_light_hash();
        }
        removed
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
        Arc::make_mut(chunk).set_block(pos, value);
        self.revision += 1;
        self.mark_dirty_with_neighbours(pos);
        let before = self.registry.block(previous);
        let after = self.registry.block(value);
        let block_changed = before.emission != after.emission || before.attenuation != after.attenuation;
        // A sky-opacity change only matters when stored sky light is present at
        // the edited cell or one of its six neighbours. Deep under a roof every
        // neighbour is 0 and the column below cannot brighten, so the expensive
        // column pass is skipped (a roof placement/removal still sees sky 15 at
        // the edited cell or above and takes the full path).
        let sky_changed = before.sky_attenuation != after.sky_attenuation
            && std::iter::once(pos)
                .chain([
                    BlockPos::new(pos.x - 1, pos.y, pos.z),
                    BlockPos::new(pos.x + 1, pos.y, pos.z),
                    BlockPos::new(pos.x, pos.y - 1, pos.z),
                    BlockPos::new(pos.x, pos.y + 1, pos.z),
                    BlockPos::new(pos.x, pos.y, pos.z - 1),
                    BlockPos::new(pos.x, pos.y, pos.z + 1),
                ])
                .any(|sample| self.sky_light(sample) > 0);
        if block_changed || sky_changed {
            lighting::relight_edit(self, pos, sky_changed, block_changed);
        }
        self.invalidate_light_hash();
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

    /// The packed light word at a block position, or zero for an unloaded
    /// chunk or an out-of-world Y.
    pub fn light_word(&self, pos: BlockPos) -> u16 {
        if !pos.in_world() {
            return 0;
        }
        self.section(pos.section())
            .map_or(0, |section| section.light().get(pos.local()))
    }

    /// Decoded sky and RGB block light at a block position.
    pub fn light_sample(&self, pos: BlockPos) -> LightSample {
        LightSample::from_word(self.light_word(pos))
    }

    /// Sky light `0..=15` at a block position.
    pub fn sky_light(&self, pos: BlockPos) -> u8 {
        unpack_sky(self.light_word(pos))
    }

    /// Brightest RGB block-light channel at a block position.
    pub fn block_light(&self, pos: BlockPos) -> u8 {
        self.light_sample(pos).block_level()
    }

    /// Gameplay light query from spec/07: `max(floor(sky × day × weather),
    /// max(R,G,B))`.
    ///
    /// `day_factor` and `weather_factor` are server-owned presentation-free
    /// multipliers in `0.0..=1.0` (`day_factor` from the celestial clock,
    /// `weather_factor` 1.0 clear / 0.8 rain / 0.6 thunder). The query never
    /// mutates stored light, so weather and time changes cannot desynchronise a
    /// save or another client. Returns `0..=15`.
    pub fn gameplay_light(&self, pos: BlockPos, day_factor: f32, weather_factor: f32) -> u8 {
        let sample = self.light_sample(pos);
        let day = day_factor.clamp(0.0, 1.0);
        let weather = weather_factor.clamp(0.0, 1.0);
        let sky = (f32::from(sample.sky) * day * weather).floor().clamp(0.0, 15.0) as u8;
        sky.max(sample.block_level())
    }

    /// Current light semantics version (2 after the 1.2 migration).
    pub fn light_version(&self) -> u32 {
        self.light_version
    }

    /// Marks this world's light as the given semantics version.
    pub(crate) fn set_light_version(&mut self, version: u32) {
        self.light_version = version;
    }

    /// Rolling checksum over every stored light word.
    ///
    /// The checksum is cached and invalidated on light changes, so calling it
    /// repeatedly (for example from a state hash) does not rescan all sections.
    /// It covers the packed sky+RGB words and their section positions, not block
    /// data, so a save/load round-trip can assert light equality separately
    /// from the block hash.
    pub fn light_hash(&self) -> u64 {
        if !self.light_hash_dirty.load(Ordering::Acquire) {
            return self.light_hash_cache.load(Ordering::Relaxed);
        }
        let mut chunks: Vec<(&ChunkPos, &ChunkColumn)> = self.chunks.iter().map(|(pos, c)| (pos, c.as_ref())).collect();
        chunks.sort_by_key(|(pos, _)| (pos.x, pos.z));
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for (pos, column) in chunks {
            hash = hash_word(hash, pos.x as i64 as u64);
            hash = hash_word(hash, pos.z as i64 as u64);
            for (section_pos, section) in column.sections() {
                hash = hash_word(hash, section_pos.y as i64 as u64);
                // Hash the canonical 4,096 per-cell words, not the container
                // representation: a dense section whose words are all equal is
                // the same field as a uniform section, and a save/load may
                // preserve either encoding.
                match section.light().uniform_value() {
                    Some(word) => {
                        for _ in 0..crate::light::BLOCK_COUNT {
                            hash = hash_word(hash, u64::from(word));
                        }
                    }
                    None => {
                        let words = section.light().raw().expect("dense light has raw words");
                        for &word in words.iter() {
                            hash = hash_word(hash, u64::from(word));
                        }
                    }
                }
            }
        }
        let value = ob_math::mix64(hash);
        self.light_hash_cache.store(value, Ordering::Relaxed);
        self.light_hash_dirty.store(false, Ordering::Release);
        value
    }

    /// Recomputes the whole resident world's light once (migration path).
    pub fn relight_all(&mut self) {
        lighting::relight_all(self);
        self.light_version = crate::save::LIGHT_VERSION;
        self.invalidate_light_hash();
    }

    /// Relights a 31³ box around an edit (public for host/tool callers).
    pub fn relight_around(&mut self, pos: BlockPos) {
        lighting::relight_edit(self, pos, true, true);
    }

    /// Whether a chunk containing `pos` is resident.
    pub fn is_loaded(&self, pos: BlockPos) -> bool {
        pos.in_world() && self.chunks.contains_key(&pos.chunk())
    }

    pub(crate) fn invalidate_light_hash(&self) {
        self.light_hash_dirty.store(true, Ordering::Release);
    }

    /// Recomputes sky light for every cell in a box from the stored level just
    /// above it, writing every cell (including zeros below an opaque block).
    ///
    /// Working column-at-a-time avoids a `World`/section lookup and hash
    /// invalidation per cell; the edit relight path calls this instead of a
    /// separate clear pass.
    pub(crate) fn rebuild_sky_box(&mut self, bounds: lighting::LightBounds) {
        let registry = Arc::clone(&self.registry);
        let min_chunk_x = bounds.min[0].div_euclid(SECTION_SIZE as i32);
        let max_chunk_x = bounds.max[0].div_euclid(SECTION_SIZE as i32);
        let min_chunk_z = bounds.min[2].div_euclid(SECTION_SIZE as i32);
        let max_chunk_z = bounds.max[2].div_euclid(SECTION_SIZE as i32);
        for cz in min_chunk_z..=max_chunk_z {
            for cx in min_chunk_x..=max_chunk_x {
                let Some(chunk) = self.chunks.get_mut(&ChunkPos::new(cx, cz)) else {
                    continue;
                };
                let column = Arc::make_mut(chunk);
                let local_min_x = (bounds.min[0] - cx * SECTION_SIZE as i32).max(0);
                let local_max_x = (bounds.max[0] - cx * SECTION_SIZE as i32).min(SECTION_SIZE as i32 - 1);
                let local_min_z = (bounds.min[2] - cz * SECTION_SIZE as i32).max(0);
                let local_max_z = (bounds.max[2] - cz * SECTION_SIZE as i32).min(SECTION_SIZE as i32 - 1);
                let min_sy = bounds.min[1].div_euclid(SECTION_SIZE as i32);
                let max_sy = bounds.max[1].div_euclid(SECTION_SIZE as i32);
                for z in local_min_z..=local_max_z {
                    for x in local_min_x..=local_max_x {
                        let world_x = cx * SECTION_SIZE as i32 + x;
                        let world_z = cz * SECTION_SIZE as i32 + z;
                        let above = bounds.max[1] + 1;
                        let mut level = if above >= WORLD_MAX_Y {
                            15
                        } else {
                            unpack_sky(column.light_word_at(BlockPos::new(world_x, above, world_z)))
                        };
                        // Work section by section: the inner loop then indexes
                        // the section directly instead of resolving a
                        // `SectionPos` for every one of the ~30k cells.
                        for sy in (min_sy..=max_sy).rev() {
                            let section_min_y = sy * SECTION_SIZE as i32;
                            let first_y = bounds.min[1].max(section_min_y);
                            let last_y = bounds.max[1].min(section_min_y + SECTION_SIZE as i32 - 1);
                            let Some(section) = column.section_mut(SectionPos::new(cx, sy, cz)) else {
                                continue;
                            };
                            // A fully opaque section stops sky at its top and
                            // can never store a non-zero level. Clear it in one
                            // pass and skip the per-cell work; this is the
                            // common case for the rock under a cave or roof.
                            if section.sky_is_opaque(&registry) {
                                section.clear_sky();
                                level = 0;
                                continue;
                            }
                            // A fully air section keeps the running level for
                            // every cell. If it already stores that uniform
                            // value, skipping it avoids thousands of no-op
                            // writes for the open sky above and dark air below.
                            if section.is_empty() && section.light().uniform_value() == Some(u16::from(level) << 12) {
                                continue;
                            }
                            for y in (first_y..=last_y).rev() {
                                let local = LocalPos {
                                    x: x as u8,
                                    y: (y - section_min_y) as u8,
                                    z: z as u8,
                                };
                                let attenuation = registry.block(section.get(local)).sky_attenuation;
                                level = level.saturating_sub(attenuation);
                                let word = with_sky(section.light().get(local), level);
                                section.light_mut().set(local, word);
                            }
                        }
                    }
                }
            }
        }
        self.mark_light_bounds_dirty(bounds);
        self.invalidate_light_hash();
    }

    /// Whether any cell inside a box stores non-zero RGB block light.
    ///
    /// Uniform sections answer from their one word; dense sections are scanned
    /// only over the clamped box. The edit relight path uses this to skip the
    /// block-light pass entirely for a stone/water edit in a dark room.
    pub(crate) fn box_has_block_light(&self, bounds: lighting::LightBounds) -> bool {
        let min_cx = bounds.min[0].div_euclid(SECTION_SIZE as i32);
        let max_cx = bounds.max[0].div_euclid(SECTION_SIZE as i32);
        let min_cz = bounds.min[2].div_euclid(SECTION_SIZE as i32);
        let max_cz = bounds.max[2].div_euclid(SECTION_SIZE as i32);
        let min_sy = bounds.min[1].div_euclid(SECTION_SIZE as i32);
        let max_sy = bounds.max[1].div_euclid(SECTION_SIZE as i32);
        for cz in min_cz..=max_cz {
            for cx in min_cx..=max_cx {
                for sy in min_sy..=max_sy {
                    let Some(section) = self.section(SectionPos::new(cx, sy, cz)) else {
                        continue;
                    };
                    if let Some(word) = section.light().uniform_value() {
                        if word & 0x0FFF != 0 {
                            return true;
                        }
                        continue;
                    }
                    let origin_y = sy * SECTION_SIZE as i32;
                    let min_x = (bounds.min[0] - cx * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    let max_x = (bounds.max[0] - cx * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    let min_z = (bounds.min[2] - cz * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    let max_z = (bounds.max[2] - cz * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    let min_y = (bounds.min[1] - origin_y).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    let max_y = (bounds.max[1] - origin_y).clamp(0, SECTION_SIZE as i32 - 1) as u8;
                    for y in min_y..=max_y {
                        for z in min_z..=max_z {
                            for x in min_x..=max_x {
                                if section.light().get(ob_math::LocalPos { x, y, z }) & 0x0FFF != 0 {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Clears sky and/or block light over an inclusive world-space box.
    pub(crate) fn clear_light_box(&mut self, bounds: lighting::LightBounds, sky: bool, block: bool) {
        if !sky && !block {
            return;
        }
        let min_chunk_x = bounds.min[0].div_euclid(SECTION_SIZE as i32);
        let max_chunk_x = bounds.max[0].div_euclid(SECTION_SIZE as i32);
        let min_chunk_z = bounds.min[2].div_euclid(SECTION_SIZE as i32);
        let max_chunk_z = bounds.max[2].div_euclid(SECTION_SIZE as i32);
        let min_section_y = bounds.min[1].div_euclid(SECTION_SIZE as i32);
        let max_section_y = bounds.max[1].div_euclid(SECTION_SIZE as i32);
        for cz in min_chunk_z..=max_chunk_z {
            for cx in min_chunk_x..=max_chunk_x {
                let Some(chunk) = self.chunks.get_mut(&ChunkPos::new(cx, cz)) else {
                    continue;
                };
                let column = Arc::make_mut(chunk);
                for sy in min_section_y..=max_section_y {
                    let Some(section) = column.section_mut(SectionPos::new(cx, sy, cz)) else {
                        continue;
                    };
                    let section_min_y = sy * SECTION_SIZE as i32;
                    let local_min = [
                        (bounds.min[0] - cx * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                        (bounds.min[1] - section_min_y).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                        (bounds.min[2] - cz * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                    ];
                    let local_max = [
                        (bounds.max[0] - cx * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                        (bounds.max[1] - section_min_y).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                        (bounds.max[2] - cz * SECTION_SIZE as i32).clamp(0, SECTION_SIZE as i32 - 1) as u8,
                    ];
                    section.clear_light_box(local_min, local_max, sky, block);
                }
            }
        }
        self.mark_light_bounds_dirty(bounds);
        self.invalidate_light_hash();
    }

    pub(crate) fn set_light_word_raw(&mut self, pos: BlockPos, word: u16) -> bool {
        if !pos.in_world() {
            return false;
        }
        let Some(chunk) = self.chunks.get_mut(&pos.chunk()) else {
            return false;
        };
        let Some(section) = Arc::make_mut(chunk).section_mut(pos.section()) else {
            return false;
        };
        section.light_mut().set(pos.local(), word);
        // Light changes up to 14 blocks away change vertex light through the
        // mesher's one-block snapshot border, so the touched section must be
        // remeshed even when the edited block is not inside it.
        self.dirty.insert(pos.section());
        self.invalidate_light_hash();
        true
    }

    /// Marks every section intersecting an inclusive world-space box dirty.
    ///
    /// Used by the bulk sky writers, which bypass [`Self::set_light_word_raw`]
    /// and therefore cannot rely on per-cell dirty marking.
    fn mark_light_bounds_dirty(&mut self, bounds: lighting::LightBounds) {
        let min_cx = bounds.min[0].div_euclid(SECTION_SIZE as i32);
        let max_cx = bounds.max[0].div_euclid(SECTION_SIZE as i32);
        let min_cz = bounds.min[2].div_euclid(SECTION_SIZE as i32);
        let max_cz = bounds.max[2].div_euclid(SECTION_SIZE as i32);
        let min_sy = bounds.min[1].div_euclid(SECTION_SIZE as i32);
        let max_sy = bounds.max[1].div_euclid(SECTION_SIZE as i32);
        for cz in min_cz..=max_cz {
            for cx in min_cx..=max_cx {
                if !self.chunks.contains_key(&ChunkPos::new(cx, cz)) {
                    continue;
                }
                for sy in min_sy..=max_sy {
                    self.dirty.insert(SectionPos::new(cx, sy, cz));
                }
            }
        }
    }

    pub(crate) fn set_sky_light(&mut self, pos: BlockPos, level: u8) {
        let word = with_sky(self.light_word(pos), level);
        self.set_light_word_raw(pos, word);
    }

    pub(crate) fn set_block_light(&mut self, pos: BlockPos, red: u8, green: u8, blue: u8) {
        let word = with_block(self.light_word(pos), red, green, blue);
        self.set_light_word_raw(pos, word);
    }

    /// Number of loaded chunks carrying a dense (non-uniform) light section.
    ///
    /// Diagnostic for the memory budget and the migration tests.
    pub fn dense_light_section_count(&self) -> usize {
        self.chunks
            .values()
            .flat_map(|column| column.sections())
            .filter(|(_, section)| section.light().uniform_value().is_none())
            .count()
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

/// FNV-1a step over one 64-bit word.
fn hash_word(mut hash: u64, word: u64) -> u64 {
    for byte in word.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

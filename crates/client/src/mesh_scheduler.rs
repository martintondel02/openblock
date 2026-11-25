//! Background section meshing on a dedicated rayon thread pool.
//!
//! The main thread captures an immutable [`SectionSnapshot`] for each dirty
//! section, hands it to a worker and keeps rendering. Completed meshes arrive
//! through a channel; every submission carries an epoch so a result that was
//! overtaken by a world edit is discarded instead of replacing a fresher mesh.
//!
//! Only one job per section is in flight at a time. Submitting a section that
//! is already in flight returns `false` and invalidates the running job's
//! epoch, so the section can be marked dirty and retried once the worker
//! finishes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use ob_content::Registry;
use ob_math::{ChunkPos, SectionPos};
use ob_render::{LodBatchKey, LodBatchMesh, LodConfig, MeshData, SectionSnapshot, mesh_lod_batch};
use ob_world::World;
use rayon::ThreadPool;

/// Hard upper bound on mesh worker threads.
pub const MAX_MESH_WORKERS: usize = 16;

/// A completed section mesh returned by a worker.
#[derive(Debug)]
pub struct MeshResult {
    /// Section the mesh belongs to.
    pub pos: SectionPos,
    /// Epoch assigned when the snapshot was submitted.
    pub epoch: u64,
    /// Mesh built from that snapshot.
    pub mesh: MeshData,
}

/// A completed merged far-LOD batch returned by a worker.
#[derive(Debug)]
pub struct LodBatchResult {
    /// Group the batch belongs to.
    pub key: LodBatchKey,
    /// Epoch assigned when the job was submitted.
    pub epoch: u64,
    /// Merged shell geometry and its bounds.
    pub batch: LodBatchMesh,
}

/// Internal completion message; `mesh` is moved to [`MeshResult`] only when
/// the epoch is still current.
struct Completion {
    pos: SectionPos,
    epoch: u64,
    mesh: MeshData,
}

/// Internal far-batch completion message.
struct LodCompletion {
    key: LodBatchKey,
    epoch: u64,
    batch: LodBatchMesh,
}

/// Snapshot capture and worker-pool scheduler for section meshes.
pub struct MeshScheduler {
    pool: ThreadPool,
    workers: usize,
    registry: Arc<Registry>,
    sender: Sender<Completion>,
    receiver: Receiver<Completion>,
    /// Epoch currently expected for a section. Missing after a result has been
    /// accepted or the section has been forgotten (unloaded).
    epochs: HashMap<SectionPos, u64>,
    /// Epoch of the job actually running for a section, if any.
    in_flight: HashMap<SectionPos, u64>,
    /// Far-batch job channel, kept separate from the section channel so a
    /// burst of section completions cannot delay a batch upload.
    lod_sender: Sender<LodCompletion>,
    lod_receiver: Receiver<LodCompletion>,
    /// Epoch expected for a far group.
    lod_epochs: HashMap<LodBatchKey, u64>,
    /// Epoch of the far-batch job actually running, if any.
    lod_in_flight: HashMap<LodBatchKey, u64>,
    next_epoch: u64,
    submitted: u64,
    completed: u64,
    dropped: u64,
    lod_submitted: u64,
    lod_completed: u64,
    lod_dropped: u64,
}

impl MeshScheduler {
    /// Builds a scheduler with `workers` threads (clamped to `1..=16`).
    pub fn new(workers: usize, registry: Arc<Registry>) -> Self {
        let workers = workers.clamp(1, MAX_MESH_WORKERS);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("ob-mesh-{index}"))
            .build()
            .expect("failed to create the section mesh worker pool");
        let (sender, receiver) = mpsc::channel();
        let (lod_sender, lod_receiver) = mpsc::channel();
        Self {
            pool,
            workers,
            registry,
            sender,
            receiver,
            epochs: HashMap::new(),
            in_flight: HashMap::new(),
            lod_sender,
            lod_receiver,
            lod_epochs: HashMap::new(),
            lod_in_flight: HashMap::new(),
            next_epoch: 1,
            submitted: 0,
            completed: 0,
            dropped: 0,
            lod_submitted: 0,
            lod_completed: 0,
            lod_dropped: 0,
        }
    }

    /// Captures `pos` on the calling thread and queues a worker mesh job.
    ///
    /// Returns `true` when a job was spawned. Returns `false` when a job for
    /// the section is already running; the tracked epoch is advanced so the
    /// in-flight result will be dropped, and the caller should mark the
    /// section dirty again to retry after the worker completes.
    pub fn submit(&mut self, world: &World, pos: SectionPos) -> bool {
        let epoch = self.next_epoch;
        self.next_epoch += 1;

        if self.in_flight.contains_key(&pos) {
            // The snapshot being meshed is already older than the world. Publish
            // the new epoch so the completion is rejected, but do not queue a
            // second job: the caller retries after the first one finishes.
            self.epochs.insert(pos, epoch);
            return false;
        }

        let snapshot = SectionSnapshot::capture(world, pos);
        self.epochs.insert(pos, epoch);
        self.in_flight.insert(pos, epoch);
        let sender = self.sender.clone();
        let registry = Arc::clone(&self.registry);
        self.pool.spawn(move || {
            let mesh = snapshot.mesh(&registry);
            // A disconnected receiver only means the scheduler was dropped;
            // dropping the mesh here is harmless.
            let _ = sender.send(Completion { pos, epoch, mesh });
        });
        self.submitted += 1;
        true
    }

    /// Invalidates any in-flight result for a section without queueing a new
    /// job.
    ///
    /// The client uses this for dirty sections that do not fit in the
    /// per-frame submission budget: the old snapshot no longer matches the
    /// world, so its completion must not replace a mesh, and the section stays
    /// dirty for a retry.
    pub fn invalidate(&mut self, pos: SectionPos) {
        if self.in_flight.contains_key(&pos) {
            let epoch = self.next_epoch;
            self.next_epoch += 1;
            self.epochs.insert(pos, epoch);
        }
    }

    /// Non-blocking drain of completed results.
    ///
    /// Stale results are dropped internally; the returned mesh is always the
    /// current mesh for its section. Returns `None` when no current result is
    /// ready.
    pub fn try_recv(&mut self) -> Option<MeshResult> {
        loop {
            match self.receiver.try_recv() {
                Ok(completion) => {
                    self.completed += 1;
                    if self.in_flight.get(&completion.pos) == Some(&completion.epoch) {
                        self.in_flight.remove(&completion.pos);
                    }
                    if self.epochs.get(&completion.pos) == Some(&completion.epoch) {
                        self.epochs.remove(&completion.pos);
                        return Some(MeshResult {
                            pos: completion.pos,
                            epoch: completion.epoch,
                            mesh: completion.mesh,
                        });
                    }
                    self.dropped += 1;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// Forgets every tracked epoch in a chunk column (streaming unload).
    ///
    /// In-flight jobs are left to finish and their completions are discarded
    /// because the epoch entry is gone.
    pub fn forget_chunk(&mut self, chunk: ChunkPos) {
        self.epochs.retain(|pos, _| pos.x != chunk.x || pos.z != chunk.z);
    }

    /// Forgets one section's tracked epoch.
    pub fn forget_section(&mut self, pos: SectionPos) {
        self.epochs.remove(&pos);
    }

    /// Queues a merged far-LOD batch job for a small cloned world.
    ///
    /// `world` carries the group's 2x2 columns plus one chunk of border, so the
    /// worker can cull boundary faces without touching the live world. Returns
    /// `false` when a batch for this group is already in flight; its epoch is
    /// advanced so the stale completion is discarded.
    pub fn submit_lod_batch(&mut self, key: LodBatchKey, world: World, config: LodConfig) -> bool {
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        if self.lod_in_flight.contains_key(&key) {
            self.lod_epochs.insert(key, epoch);
            return false;
        }
        self.lod_epochs.insert(key, epoch);
        self.lod_in_flight.insert(key, epoch);
        let sender = self.lod_sender.clone();
        self.pool.spawn(move || {
            let config = config.normalised();
            let batch = mesh_lod_batch(&world, key, &config);
            let _ = sender.send(LodCompletion { key, epoch, batch });
        });
        self.lod_submitted += 1;
        true
    }

    /// Invalidates an in-flight far batch without queueing a replacement.
    pub fn invalidate_lod_batch(&mut self, key: LodBatchKey) {
        if self.lod_in_flight.contains_key(&key) {
            let epoch = self.next_epoch;
            self.next_epoch += 1;
            self.lod_epochs.insert(key, epoch);
        }
    }

    /// Forgets all epoch state for a far group (unloaded or promoted to full).
    pub fn forget_lod_batch(&mut self, key: LodBatchKey) {
        self.lod_epochs.remove(&key);
    }

    /// Non-blocking drain of completed far batches.
    pub fn try_recv_lod(&mut self) -> Option<LodBatchResult> {
        loop {
            match self.lod_receiver.try_recv() {
                Ok(completion) => {
                    self.lod_completed += 1;
                    if self.lod_in_flight.get(&completion.key) == Some(&completion.epoch) {
                        self.lod_in_flight.remove(&completion.key);
                    }
                    if self.lod_epochs.get(&completion.key) == Some(&completion.epoch) {
                        self.lod_epochs.remove(&completion.key);
                        return Some(LodBatchResult {
                            key: completion.key,
                            epoch: completion.epoch,
                            batch: completion.batch,
                        });
                    }
                    self.lod_dropped += 1;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// Whether a far-batch job for `key` is running.
    pub fn lod_in_flight(&self, key: LodBatchKey) -> bool {
        self.lod_in_flight.contains_key(&key)
    }

    /// Number of far-batch jobs currently running.
    pub fn lod_in_flight_count(&self) -> usize {
        self.lod_in_flight.len()
    }

    /// Number of far-batch completions received (current and stale).
    pub fn lod_completed_count(&self) -> u64 {
        self.lod_completed
    }

    /// Number of far-batch completions discarded as stale.
    pub fn lod_dropped_count(&self) -> u64 {
        self.lod_dropped
    }

    /// Number of far batches submitted since creation.
    pub fn lod_submitted_count(&self) -> u64 {
        self.lod_submitted
    }

    /// Number of jobs currently being meshed.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Number of worker threads in the pool.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Total number of worker jobs spawned since creation.
    pub fn submitted_count(&self) -> u64 {
        self.submitted
    }

    /// Total number of worker completions received (current and stale).
    pub fn completed_count(&self) -> u64 {
        self.completed
    }

    /// Number of completions discarded because a newer epoch superseded them.
    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }

    /// Number of sections with a tracked result epoch.
    pub fn tracked_count(&self) -> usize {
        self.epochs.len()
    }

    /// Current epoch for a section, if one is tracked (tests and diagnostics).
    #[cfg(test)]
    fn epoch_of(&self, pos: SectionPos) -> Option<u64> {
        self.epochs.get(&pos).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use ob_content::{BlockId, Registry};
    use ob_math::{BlockPos, ChunkPos, SectionPos};
    use ob_world::World;
    use ob_worldgen::Generator;

    use super::*;

    /// Small deterministic world: a 3×3 block area of generated terrain.
    fn generated_world(seed: u64) -> (Arc<Registry>, World) {
        let registry = Arc::new(Registry::base());
        let generator = Generator::new(seed);
        let mut world = World::new(Arc::clone(&registry), seed);
        for z in -1..=1 {
            for x in -1..=1 {
                world.insert_chunk(generator.generate_chunk(ChunkPos::new(x, z), &registry));
            }
        }
        (registry, world)
    }

    /// A section that contains a generated surface block at column (0, 0).
    fn surface_section(world: &World) -> SectionPos {
        let height = world
            .height_at(0, 0)
            .expect("generated column (0, 0) should have a surface");
        SectionPos::new(0, height.div_euclid(16), 0)
    }

    /// Polls the scheduler until a current result arrives or the timeout expires.
    fn wait_for_result(scheduler: &mut MeshScheduler, timeout: Duration) -> Option<MeshResult> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(result) = scheduler.try_recv() {
                return Some(result);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn two_workers_mesh_a_generated_surface_section() {
        let (registry, world) = generated_world(42);
        let pos = surface_section(&world);
        let mut scheduler = MeshScheduler::new(2, registry);
        assert_eq!(scheduler.workers(), 2);
        assert!(scheduler.submit(&world, pos));

        let result = wait_for_result(&mut scheduler, Duration::from_secs(10)).expect("the worker should return a mesh");
        assert_eq!(result.pos, pos);
        assert!(
            !result.mesh.vertices.is_empty(),
            "a surface section must produce geometry"
        );
        assert_eq!(scheduler.in_flight_count(), 0);
        assert_eq!(scheduler.submitted_count(), 1);
        assert_eq!(scheduler.completed_count(), 1);
        assert_eq!(scheduler.dropped_count(), 0);
    }

    #[test]
    fn stale_epoch_is_dropped_and_only_the_latest_result_is_accepted() {
        let (registry, mut world) = generated_world(42);
        let pos = surface_section(&world);
        let mut scheduler = MeshScheduler::new(2, registry);

        assert!(scheduler.submit(&world, pos));
        let first_epoch = scheduler.epoch_of(pos).expect("first submission tracks an epoch");

        // Edit a block in the section while the first snapshot is in flight,
        // then re-submit: the second call must invalidate the running epoch.
        let height = world.height_at(0, 0).expect("surface");
        assert!(world.set_block(BlockPos::new(0, height, 0), BlockId::AIR));
        assert!(
            !scheduler.submit(&world, pos),
            "an in-flight section cannot be queued twice",
        );
        let latest_epoch = scheduler.epoch_of(pos).expect("the retry tracks a newer epoch");
        assert!(latest_epoch > first_epoch);

        // The first completion must be rejected, never surfaced as a result.
        let deadline = Instant::now() + Duration::from_secs(10);
        while scheduler.in_flight_count() > 0 {
            assert!(scheduler.try_recv().is_none(), "a stale epoch must never be returned",);
            assert!(Instant::now() < deadline, "stale worker did not finish in time");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(scheduler.dropped_count(), 1);

        // Retry now that the worker is free: the fresh snapshot is accepted.
        assert!(scheduler.submit(&world, pos));
        let expected_epoch = scheduler.epoch_of(pos).expect("retry tracks an epoch");
        assert!(expected_epoch > latest_epoch);
        let result = wait_for_result(&mut scheduler, Duration::from_secs(10)).expect("the retry should return a mesh");
        assert_eq!(result.epoch, expected_epoch);
        assert_eq!(
            result.mesh.vertices,
            SectionSnapshot::capture(&world, pos).mesh(&scheduler.registry).vertices
        );
    }

    /// Polls the scheduler until a current far-batch result arrives.
    fn wait_for_lod_result(scheduler: &mut MeshScheduler, timeout: Duration) -> Option<LodBatchResult> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(result) = scheduler.try_recv_lod() {
                return Some(result);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn lod_batch_worker_builds_a_shell_and_reports_counts() {
        let (registry, world) = generated_world(42);
        let local = World::from_columns(
            Arc::clone(&registry),
            world.seed(),
            world.chunks().map(|(_, column)| column.clone()).collect::<Vec<_>>(),
        );
        let config = LodConfig {
            full_detail_radius_chunks: 0,
            group_chunks: 2,
            shell_depth_blocks: 48,
        };
        let mut scheduler = MeshScheduler::new(2, registry);
        let key = LodBatchKey { x: 0, z: 0 };
        assert!(scheduler.submit_lod_batch(key, local, config));
        assert!(scheduler.lod_in_flight(key));
        let result = wait_for_lod_result(&mut scheduler, Duration::from_secs(10)).expect("far batch");
        assert_eq!(result.key, key);
        assert_eq!(result.batch.columns, 4, "the 2x2 group must use four columns");
        assert!(
            !result.batch.is_empty(),
            "generated terrain must produce shell geometry"
        );
        assert!(result.batch.shell_sections > 0);
        assert!(result.batch.skipped_sections > 0, "deep sections must be skipped");
        assert!(!scheduler.lod_in_flight(key));
        assert_eq!(scheduler.lod_completed_count(), 1);
        assert_eq!(scheduler.lod_submitted_count(), 1);
    }

    #[test]
    fn stale_lod_batch_epoch_is_dropped_and_retry_succeeds() {
        let (registry, world) = generated_world(7);
        let local = || {
            World::from_columns(
                Arc::clone(&registry),
                world.seed(),
                world.chunks().map(|(_, column)| column.clone()).collect::<Vec<_>>(),
            )
        };
        let config = LodConfig {
            full_detail_radius_chunks: 0,
            group_chunks: 2,
            shell_depth_blocks: 48,
        };
        let mut scheduler = MeshScheduler::new(2, Arc::clone(&registry));
        let key = LodBatchKey { x: 0, z: 0 };
        assert!(scheduler.submit_lod_batch(key, local(), config));
        scheduler.invalidate_lod_batch(key);
        let deadline = Instant::now() + Duration::from_secs(10);
        while scheduler.lod_in_flight(key) {
            assert!(
                scheduler.try_recv_lod().is_none(),
                "an invalidated batch must not surface"
            );
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(scheduler.lod_dropped_count(), 1);
        assert!(scheduler.submit_lod_batch(key, local(), config));
        let result = wait_for_lod_result(&mut scheduler, Duration::from_secs(10)).expect("retry");
        assert_eq!(result.key, key);
        assert_eq!(scheduler.lod_dropped_count(), 1, "a fresh epoch must be accepted");
    }

    #[test]
    fn submitting_an_in_flight_section_returns_false() {
        let (registry, world) = generated_world(7);
        let pos = surface_section(&world);
        let mut scheduler = MeshScheduler::new(2, registry);

        assert!(scheduler.submit(&world, pos), "the first submission should queue a job");
        assert!(
            !scheduler.submit(&world, pos),
            "the second submission must skip the in-flight section",
        );
        assert_eq!(scheduler.in_flight_count(), 1);
        assert_eq!(scheduler.submitted_count(), 1);

        // The second submit invalidated the running epoch, so draining must
        // discard its result; finish the test with a clean state.
        let deadline = Instant::now() + Duration::from_secs(10);
        while scheduler.in_flight_count() > 0 {
            let _ = scheduler.try_recv();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(scheduler.dropped_count(), 1);
    }
}

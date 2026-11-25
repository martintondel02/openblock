//! Client-side far-terrain scheduling.
//!
//! The renderer owns a neutral heightfield mesh; this module owns the policy
//! that keeps it on screen without ever blocking the frame:
//!
//! * build on a background thread, at most one build in flight;
//! * rebuild only after the camera has moved past
//!   [`ob_render::FAR_RECENTER_HYSTERESIS`] (or the seed/radii changed);
//! * throttle rebuild starts, so flying does not start a new 8,192-block
//!   heightfield every frame;
//! * discard a finished build that is already too far behind a teleport or a
//!   fast flight, instead of uploading stale geometry;
//! * upload the finished CPU mesh on the main thread (a bounded buffer write).
//!
//! The far mesh is visual only: it is built from
//! [`ob_worldgen::Generator::surface_sample`] for the current seed but never
//! feeds collision, streaming, spawning, persistence or anti-cheat. A build
//! failure leaves the previous mesh in place.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ob_math::SEA_LEVEL;
use ob_render::{FarBiome, FarSample, FarTerrainBuild, FarTerrainConfig, Renderer, build_far_terrain};
use ob_worldgen::Generator;

/// Minimum wall time between two far-terrain rebuild starts.
///
/// Hysteresis already limits ordinary walking to one rebuild every few seconds;
/// this throttle mainly protects against fast flight and repeated setting
/// changes, which could otherwise queue back-to-back 8,192-block builds.
pub(crate) const FAR_BUILD_MIN_INTERVAL: Duration = Duration::from_millis(350);

/// Whether a mesh centred on `center` should be rebuilt for a camera at `eye`.
///
/// Pure so the recenter policy can be unit-tested without a GPU.
pub(crate) fn needs_recenter(center: [f64; 2], eye: [f64; 2], hysteresis: f32) -> bool {
    let dx = eye[0] - center[0];
    let dz = eye[1] - center[1];
    dx * dx + dz * dz > f64::from(hysteresis) * f64::from(hysteresis)
}

/// Maps a worldgenerator biome to the renderer's neutral far-surface palette.
pub(crate) fn far_biome(biome: ob_worldgen::Biome) -> FarBiome {
    match biome {
        ob_worldgen::Biome::Plains => FarBiome::Plains,
        ob_worldgen::Biome::Forest => FarBiome::Forest,
        ob_worldgen::Biome::Desert => FarBiome::Desert,
        ob_worldgen::Biome::SnowyTaiga => FarBiome::SnowyTaiga,
        ob_worldgen::Biome::Mountains => FarBiome::Mountains,
    }
}

/// Builds one far-terrain mesh synchronously.
///
/// The background worker and the offscreen screenshot/F2 paths share this
/// function, so the windowed game, screenshots and the benchmark all use the
/// same geometry. The 8,192-block default build costs roughly 40k cheap column
/// samples, parallelised on the rayon pool.
pub(crate) fn build_now(
    seed: u64,
    center: [f64; 2],
    view_distance_chunks: i32,
    simulation_distance_chunks: i32,
) -> FarTerrainBuild {
    let config = FarTerrainConfig::new(view_distance_chunks, simulation_distance_chunks, SEA_LEVEL as f32 + 1.0);
    let generator = Generator::new(seed);
    build_far_terrain(
        seed,
        config,
        move |x, z| {
            let sample = generator.surface_sample(x, z);
            FarSample {
                height: sample.height,
                biome: far_biome(sample.biome),
            }
        },
        center,
    )
}

/// Live far-terrain mesh and worker state for one runtime.
pub(crate) struct FarTerrainState {
    config: FarTerrainConfig,
    seed: u64,
    center: Option<[f64; 2]>,
    stats: Option<ob_render::FarTerrainStats>,
    in_flight: Option<Receiver<FarTerrainBuild>>,
    last_started: Option<Instant>,
    last_completed: Option<Instant>,
    last_eye: [f64; 2],
}

impl Default for FarTerrainState {
    fn default() -> Self {
        Self::new()
    }
}

impl FarTerrainState {
    /// Creates an idle state; the first [`Self::maintain`] call starts a build.
    pub(crate) fn new() -> Self {
        Self {
            config: FarTerrainConfig::new(128, 6, SEA_LEVEL as f32 + 1.0).normalised(),
            seed: 0,
            center: None,
            stats: None,
            in_flight: None,
            last_started: None,
            last_completed: None,
            last_eye: [0.0, 0.0],
        }
    }

    /// Drops the mesh and any in-flight build (world unload, disconnect,
    /// renderer reset or seed change).
    pub(crate) fn reset(&mut self, renderer: &mut Renderer) {
        self.in_flight = None;
        self.center = None;
        self.stats = None;
        self.last_started = None;
        self.last_completed = None;
        renderer.clear_far_terrain();
    }

    /// Current mesh centre, if a build has been uploaded.
    pub(crate) fn center(&self) -> Option<[f64; 2]> {
        self.center
    }

    /// Latest build counters, if a build has been uploaded.
    pub(crate) fn stats(&self) -> Option<&ob_render::FarTerrainStats> {
        self.stats.as_ref()
    }

    /// Whether a worker build is currently running.
    pub(crate) fn is_building(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Starts, polls and uploads far-terrain work without blocking.
    ///
    /// Call once per frame with the camera eye and the current settings. The
    /// method only performs GPU work when a finished build is ready; the
    /// expensive sampling and meshing always runs on the worker thread.
    pub(crate) fn maintain(
        &mut self,
        renderer: &mut Renderer,
        seed: u64,
        view_distance_chunks: i32,
        simulation_distance_chunks: i32,
        eye: [f64; 2],
    ) {
        let config = FarTerrainConfig::new(view_distance_chunks, simulation_distance_chunks, SEA_LEVEL as f32 + 1.0)
            .normalised();
        self.last_eye = eye;
        if config != self.config || seed != self.seed {
            self.config = config;
            self.seed = seed;
            self.reset(renderer);
        }

        // 1. Collect a finished worker build (if any) and upload it.
        if let Some(receiver) = &self.in_flight {
            match receiver.try_recv() {
                Ok(build) => {
                    self.in_flight = None;
                    self.last_completed = Some(Instant::now());
                    // A teleport or a long flight can leave a build centred far
                    // behind the camera. Uploading it would show a stale horizon
                    // for a frame and waste the buffer write; discard instead and
                    // request a fresh build on the next maintain pass.
                    let stale = needs_recenter(build.mesh.center, self.last_eye, self.config.hysteresis * 1.5);
                    if stale {
                        log::debug!(
                            "discarded stale far-terrain build at {:?} (camera {:?})",
                            build.mesh.center,
                            self.last_eye
                        );
                    } else {
                        self.center = Some(build.mesh.center);
                        log::info!(
                            "far terrain uploaded: {} tiers, {} quads, {} vertices, {:.1} ms, centre {:.0}/{:.0}",
                            build.stats.tiers.len(),
                            build.stats.opaque_quads,
                            build.stats.opaque_vertices,
                            build.stats.build_ms,
                            build.mesh.center[0],
                            build.mesh.center[1],
                        );
                        self.stats = Some(build.stats);
                        renderer.set_far_terrain(build.mesh);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = None;
                    log::warn!("far-terrain worker ended without a result");
                }
            }
        }

        // 2. Decide whether another build is needed.
        let needs = self.in_flight.is_none()
            && self
                .center
                .is_none_or(|center| needs_recenter(center, eye, self.config.hysteresis));
        if !needs {
            return;
        }
        if self
            .last_started
            .is_some_and(|started| started.elapsed() < FAR_BUILD_MIN_INTERVAL)
        {
            return;
        }

        // 3. Hand the expensive work to a named worker thread. The generator
        //    is Copy and the sampler is a pure function of the seed and XZ.
        let config = self.config;
        let seed = self.seed;
        let center = eye;
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ob-far-terrain".to_string())
            .spawn(move || {
                let build = build_now(
                    seed,
                    center,
                    config.view_distance_chunks,
                    config.simulation_distance_chunks,
                );
                // A closed receiver means the world was reset; dropping the
                // result is the correct, cheap outcome.
                let _ = sender.send(build);
            });
        match spawned {
            Ok(_handle) => {
                self.in_flight = Some(receiver);
                self.last_started = Some(Instant::now());
            }
            Err(error) => {
                log::warn!("could not start far-terrain worker: {error}");
                self.last_started = Some(Instant::now());
            }
        }
    }
}

/// One-line summary of a synchronous build for logs and screenshots.
pub(crate) fn describe_build(build: &FarTerrainBuild) -> String {
    format!(
        "{} tiers, {} quads, {} vertices, {} water triangles in {:.1} ms centered at {:.0}/{:.0}",
        build.stats.tiers.len(),
        build.stats.opaque_quads,
        build.stats.opaque_vertices,
        build.stats.water_triangles,
        build.stats.build_ms,
        build.mesh.center[0],
        build.mesh.center[1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_render::{FAR_RECENTER_HYSTERESIS, FarTerrainMesh, FarTerrainVertex};

    #[test]
    fn recenter_hysteresis_is_a_circle() {
        let center = [0.0, 0.0];
        assert!(!needs_recenter(center, [50.0, 0.0], FAR_RECENTER_HYSTERESIS));
        assert!(!needs_recenter(center, [0.0, -96.0], FAR_RECENTER_HYSTERESIS));
        assert!(needs_recenter(center, [97.0, 0.0], FAR_RECENTER_HYSTERESIS));
        assert!(needs_recenter(center, [70.0, 70.0], FAR_RECENTER_HYSTERESIS));
        assert!(!needs_recenter(center, [-10.0, 12.0], FAR_RECENTER_HYSTERESIS));
    }

    #[test]
    fn synchronous_build_is_deterministic_and_uses_the_requested_center() {
        let first = build_now(42, [128.0, -64.0], 128, 6);
        let second = build_now(42, [128.0, -64.0], 128, 6);
        assert_eq!(first.mesh.vertices, second.mesh.vertices);
        assert_eq!(first.mesh.indices, second.mesh.indices);
        assert_eq!(first.mesh.center, [128.0, -64.0]);
        assert_eq!(first.stats.center, [128.0, -64.0]);
        assert!(!first.stats.tiers.is_empty());
        assert_eq!(
            first.mesh.indices.len() as u64,
            first.stats.opaque_quads * 6,
            "index and quad counters must agree"
        );
        assert_eq!(first.mesh.vertices.len() as u64, first.stats.opaque_vertices);
        assert!(first.stats.opaque_quads < 200_000);
        assert!(first.mesh.min[0] <= -1900.0 && first.mesh.max[0] >= 1900.0);
        for vertex in &first.mesh.vertices {
            assert!(vertex.position[1].is_finite());
        }
    }

    #[test]
    fn far_vertices_are_compact_and_finite() {
        assert_eq!(std::mem::size_of::<FarTerrainVertex>(), 36);
        let build = build_now(7, [0.0, 0.0], 512, 32);
        assert!(build.stats.tiers.len() <= 5);
        assert!(build.stats.opaque_quads <= 200_000);
        assert!(build.mesh.max[0] >= 8_000.0);
    }

    #[test]
    fn biome_mapping_covers_every_generator_biome() {
        for biome in ob_worldgen::Biome::ALL {
            let mapped = far_biome(biome);
            assert!(matches!(
                mapped,
                FarBiome::Plains | FarBiome::Forest | FarBiome::Desert | FarBiome::SnowyTaiga | FarBiome::Mountains
            ));
        }
        assert_eq!(far_biome(ob_worldgen::Biome::Desert), FarBiome::Desert);
        assert_eq!(far_biome(ob_worldgen::Biome::Forest), FarBiome::Forest);
    }

    #[test]
    fn describe_build_reports_every_counter() {
        let build = build_now(1, [0.0, 0.0], 64, 6);
        let text = describe_build(&build);
        assert!(text.contains("tiers"));
        assert!(text.contains("quads"));
        assert!(text.contains("vertices"));
        assert!(text.contains("water triangles"));
        assert!(text.contains("ms centered at"));
    }

    #[test]
    fn default_state_is_idle_and_has_no_debug_line() {
        let state = FarTerrainState::new();
        assert!(!state.is_building());
        assert!(state.center().is_none());
        assert!(state.stats().is_none());
    }

    #[test]
    fn mesh_from_build_has_no_worldgen_types() {
        // Compile-time proof that the renderer-facing mesh is neutral: it can
        // be constructed without importing ob_worldgen.
        let mesh = FarTerrainMesh::default();
        assert!(mesh.is_empty());
    }
}

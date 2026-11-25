//! Headless 512-block (32-chunk) render-distance benchmark.
//!
//! The harness opens a real [`ob_server::Host`], streams a full 65×65 column
//! world, builds the same distance-LOD scene the client uses (or, with
//! `--full-detail`, the pre-LOD per-section scene for comparison), then renders
//! `--frames` cached offscreen frames at 1280×720 and reads every frame back so
//! the GPU has completed before the timer stops.
//!
//! Meshing is parallel with rayon, matching the client's worker pool rather
//! than the sequential cache builder in the older `bench` subcommand. The
//! measured frame time therefore includes CPU submission, rasterisation and the
//! GPU→CPU readback; it does *not* include meshing or buffer upload. No window
//! is opened and no timestamps or display state are required.
//!
//! The PNG uses the Classic procedural texture pack and an elevated camera so
//! the 512-block horizon is visible.

use std::collections::BTreeSet;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ob_content::Registry;
use ob_math::{BlockPos, ChunkPos, N_SECTIONS, SECTION_SIZE, SectionPos, WORLD_MIN_Y, WorldVec};
use ob_render::{
    Camera, DEFAULT_FULL_DETAIL_RADIUS_CHUNKS, DEFAULT_LOD_GROUP_CHUNKS, DEFAULT_SHELL_DEPTH_BLOCKS, LodBatchKey,
    LodConfig, LodTier, OffscreenRenderer, TexturePackStyle, far_plane_for_render_distance, mesh_lod_batch,
    mesh_section,
};
use ob_server::{Host, HostConfig};
use ob_world::World;
use rayon::prelude::*;

use crate::measure::{self, Stats};

/// Default horizontal radius in chunks (512 blocks).
pub const DEFAULT_RADIUS: i32 = 32;
/// Columns generated/sent per streaming call in the harness.
const STREAM_BUDGET: usize = 64;
/// Safety bound on streaming calls (65×65 columns at 64 per call is 67).
const STREAM_GUARD: usize = 2000;
/// Section meshes uploaded per parallel batch, bounding transient CPU memory.
const MESH_BATCH: usize = 256;
/// Far groups meshed per parallel batch.
const GROUP_BATCH: usize = 24;

/// Options for one render-distance benchmark run.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Streaming radius in chunks.
    pub radius: i32,
    /// Measured frames.
    pub frames: u32,
    /// Unmeasured warm-up frames.
    pub warmup: u32,
    /// Offscreen width.
    pub width: u32,
    /// Offscreen height.
    pub height: u32,
    /// World seed.
    pub seed: u64,
    /// Disable the distance LOD (baseline comparison).
    pub full_detail: bool,
    /// Screenshot path; `Some` writes the last frame as PNG.
    pub png: Option<PathBuf>,
    /// Explicit data directory; `None` uses a self-cleaning temp directory.
    pub data_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            radius: DEFAULT_RADIUS,
            frames: 120,
            warmup: 10,
            width: 1280,
            height: 720,
            seed: 42,
            full_detail: false,
            png: Some(PathBuf::from("/tmp/openblock-512.png")),
            data_dir: None,
        }
    }
}

/// Cache and frame counters reported by the harness.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Adapter description.
    pub adapter: String,
    /// CPU model string.
    pub cpu: String,
    /// OS description.
    pub os: String,
    /// Loaded chunk columns.
    pub columns: usize,
    /// Loaded sections (all, including empty).
    pub world_sections: usize,
    /// World heap estimate.
    pub world_heap_bytes: usize,
    /// Streaming wall time.
    pub stream_ms: f64,
    /// Columns generated during streaming.
    pub streamed_columns: usize,
    /// Streaming calls made.
    pub stream_calls: usize,
    /// Scene build (meshing + upload) wall time.
    pub mesh_ms: f64,
    /// Full-detail section meshes cached.
    pub cache_sections: u32,
    /// Far batches cached.
    pub cache_batches: u32,
    /// Shell sections represented by far batches.
    pub far_sections: u64,
    /// Sections omitted from far columns by the shell.
    pub skipped_sections: u64,
    /// Vertices in all cached geometry.
    pub cache_vertices: u64,
    /// Last frame draw calls.
    pub draw_calls: u32,
    /// Last frame full section draws.
    pub drawn_sections: u32,
    /// Last frame far batch draws.
    pub drawn_batches: u32,
    /// Last frame culled section meshes.
    pub culled_sections: u32,
    /// Last frame culled far batches.
    pub culled_batches: u32,
    /// Last frame vertices submitted.
    pub drawn_vertices: u64,
    /// Frame-time samples.
    pub frames: Stats,
    /// RSS after the scene build.
    pub rss_after_build_mib: f64,
    /// Peak RSS after measured frames.
    pub peak_rss_mib: f64,
    /// Estimated CPU+GPU vertex data bytes.
    pub mesh_bytes: u64,
    /// Screenshot path, when written.
    pub png: Option<PathBuf>,
}

/// Parses benchmark flags on top of [`Options::default`].
pub fn options_from_flags(flags: &[(String, String)]) -> Result<Options, String> {
    let mut options = Options::default();
    for (name, value) in flags {
        match name.as_str() {
            "render-distance" | "radius" => {
                let raw = required(name, value)?;
                options.radius = raw.parse().map_err(|_| format!("invalid --{name} value '{raw}'"))?;
                if !(1..=32).contains(&options.radius) {
                    return Err("--render-distance must be between 1 and 32 chunks".to_string());
                }
            }
            "frames" => {
                let raw = required(name, value)?;
                options.frames = raw.parse().map_err(|_| format!("invalid --frames value '{raw}'"))?;
                if options.frames == 0 {
                    return Err("--frames must be at least 1".to_string());
                }
            }
            "warmup" => {
                let raw = required(name, value)?;
                options.warmup = raw.parse().map_err(|_| format!("invalid --warmup value '{raw}'"))?;
            }
            "width" => {
                let raw = required(name, value)?;
                options.width = raw.parse().map_err(|_| format!("invalid --width value '{raw}'"))?;
                if options.width == 0 {
                    return Err("--width must be at least 1".to_string());
                }
            }
            "height" => {
                let raw = required(name, value)?;
                options.height = raw.parse().map_err(|_| format!("invalid --height value '{raw}'"))?;
                if options.height == 0 {
                    return Err("--height must be at least 1".to_string());
                }
            }
            "seed" => {
                let raw = required(name, value)?;
                options.seed = raw.parse().map_err(|_| format!("invalid --seed value '{raw}'"))?;
            }
            "png" => options.png = Some(PathBuf::from(required(name, value)?)),
            "no-png" => options.png = None,
            "data-dir" => options.data_dir = Some(PathBuf::from(required(name, value)?)),
            "full-detail" => options.full_detail = true,
            "help" | "version" => {}
            other => return Err(format!("unknown option --{other}")),
        }
    }
    Ok(options)
}

/// Runs the benchmark and prints a human-readable report.
pub fn run_from_flags(flags: &[(String, String)]) -> Result<(), String> {
    let options = options_from_flags(flags)?;
    if flags.iter().any(|(name, _)| name == "help") {
        println!("{}", usage());
        return Ok(());
    }
    run(&options).map(|report| print!("{}", format_report(&options, &report)))
}

/// Runs one benchmark.
pub fn run(options: &Options) -> Result<Report, String> {
    if options.radius < 1 || options.radius > 32 {
        return Err("render distance must be between 1 and 32 chunks".to_string());
    }
    let temp = if options.data_dir.is_none() {
        Some(measure::temp_dir("openblock-rd512").map_err(|error| format!("temp dir: {error}"))?)
    } else {
        None
    };
    let data_dir = options
        .data_dir
        .clone()
        .unwrap_or_else(|| temp.as_ref().expect("temp dir").path().to_path_buf());

    let registry = Arc::new(Registry::base());
    let config = HostConfig {
        seed: options.seed,
        world_name: "rd512".to_string(),
        data_dir: Some(data_dir.clone()),
        render_distance_chunks: options.radius,
        generate_budget: STREAM_BUDGET,
    };
    let mut host = Host::open(registry, config).map_err(|error| format!("open host: {error}"))?;

    let stream_start = Instant::now();
    let mut streamed_columns = 0usize;
    let mut stream_calls = 0usize;
    loop {
        let generated = host.ensure_streaming(STREAM_BUDGET);
        streamed_columns += generated;
        if generated == 0 {
            break;
        }
        stream_calls += 1;
        if stream_calls >= STREAM_GUARD {
            return Err(format!(
                "streaming did not converge after {STREAM_GUARD} calls ({} columns)",
                host.world().chunk_count()
            ));
        }
    }
    let stream_ms = millis(stream_start);

    let columns = host.world().chunk_count();
    let world_sections = host.world().section_count();
    let world_heap_bytes = host.world().heap_bytes();
    let _dirty = host.take_dirty_sections();

    // A slightly elevated viewpoint makes the 512-block horizon visible and
    // conservatively increases the visible section count. It is still a real
    // world position above the spawn column, not a synthetic test box.
    let player = host.player();
    let eye = WorldVec::new(player.pos.x, player.eye().y + 18.0, player.pos.z);
    let mut camera = Camera::new(eye, player.yaw, -0.10);
    camera.far = far_plane_for_render_distance(options.radius);
    let camera_chunk = BlockPos::new(
        camera.position.x.floor() as i32,
        camera.position.y.floor() as i32,
        camera.position.z.floor() as i32,
    )
    .chunk();

    let mut renderer = OffscreenRenderer::new_with_style(options.width, options.height, TexturePackStyle::Classic)
        .map_err(|error| format!("open offscreen renderer: {error}"))?;
    let adapter = renderer.adapter_summary().to_string();

    let mesh_start = Instant::now();
    let mut report = Report {
        adapter,
        cpu: measure::cpu_model(),
        os: measure::os_description(),
        columns,
        world_sections,
        world_heap_bytes,
        stream_ms,
        streamed_columns,
        stream_calls,
        ..Report::default()
    };
    let lod = LodConfig {
        full_detail_radius_chunks: options.radius.min(DEFAULT_FULL_DETAIL_RADIUS_CHUNKS),
        group_chunks: DEFAULT_LOD_GROUP_CHUNKS,
        shell_depth_blocks: DEFAULT_SHELL_DEPTH_BLOCKS,
    }
    .normalised();
    if options.full_detail {
        build_full_detail_scene(&mut renderer, host.world(), &mut report)?;
    } else {
        build_lod_scene(&mut renderer, host.world(), camera_chunk, &lod, &mut report)?;
    }
    report.mesh_ms = millis(mesh_start);
    report.mesh_bytes = report.cache_vertices * std::mem::size_of::<ob_render::Vertex>() as u64 * 2;
    report.rss_after_build_mib = measure::rss_kib().map_or(0.0, |kib| kib as f64 / 1024.0);

    for _ in 0..options.warmup {
        renderer
            .render_cached(&camera)
            .map_err(|error| format!("warm-up frame: {error}"))?;
    }
    let mut samples = Vec::with_capacity(options.frames as usize);
    let mut last_stats = ob_render::RenderStats::default();
    for _ in 0..options.frames {
        let start = Instant::now();
        last_stats = renderer
            .render_cached(&camera)
            .map_err(|error| format!("measured frame: {error}"))?;
        samples.push(millis(start));
    }
    report.frames = Stats::from_samples(&samples);
    report.draw_calls = last_stats.draw_calls;
    report.drawn_sections = last_stats.sections;
    report.drawn_batches = last_stats.lod_batches;
    report.culled_sections = last_stats.culled_sections;
    report.culled_batches = last_stats.culled_lod_batches;
    report.drawn_vertices = last_stats.vertices;
    report.peak_rss_mib = measure::peak_rss_kib().map_or(0.0, |kib| kib as f64 / 1024.0);

    if let Some(path) = &options.png {
        write_png(path, options.width, options.height, &renderer.read_rgba())
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        report.png = Some(path.clone());
    }
    Ok(report)
}

/// Meshes and uploads the LOD scene matching the client's tier policy.
fn build_lod_scene(
    renderer: &mut OffscreenRenderer,
    world: &World,
    camera_chunk: ChunkPos,
    config: &LodConfig,
    report: &mut Report,
) -> Result<(), String> {
    let base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
    let mut near = Vec::new();
    let mut far = BTreeSet::new();
    for (chunk, _) in world.chunks() {
        if config.chunk_tier(camera_chunk, *chunk) == LodTier::Shell {
            far.insert(LodBatchKey::containing(*chunk, config.group_chunks));
        } else {
            for index in 0..N_SECTIONS {
                let pos = SectionPos::new(chunk.x, base_y + index as i32, chunk.z);
                if world.section(pos).is_some_and(|section| !section.is_empty()) {
                    near.push(pos);
                }
            }
        }
    }
    near.sort_unstable();
    for batch in near.chunks(MESH_BATCH) {
        let meshes: Vec<(SectionPos, ob_render::MeshData)> = batch
            .par_iter()
            .filter_map(|pos| {
                let mesh = mesh_section(world, *pos);
                (!mesh.is_empty()).then_some((*pos, mesh))
            })
            .collect();
        for (pos, mesh) in meshes {
            report.cache_sections += 1;
            report.cache_vertices += mesh.vertices.len() as u64;
            renderer.upload_mesh(pos, mesh);
        }
    }
    let far: Vec<LodBatchKey> = far.into_iter().collect();
    for batch in far.chunks(GROUP_BATCH) {
        let built: Vec<(LodBatchKey, ob_render::LodBatchMesh)> = batch
            .par_iter()
            .map(|key| (*key, mesh_lod_batch(world, *key, config)))
            .collect();
        for (key, batch) in built {
            if batch.is_empty() {
                continue;
            }
            report.cache_batches += 1;
            report.far_sections += u64::from(batch.shell_sections);
            report.skipped_sections += u64::from(batch.skipped_sections);
            report.cache_vertices += batch.mesh.vertices.len() as u64;
            renderer.upload_lod_batch(key, batch.min, batch.max, batch.mesh);
        }
    }
    Ok(())
}

/// Meshes and uploads every non-empty section (pre-LOD baseline).
fn build_full_detail_scene(renderer: &mut OffscreenRenderer, world: &World, report: &mut Report) -> Result<(), String> {
    let base_y = WORLD_MIN_Y / SECTION_SIZE as i32;
    let mut positions = Vec::new();
    for (chunk, _) in world.chunks() {
        for index in 0..N_SECTIONS {
            let pos = SectionPos::new(chunk.x, base_y + index as i32, chunk.z);
            if world.section(pos).is_some_and(|section| !section.is_empty()) {
                positions.push(pos);
            }
        }
    }
    positions.sort_unstable();
    for batch in positions.chunks(MESH_BATCH) {
        let meshes: Vec<(SectionPos, ob_render::MeshData)> = batch
            .par_iter()
            .filter_map(|pos| {
                let mesh = mesh_section(world, *pos);
                (!mesh.is_empty()).then_some((*pos, mesh))
            })
            .collect();
        for (pos, mesh) in meshes {
            report.cache_sections += 1;
            report.cache_vertices += mesh.vertices.len() as u64;
            renderer.upload_mesh(pos, mesh);
        }
    }
    Ok(())
}

/// Prints the human-readable benchmark report.
pub fn format_report(options: &Options, report: &Report) -> String {
    let mut out = String::new();
    out.push_str("OpenBlock 512-block render-distance benchmark\n");
    out.push_str(&format!(
        "  world: seed {}, render distance {} chunks ({} blocks), {} columns, {} sections\n",
        options.seed,
        options.radius,
        options.radius * 16,
        report.columns,
        report.world_sections
    ));
    out.push_str(&format!(
        "  scene: {} near sections + {} far batches, {} vertices, {} shell sections, {} skipped sections\n",
        report.cache_sections,
        report.cache_batches,
        report.cache_vertices,
        report.far_sections,
        report.skipped_sections
    ));
    out.push_str(&format!(
        "  targets: {}x{}, {} measured frames ({} warm-up), {}\n",
        options.width,
        options.height,
        options.frames,
        options.warmup,
        if options.full_detail {
            "full detail, no LOD"
        } else {
            "distance LOD"
        }
    ));
    out.push_str(&format!("  adapter: {}\n", report.adapter));
    out.push_str(&format!("  CPU: {}\n", report.cpu));
    out.push_str(&format!("  OS: {}\n", report.os));
    out.push_str(&format!(
        "  stream: {} columns in {:.1} ms ({} calls, {} generated)\n",
        report.columns, report.stream_ms, report.stream_calls, report.streamed_columns
    ));
    out.push_str(&format!(
        "  mesh+upload: {:.1} ms (parallel, {} threads available)\n",
        report.mesh_ms,
        rayon::current_num_threads()
    ));
    out.push_str(&format!(
        "  frame_ms: p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2}  mean {:.2}  -> {:.1} FPS\n",
        report.frames.p50_ms,
        report.frames.p95_ms,
        report.frames.p99_ms,
        report.frames.max_ms,
        report.frames.mean_ms,
        report.frames.per_second()
    ));
    out.push_str(&format!(
        "  last frame: {} draws ({} sections + {} batches), {} vertices, culled {} sections + {} batches\n",
        report.draw_calls,
        report.drawn_sections,
        report.drawn_batches,
        report.drawn_vertices,
        report.culled_sections,
        report.culled_batches
    ));
    out.push_str(&format!(
        "  memory: world heap {:.1} MiB, mesh estimate {:.1} MiB, RSS after build {:.0} MiB, peak {:.0} MiB\n",
        report.world_heap_bytes as f64 / (1024.0 * 1024.0),
        report.mesh_bytes as f64 / (1024.0 * 1024.0),
        report.rss_after_build_mib,
        report.peak_rss_mib
    ));
    if let Some(path) = &report.png {
        out.push_str(&format!("  png: {}\n", path.display()));
    }
    out
}

/// Usage text for the binary.
pub fn usage() -> &'static str {
    "usage: render-distance-bench [options]\n\
     options:\n\
       --render-distance N   streaming radius in chunks, 1..=32 (default 32)\n\
       --frames N            measured frames (default 120)\n\
       --warmup N            unmeasured warm-up frames (default 10)\n\
       --width W             target width (default 1280)\n\
       --height H            target height (default 720)\n\
       --seed S              world seed (default 42)\n\
       --full-detail         disable LOD and mesh every section (baseline)\n\
       --png PATH            screenshot path (default /tmp/openblock-512.png)\n\
       --no-png              skip the screenshot\n\
       --data-dir DIR        keep the generated world here (default: temp dir)\n"
}

fn required<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    if value.is_empty() {
        Err(format!("missing value for --{name}"))
    } else {
        Ok(value)
    }
}

fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let file = std::fs::File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer.write_image_data(rgba).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_the_512_block_profile() {
        let options = Options::default();
        assert_eq!(options.radius, 32);
        assert_eq!(options.frames, 120);
        assert_eq!(options.width, 1280);
        assert_eq!(options.height, 720);
        assert!(!options.full_detail);
        assert_eq!(options.png.as_deref(), Some(Path::new("/tmp/openblock-512.png")));
    }

    #[test]
    fn flags_override_and_validate() {
        let options = options_from_flags(&flags(&[
            ("render-distance", "16"),
            ("frames", "3"),
            ("warmup", "0"),
            ("width", "640"),
            ("height", "360"),
            ("seed", "7"),
            ("full-detail", ""),
            ("no-png", ""),
            ("data-dir", "/tmp/rd"),
        ]))
        .expect("flags parse");
        assert_eq!(options.radius, 16);
        assert_eq!(options.frames, 3);
        assert!(options.full_detail);
        assert_eq!(options.png, None);
        assert_eq!(options.data_dir.as_deref(), Some(Path::new("/tmp/rd")));

        assert!(options_from_flags(&flags(&[("render-distance", "0")])).is_err());
        assert!(options_from_flags(&flags(&[("render-distance", "33")])).is_err());
        assert!(options_from_flags(&flags(&[("frames", "0")])).is_err());
        assert!(options_from_flags(&flags(&[("width", "0")])).is_err());
        assert!(options_from_flags(&flags(&[("nope", "1")])).is_err());
        assert!(options_from_flags(&flags(&[("png", "")])).is_err());
    }

    #[test]
    fn report_names_every_measurement() {
        let report = Report {
            adapter: "RADV".into(),
            cpu: "CPU".into(),
            os: "Linux".into(),
            columns: 4225,
            world_sections: 101400,
            stream_ms: 123.4,
            mesh_ms: 456.7,
            cache_sections: 1000,
            cache_batches: 200,
            cache_vertices: 1_000_000,
            frames: Stats {
                count: 3,
                mean_ms: 10.0,
                p50_ms: 9.0,
                p95_ms: 12.0,
                p99_ms: 13.0,
                max_ms: 14.0,
                ..Stats::default()
            },
            png: Some(PathBuf::from("/tmp/openblock-512.png")),
            ..Report::default()
        };
        let text = format_report(&Options::default(), &report);
        for field in [
            "world:",
            "scene:",
            "targets:",
            "adapter:",
            "CPU:",
            "OS:",
            "stream:",
            "mesh+upload:",
            "frame_ms:",
            "last frame:",
            "memory:",
            "png:",
        ] {
            assert!(text.contains(field), "missing {field}:\n{text}");
        }
    }
}

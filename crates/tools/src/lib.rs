//! Offline tooling: asset compiler, content pack compiler, world pregenerators, inspectors and
//! the CPU benchmark/soak harnesses.
//!
//! The `openblock-tools` binary exposes `version`, `inspect-world` and the offscreen `bench`.
//! The 1.0 CPU measurement tools are separate binaries:
//!
//! * `worldgen_bench` - deterministic chunk-generation throughput and content densities.
//! * `server_bench` - authoritative `ob_server::Host` tick cost across idle, player, mob,
//!   item, container and streaming workloads.
//! * `save_bench` - world/sidecar save size, save and load latency, and legacy save migration.
//! * `soak` - long headless scripted session with invariant, cap and save/reload checks.
//!
//! [`mod@measure`] holds the shared percentile, RSS and temp-directory helpers.
//! [`mod@bench`] is a fixed-seed, CPU-timed synthetic frame loop that renders through
//! [`ob_render::OffscreenRenderer`]. It needs a working GPU adapter but never opens a window.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod measure;

pub mod bench {
    //! Reproducible offscreen benchmark harness behind `openblock-tools bench`.
    //!
    //! The harness opens the same [`ob_server::Host`] the game uses, drains streaming, meshes
    //! every drawn section once to record geometry counts and mesh time, then times whole
    //! offscreen frames one at a time. It writes no window, no timestamp and no machine-specific
    //! path, so two runs of the same build differ only in the measured samples.
    //!
    //! The v0.0.1 [`ob_render::OffscreenRenderer`] rebuilds and uploads section buffers inside
    //! every `render_world` call and reads the colour target back before returning, so the
    //! measured frames include that mesh/upload/readback work as well as the GPU pass. `mesh_ms`
    //! is the separate one-time mesh pass; the public offscreen API does not expose an upload-only
    //! step, so upload time cannot be isolated from the measured frames.

    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use ob_content::Registry;
    use ob_render::{Camera, OffscreenRenderer};
    use ob_server::{Host, HostConfig};

    /// Identifier written as `schema` in the benchmark JSON report.
    pub const SCHEMA: &str = "openblock.bench/0";

    /// Chunk columns generated per streaming call while the harness drains the world.
    const STREAM_BUDGET: usize = 8;

    /// Upper bound on streaming calls, matching the client screenshot path.
    const STREAM_GUARD: usize = 500;

    /// Fixed world name under the bench data directory.
    const WORLD_NAME: &str = "bench";

    /// Command-line options of the `bench` subcommand.
    #[derive(Debug, Clone, PartialEq)]
    pub struct BenchOptions {
        /// Number of measured frames (at least one).
        pub frames: u32,
        /// Number of unmeasured warm-up frames.
        pub warmup: u32,
        /// Offscreen target width in pixels.
        pub width: u32,
        /// Offscreen target height in pixels.
        pub height: u32,
        /// Seed used when a new bench world is created.
        pub seed: u64,
        /// Streaming radius in chunks; also sets the camera far plane to `R * 16 + 64` blocks.
        pub render_distance: i32,
        /// Root of the data directory holding the bench world save.
        pub data_dir: PathBuf,
        /// Optional path of the JSON report.
        pub json: Option<PathBuf>,
        /// Optional p95 frame-time limit in milliseconds.
        pub max_frame_ms: Option<f64>,
    }

    impl Default for BenchOptions {
        fn default() -> Self {
            Self {
                frames: 60,
                warmup: 5,
                width: 1280,
                height: 720,
                seed: 42,
                render_distance: 8,
                data_dir: PathBuf::from("target/bench-data"),
                json: None,
                max_frame_ms: None,
            }
        }
    }

    /// Measurements of one benchmark run.
    #[derive(Debug, Clone, PartialEq)]
    pub struct BenchReport {
        /// Adapter description reported by the offscreen renderer.
        pub adapter: String,
        /// Target width in pixels.
        pub width: u32,
        /// Target height in pixels.
        pub height: u32,
        /// Number of measured frames.
        pub frames: u32,
        /// CPU time spent meshing every drawn section once, in milliseconds.
        pub mesh_ms: f64,
        /// Median frame time in milliseconds.
        pub p50_ms: f64,
        /// 95th-percentile frame time in milliseconds.
        pub p95_ms: f64,
        /// Longest frame in milliseconds.
        pub max_ms: f64,
        /// Frames per second implied by the mean frame time.
        pub fps: f64,
        /// Number of non-empty section meshes drawn.
        pub sections: u32,
        /// Triangles in those section meshes.
        pub triangles: u64,
        /// Seed of the world that was actually loaded.
        ///
        /// [`ob_server::Host`] loads an existing save instead of regenerating it, so this can
        /// differ from the requested seed when the data directory already holds a bench world.
        pub world_seed: u64,
    }

    /// Error from the benchmark harness.
    #[derive(Debug)]
    pub struct BenchError {
        /// Human-readable failure reason.
        message: String,
    }

    impl BenchError {
        /// Wraps a message.
        fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
            }
        }
    }

    impl std::fmt::Display for BenchError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.message)
        }
    }

    impl std::error::Error for BenchError {}

    impl From<ob_server::HostError> for BenchError {
        fn from(error: ob_server::HostError) -> Self {
            Self::new(error.to_string())
        }
    }

    impl From<ob_render::RenderError> for BenchError {
        fn from(error: ob_render::RenderError) -> Self {
            Self::new(error.to_string())
        }
    }

    /// Parses the `bench` flags on top of [`BenchOptions::default`].
    ///
    /// Returns a message suitable for printing when a flag is missing, malformed or unknown.
    pub fn bench_options_from_flags(flags: &[(String, String)]) -> Result<BenchOptions, String> {
        let mut options = BenchOptions::default();
        for (name, value) in flags {
            match name.as_str() {
                "frames" => {
                    let parsed = parse_u32(name, value)?;
                    if parsed == 0 {
                        return Err("--frames must be at least 1".to_string());
                    }
                    options.frames = parsed;
                }
                "warmup" => options.warmup = parse_u32(name, value)?,
                "width" => {
                    let parsed = parse_u32(name, value)?;
                    if parsed == 0 {
                        return Err("--width must be at least 1".to_string());
                    }
                    options.width = parsed;
                }
                "height" => {
                    let parsed = parse_u32(name, value)?;
                    if parsed == 0 {
                        return Err("--height must be at least 1".to_string());
                    }
                    options.height = parsed;
                }
                "seed" => {
                    let raw = required_value(name, value)?;
                    options.seed = raw.parse().map_err(|_| format!("invalid --seed value '{raw}'"))?;
                }
                "render-distance" => {
                    let raw = required_value(name, value)?;
                    let parsed: i32 = raw
                        .parse()
                        .map_err(|_| format!("invalid --render-distance value '{raw}'"))?;
                    if !(1..=32).contains(&parsed) {
                        return Err("--render-distance must be between 1 and 32 chunks".to_string());
                    }
                    options.render_distance = parsed;
                }
                "data-dir" => options.data_dir = PathBuf::from(required_value(name, value)?),
                "json" => options.json = Some(PathBuf::from(required_value(name, value)?)),
                "max-frame-ms" => {
                    let raw = required_value(name, value)?;
                    let parsed: f64 = raw
                        .parse()
                        .map_err(|_| format!("invalid --max-frame-ms value '{raw}'"))?;
                    if !parsed.is_finite() || parsed < 0.0 {
                        return Err("--max-frame-ms must be a finite non-negative number".to_string());
                    }
                    options.max_frame_ms = Some(parsed);
                }
                "help" | "version" => {}
                other => return Err(format!("unknown option --{other}")),
            }
        }
        Ok(options)
    }

    /// Runs one benchmark and returns its measurements.
    ///
    /// The world is opened through [`ob_server::Host`], streaming is drained, every drawn
    /// section is meshed once and then warm-up and measured frames are rendered offscreen.
    pub fn run_bench(options: &BenchOptions) -> Result<BenchReport, BenchError> {
        if options.frames == 0 {
            return Err(BenchError::new("bench needs at least one measured frame"));
        }
        if options.width == 0 || options.height == 0 {
            return Err(BenchError::new("bench target width and height must be at least 1"));
        }
        if options.render_distance < 1 {
            return Err(BenchError::new("bench render distance must be at least 1 chunk"));
        }

        let registry = Arc::new(Registry::base());
        let config = HostConfig {
            seed: options.seed,
            world_name: WORLD_NAME.to_string(),
            data_dir: Some(options.data_dir.clone()),
            render_distance_chunks: options.render_distance,
            generate_budget: STREAM_BUDGET,
        };
        let mut host = Host::open(registry, config)?;
        let world_seed = host.world().seed();
        if world_seed != options.seed {
            ob_core::warn!(
                "bench loaded the existing world in {} with seed {world_seed}, not the requested {}; \
                 remove that directory to regenerate it",
                options.data_dir.display(),
                options.seed
            );
        }

        // Drain streaming exactly like the client does before its first frame.
        let mut steps = 0usize;
        loop {
            if host.ensure_streaming(STREAM_BUDGET) == 0 {
                break;
            }
            steps += 1;
            if steps >= STREAM_GUARD {
                ob_core::warn!("bench streaming stopped after {STREAM_GUARD} calls; the world may be partial");
                break;
            }
        }

        // Take the dirty set so a warm save cannot leave stale mesh work behind. The offscreen
        // frame draws the loaded sections that fall inside the camera frustum, so the stats pass
        // below meshes exactly that set once to record its geometry and CPU mesh time.
        let dirty = host.take_dirty_sections();
        ob_core::debug!("bench drained {} dirty sections", dirty.len());

        let (eye, yaw, pitch) = {
            let player = host.player();
            (player.eye(), player.yaw, player.pitch)
        };
        let mut camera = Camera::new(eye, yaw, pitch);
        camera.far = options.render_distance as f32 * 16.0 + 64.0;

        let mut renderer = OffscreenRenderer::new(options.width, options.height)?;
        let adapter = renderer.adapter_summary().to_string();

        // Build the GPU mesh cache once; the measured frames then contain only
        // rasterisation plus the GPU-to-CPU readback, not meshing or upload.
        let mesh_start = Instant::now();
        let (sections, vertices) = renderer.rebuild_mesh_cache(host.world());
        let mesh_ms = elapsed_ms(mesh_start);
        let triangles = vertices / 3;

        for _ in 0..options.warmup {
            renderer.render_cached(&camera)?;
        }

        let mut samples = Vec::with_capacity(options.frames as usize);
        for _ in 0..options.frames {
            let start = Instant::now();
            renderer.render_cached(&camera)?;
            samples.push(elapsed_ms(start));
        }

        // Nearest-rank percentiles, per the measurement protocol in
        // docs/spec/25-performance-budgets.md; FPS is 1000 / mean frame time.
        samples.sort_by(f64::total_cmp);
        let mean_ms = samples.iter().sum::<f64>() / samples.len() as f64;
        let fps = if mean_ms > 0.0 { 1000.0 / mean_ms } else { 0.0 };
        Ok(BenchReport {
            adapter,
            width: options.width,
            height: options.height,
            frames: options.frames,
            mesh_ms,
            p50_ms: percentile(&samples, 50.0),
            p95_ms: percentile(&samples, 95.0),
            max_ms: samples.last().copied().unwrap_or(0.0),
            fps,
            sections,
            triangles,
            world_seed,
        })
    }

    /// Formats the compact human-readable benchmark report.
    pub fn format_bench_report(options: &BenchOptions, report: &BenchReport) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "openblock-tools bench: {}x{}, {} frames ({} warm-up), world seed {}, render distance {} chunks\n",
            report.width, report.height, report.frames, options.warmup, report.world_seed, options.render_distance
        ));
        out.push_str(&format!("adapter: {}\n", report.adapter));
        out.push_str(&format!("cached sections: {}\n", report.sections));
        out.push_str(&format!("triangles: {}\n", report.triangles));
        out.push_str(&format!("mesh_ms: {:.3}\n", report.mesh_ms));
        out.push_str(&format!(
            "frame_ms: p50={:.3} p95={:.3} max={:.3}\n",
            report.p50_ms, report.p95_ms, report.max_ms
        ));
        out.push_str(&format!("fps: {:.3}\n", report.fps));
        out
    }

    /// Serialises one report as the version-0 benchmark JSON document.
    ///
    /// Keys are sorted and floating-point values are rounded to three decimals. No timestamp
    /// or machine-specific path is included.
    pub fn bench_json(report: &BenchReport) -> String {
        let mut frame_ms = BTreeMap::new();
        frame_ms.insert("max", json_f64(report.max_ms));
        frame_ms.insert("p50", json_f64(report.p50_ms));
        frame_ms.insert("p95", json_f64(report.p95_ms));

        let mut root = BTreeMap::new();
        root.insert("schema", json_string(SCHEMA));
        root.insert("adapter", json_string(&report.adapter));
        root.insert("width", report.width.to_string());
        root.insert("height", report.height.to_string());
        root.insert("frames", report.frames.to_string());
        root.insert("mesh_ms", json_f64(report.mesh_ms));
        root.insert("frame_ms", json_object(&frame_ms));
        root.insert("fps", json_f64(report.fps));
        root.insert("sections", report.sections.to_string());
        json_object(&root)
    }

    /// Returns the `--max-frame-ms` failure message when p95 exceeds the configured limit.
    pub fn max_frame_violation(options: &BenchOptions, report: &BenchReport) -> Option<String> {
        let limit = options.max_frame_ms?;
        if report.p95_ms > limit {
            Some(format!(
                "bench p95 {:.3} ms exceeds --max-frame-ms {:.3} ms",
                report.p95_ms, limit
            ))
        } else {
            None
        }
    }

    /// Elapsed wall-clock time in milliseconds.
    fn elapsed_ms(start: Instant) -> f64 {
        start.elapsed().as_secs_f64() * 1000.0
    }

    /// Nearest-rank percentile over sorted, non-empty `samples`.
    fn percentile(samples: &[f64], percentile: f64) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let rank = (((percentile / 100.0) * samples.len() as f64).ceil() as usize).max(1);
        samples[rank.min(samples.len()) - 1]
    }

    /// Parses one required `u32` flag value.
    fn parse_u32(name: &str, value: &str) -> Result<u32, String> {
        let raw = required_value(name, value)?;
        raw.parse().map_err(|_| format!("invalid --{name} value '{raw}'"))
    }

    /// Rejects an empty flag value, which the shared parser produces for a missing argument.
    fn required_value<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
        if value.is_empty() {
            Err(format!("missing value for --{name}"))
        } else {
            Ok(value)
        }
    }

    /// Renders a string as a JSON string literal.
    fn json_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for character in value.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                control if (control as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", control as u32));
                }
                other => out.push(other),
            }
        }
        out.push('"');
        out
    }

    /// Renders an `f64` rounded to three decimals, normalising negative zero.
    fn json_f64(value: f64) -> String {
        let text = if value.is_finite() {
            format!("{value:.3}")
        } else {
            "0.000".to_string()
        };
        if text == "-0.000" { "0.000".to_string() } else { text }
    }

    /// Renders a sorted object from pre-rendered JSON values.
    fn json_object(fields: &BTreeMap<&'static str, String>) -> String {
        let mut out = String::from("{");
        for (index, (key, value)) in fields.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(key);
            out.push_str("\":");
            out.push_str(value);
        }
        out.push('}');
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Flags built from literal pairs.
        fn flags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect()
        }

        /// Report used by the formatting and gate tests.
        fn sample_report() -> BenchReport {
            BenchReport {
                adapter: "RADV (Vulkan, test)".to_string(),
                width: 640,
                height: 360,
                frames: 30,
                mesh_ms: 12.3451,
                p50_ms: 1.2341,
                p95_ms: 2.3459,
                max_ms: 3.4561,
                fps: 800.0,
                sections: 123,
                triangles: 4567,
                world_seed: 42,
            }
        }

        #[test]
        fn defaults_match_the_documented_cli() {
            let options = BenchOptions::default();
            assert_eq!(options.frames, 60);
            assert_eq!(options.warmup, 5);
            assert_eq!(options.width, 1280);
            assert_eq!(options.height, 720);
            assert_eq!(options.seed, 42);
            assert_eq!(options.render_distance, 8);
            assert_eq!(options.data_dir, PathBuf::from("target/bench-data"));
            assert_eq!(options.json, None);
            assert_eq!(options.max_frame_ms, None);
        }

        #[test]
        fn flags_override_every_option() {
            let options = bench_options_from_flags(&flags(&[
                ("frames", "30"),
                ("warmup", "0"),
                ("width", "640"),
                ("height", "360"),
                ("seed", "7"),
                ("render-distance", "6"),
                ("data-dir", "/tmp/bench-data"),
                ("json", "/tmp/bench.json"),
                ("max-frame-ms", "2000"),
            ]))
            .expect("the documented flags must parse");
            assert_eq!(options.frames, 30);
            assert_eq!(options.warmup, 0);
            assert_eq!(options.width, 640);
            assert_eq!(options.height, 360);
            assert_eq!(options.seed, 7);
            assert_eq!(options.render_distance, 6);
            assert_eq!(options.data_dir, PathBuf::from("/tmp/bench-data"));
            assert_eq!(options.json, Some(PathBuf::from("/tmp/bench.json")));
            assert_eq!(options.max_frame_ms, Some(2000.0));
        }

        #[test]
        fn malformed_flags_are_rejected() {
            assert_eq!(
                bench_options_from_flags(&flags(&[("nope", "1")])).unwrap_err(),
                "unknown option --nope"
            );
            assert_eq!(
                bench_options_from_flags(&flags(&[("json", "")])).unwrap_err(),
                "missing value for --json"
            );
            assert!(bench_options_from_flags(&flags(&[("frames", "0")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("frames", "many")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("width", "0")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("height", "0")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("render-distance", "0")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("render-distance", "33")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("max-frame-ms", "-1")])).is_err());
            assert!(bench_options_from_flags(&flags(&[("max-frame-ms", "NaN")])).is_err());
        }

        #[test]
        fn run_bench_rejects_an_empty_measurement_before_opening_a_gpu() {
            let options = BenchOptions {
                frames: 0,
                ..BenchOptions::default()
            };
            assert!(run_bench(&options).is_err());
        }

        #[test]
        fn percentiles_use_nearest_rank() {
            let samples: Vec<f64> = (1..=100).map(f64::from).collect();
            assert_eq!(percentile(&samples, 50.0), 50.0);
            assert_eq!(percentile(&samples, 95.0), 95.0);
            assert_eq!(percentile(&samples, 100.0), 100.0);
            assert_eq!(percentile(&[], 50.0), 0.0);
        }

        #[test]
        fn json_report_has_sorted_keys_and_three_decimals() {
            let json = bench_json(&sample_report());
            assert_eq!(
                json,
                concat!(
                    r#"{"adapter":"RADV (Vulkan, test)","fps":800.000,"#,
                    r#""frame_ms":{"max":3.456,"p50":1.234,"p95":2.346},"#,
                    r#""frames":30,"height":360,"mesh_ms":12.345,"#,
                    r#""schema":"openblock.bench/0","sections":123,"width":640}"#
                )
            );
        }

        #[test]
        fn json_strings_are_escaped() {
            assert_eq!(json_string("plain"), "\"plain\"");
            assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
            assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
            assert_eq!(json_string("a\nb"), "\"a\\nb\"");
            assert_eq!(json_string("a\u{1}b"), "\"a\\u0001b\"");
            assert_eq!(json_f64(-0.0), "0.000");
        }

        #[test]
        fn text_report_names_every_contract_field() {
            let text = format_bench_report(&BenchOptions::default(), &sample_report());
            for field in ["adapter:", "sections:", "triangles:", "mesh_ms:", "frame_ms:", "fps:"] {
                assert!(text.contains(field), "missing {field} in:\n{text}");
            }
        }

        #[test]
        fn max_frame_gate_compares_p95() {
            let report = sample_report();
            let mut options = BenchOptions::default();
            assert_eq!(max_frame_violation(&options, &report), None);

            options.max_frame_ms = Some(report.p95_ms + 0.1);
            assert_eq!(max_frame_violation(&options, &report), None);

            options.max_frame_ms = Some(report.p95_ms - 0.1);
            let message = max_frame_violation(&options, &report).expect("p95 above the limit must fail");
            assert!(message.contains("p95"));
            assert!(message.contains("--max-frame-ms"));
        }
    }
}

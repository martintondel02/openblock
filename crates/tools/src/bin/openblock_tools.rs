//! The `openblock-tools` offline tool binary.

use std::path::Path;

const USAGE: &str = "\
usage: openblock-tools <command> [options]
commands:
  version
  inspect-world <file>
  bench [--frames N] [--warmup N] [--width W] [--height H] [--seed S]
        [--render-distance R] [--data-dir DIR] [--json PATH] [--max-frame-ms MS]
  help";

const BENCH_USAGE: &str = "\
usage: openblock-tools bench [--frames N] [--warmup N] [--width W] [--height H] [--seed S]
                             [--render-distance R] [--data-dir DIR] [--json PATH]
                             [--max-frame-ms MS]
defaults: --frames 60 --warmup 5 --width 1280 --height 720 --seed 42
          --render-distance 8 --data-dir target/bench-data; --max-frame-ms unset
exits 1 when the p95 frame time exceeds --max-frame-ms";

fn main() {
    ob_core::init_logging();
    let (flags, free) = ob_core::parse_args(std::env::args().skip(1));
    let has = |name: &str| flags.iter().any(|(n, _)| n == name);
    if has("version") {
        println!("{}", ob_core::version_banner("openblock-tools"));
        return;
    }
    if free.is_empty() && !has("help") {
        println!("{}", ob_core::version_banner("openblock-tools"));
        println!("{USAGE}");
        return;
    }
    match free.first().map(String::as_str) {
        Some("version") => println!("{}", ob_core::version_banner("openblock-tools")),
        Some("inspect-world") => inspect_world(&free),
        Some("bench") => {
            if has("help") {
                println!("{BENCH_USAGE}");
                return;
            }
            run_bench(&flags);
        }
        Some("help") | None => println!("{USAGE}"),
        Some(other) => {
            eprintln!("openblock-tools: unknown command '{other}'");
            std::process::exit(2);
        }
    }
}

/// Implements `inspect-world <file>`.
fn inspect_world(free: &[String]) {
    let Some(path) = free.get(1) else {
        eprintln!("openblock-tools: inspect-world needs a path");
        std::process::exit(2);
    };
    let registry = std::sync::Arc::new(ob_content::Registry::base());
    match std::fs::read(path) {
        Ok(bytes) => match ob_world::from_bytes(registry, &bytes) {
            Ok(world) => {
                println!(
                    "world: {} chunks, {} sections, seed {}, ~{} KiB block data",
                    world.chunk_count(),
                    world.section_count(),
                    world.seed(),
                    world.heap_bytes() / 1024
                );
            }
            Err(err) => {
                eprintln!("openblock-tools: {err}");
                std::process::exit(1);
            }
        },
        Err(err) => {
            eprintln!("openblock-tools: cannot read {path}: {err}");
            std::process::exit(1);
        }
    }
}

/// Implements `bench [options]`.
fn run_bench(flags: &[(String, String)]) {
    let options = match ob_tools::bench::bench_options_from_flags(flags) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("openblock-tools: {message}");
            std::process::exit(2);
        }
    };
    let report = match ob_tools::bench::run_bench(&options) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("openblock-tools: bench failed: {error}");
            std::process::exit(1);
        }
    };
    print!("{}", ob_tools::bench::format_bench_report(&options, &report));

    if let Some(path) = &options.json {
        let document = format!("{}\n", ob_tools::bench::bench_json(&report));
        if let Err(error) = write_json(path, &document) {
            eprintln!("openblock-tools: cannot write {}: {error}", path.display());
            std::process::exit(1);
        }
        println!("json: {}", path.display());
    }

    if let Some(message) = ob_tools::bench::max_frame_violation(&options, &report) {
        eprintln!("openblock-tools: {message}");
        std::process::exit(1);
    }
}

/// Writes the JSON report, creating its parent directory when needed.
fn write_json(path: &Path, document: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, document)
}

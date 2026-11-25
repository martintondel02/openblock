//! Headless 512-block (32-chunk) render-distance benchmark.

fn main() {
    ob_core::init_logging();
    let (flags, _free) = ob_core::parse_args(std::env::args().skip(1));
    match ob_tools::render_distance_bench::run_from_flags(&flags) {
        Ok(()) => {}
        Err(message) => {
            eprintln!("render-distance-bench: {message}");
            std::process::exit(2);
        }
    }
}

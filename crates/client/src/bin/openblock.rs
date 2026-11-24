//! The `openblock` game client binary.

fn main() {
    ob_core::init_logging();
    let (flags, _free) = ob_core::parse_args(std::env::args().skip(1));
    if flags.iter().any(|(name, _)| name == "version") {
        let gpu = match std::env::var("OPENBLOCK_GPU") {
            Ok(value) => value,
            Err(_) => "detected at start".to_string(),
        };
        println!("{}", ob_core::version_banner("openblock"));
        println!("graphics: {}", gpu);
        println!("headless: --screenshot PATH renders one frame to a PNG and exits");
        return;
    }
    let options = match ob_client::options_from_args(&flags) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("openblock: {message}");
            eprintln!(
                "usage: openblock [--world NAME] [--seed N] [--data-dir DIR] [--render-distance N] [--mesh-workers N] [--fly] [--x11] [--screenshot PATH] [--seconds N] [--texture-style NAME] [--version]"
            );
            std::process::exit(2);
        }
    };
    if flags.iter().any(|(name, _)| name == "help") {
        println!(
            "usage: openblock [--world NAME] [--seed N] [--data-dir DIR] [--render-distance N] [--mesh-workers N] [--fly] [--x11] [--screenshot PATH] [--seconds N] [--texture-style NAME] [--version]"
        );
        println!("  --screenshot PATH  render one offscreen frame to a PNG and exit (no window)");
        println!("  --mesh-workers N   section meshing worker threads (1-16, default cores - 1)");
        println!("  --texture-style S  classic (default), painterly, lush or minimal");
        println!("  --render-distance N  streaming radius in chunks (overrides saved settings)");
        return;
    }
    log::info!("{}", ob_core::version_banner("openblock"));
    if let Err(err) = ob_client::run(options) {
        log::error!("{err}");
        std::process::exit(1);
    }
}

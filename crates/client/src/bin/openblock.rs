//! The `openblock` game client binary.

const USAGE: &str = "usage: openblock [--world NAME] [--seed N] [--data-dir DIR] [--render-distance N] [--mesh-workers N] [--fly] [--x11] [--screenshot PATH] [--screenshot-inventory PATH] [--seconds N] [--texture-style NAME] [--texture-pack PATH] [--no-texture-pack] [--time-of-day F] [--version]";

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
        println!("textures: --texture-pack PATH loads a Minecraft resource pack and remembers it");
        println!("          --no-texture-pack clears the saved pack and uses the procedural style");
        println!("headless: --screenshot PATH renders one HUD frame to a PNG and exits");
        println!("          --screenshot-inventory PATH renders the inventory screen to a PNG and exits");
        return;
    }
    let options = match ob_client::options_from_args(&flags) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("openblock: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if flags.iter().any(|(name, _)| name == "help") {
        println!("{USAGE}");
        println!("  --screenshot PATH  render one offscreen HUD frame to a PNG and exit (no window)");
        println!("  --screenshot-inventory PATH  render the inventory screen offscreen to a PNG and exit (no window)");
        println!("  --mesh-workers N   section meshing worker threads (1-16, default cores - 1)");
        println!("  --texture-style S  procedural fallback style: classic (default), painterly, lush or minimal");
        println!("  --texture-pack PATH  load a Minecraft-style resource pack for this run and remember it");
        println!("  --resource-pack PATH  alias for --texture-pack");
        println!("  --no-texture-pack  forget the saved resource pack and use the procedural style");
        println!("  --render-distance N  streaming radius in chunks (overrides saved settings)");
        return;
    }
    log::info!("{}", ob_core::version_banner("openblock"));
    if let Err(err) = ob_client::run(options) {
        log::error!("{err}");
        std::process::exit(1);
    }
}

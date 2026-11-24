//! The `openblock` game client binary.

const USAGE: &str = "usage: openblock [--world NAME] [--seed N] [--data-dir DIR] [--render-distance N] [--mesh-workers N] [--survival|--creative] [--fly] [--x11] [--screenshot PATH] [--screenshot-inventory PATH] [--screenshot-dead] [--screenshot-settings] [--screenshot-crafting] [--screenshot-debug] [--screenshot-items] [--seconds N] [--texture-style NAME] [--texture-pack PATH] [--no-texture-pack] [--time-of-day F] [--version]";

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
        println!("          --screenshot-dead renders the death screen; --screenshot-items seeds items and vitals");
        println!("          --screenshot-settings renders the tabbed settings screen over the world");
        println!("          --screenshot-crafting renders the crafting screen over the world");
        println!("          --screenshot-debug renders the survival HUD with the F3 debug overlay");
        println!("modes:    --survival (default) or --creative; an explicit CLI mode is saved");
        println!("controls: WASD move, Space jump, Shift crouch, Ctrl sprint, F fly, LMB mine,");
        println!("          RMB place/eat/use, C crafting, E inventory, F3 debug, Esc settings");
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
        println!("  --screenshot-dead  render the death screen (used with --screenshot PATH)");
        println!("  --screenshot-settings  render the settings screen (used with --screenshot PATH)");
        println!("  --screenshot-crafting  render the crafting screen (used with --screenshot PATH)");
        println!("  --screenshot-debug  render the HUD and F3 debug overlay (used with --screenshot PATH)");
        println!("  --screenshot-items  seed stones, planks, sticks, tools and 14 HP / 12 hunger (with --screenshot)");
        println!("  --survival         play survival (default; overrides and saves the settings mode)");
        println!("  --creative         play creative (overrides and saves the settings mode)");
        println!("  --mesh-workers N   section meshing worker threads (1-16, default cores - 1)");
        println!("  --texture-style S  procedural fallback style: classic (default), painterly, lush or minimal");
        println!("  --texture-pack PATH  load a Minecraft-style resource pack for this run and remember it");
        println!("  --resource-pack PATH  alias for --texture-pack");
        println!("  --no-texture-pack  forget the saved resource pack and use the procedural style");
        println!("  --render-distance N  streaming radius in chunks (overrides saved settings)");
        println!("  controls           WASD move, Space jump, Shift crouch, Ctrl sprint, F fly,");
        println!("                     LMB mine, RMB place/eat/use, C crafting, E inventory, F3 debug, Esc settings");
        return;
    }
    log::info!("{}", ob_core::version_banner("openblock"));
    if let Err(err) = ob_client::run(options) {
        log::error!("{err}");
        std::process::exit(1);
    }
}

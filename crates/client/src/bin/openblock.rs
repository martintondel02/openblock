//! The `openblock` game client binary.

const USAGE: &str = "usage: openblock [--world NAME] [--seed N] [--data-dir DIR] [--render-distance N] [--mesh-workers N] [--survival|--creative] [--fly] [--x11] [--no-audio] [--mute] [--connect HOST:PORT] [--name NAME] [--screenshot PATH] [--screenshot-inventory PATH] [--screenshot-dead] [--screenshot-settings[=TAB]] [--screenshot-settings-tab TAB] [--screenshot-crafting] [--screenshot-title] [--screenshot-new-world] [--screenshot-worlds] [--screenshot-connect] [--screenshot-debug] [--screenshot-mobs] [--screenshot-furnace] [--screenshot-chest] [--screenshot-items] [--screenshot-network] [--screenshot-name-tags] [--screenshot-net-ui=KIND] [--screenshot-size WxH] [--ui-scale PERCENT|auto] [--seconds N] [--texture-style NAME] [--texture-pack PATH] [--no-texture-pack] [--ui-font PATH] [--default-ui-font] [--time-of-day F] [--title|--menu] [--version]";

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
        println!("fonts:    --ui-font PATH loads a TTF/OTF UI font (e.g. Minecraftia) and remembers it");
        println!("          --default-ui-font clears the saved UI font; Monocraft is bundled");
        println!("          <data-dir>/fonts/Minecraftia.ttf is discovered automatically");
        println!("headless: --screenshot PATH renders one HUD frame to a PNG and exits");
        println!("          --screenshot-inventory PATH renders the inventory screen to a PNG and exits");
        println!(
            "          --screenshot-dead[=network] renders the death screen; --screenshot-items seeds items and vitals"
        );
        println!("          --screenshot-settings renders the tabbed settings screen over the world");
        println!("          --screenshot-settings=audio selects the Audio tab (game/display/audio/controls/textures)");
        println!("          --screenshot-crafting[=hand] renders the crafting screen (hand gating with hand)");
        println!("          --screenshot-debug renders the survival HUD with the F3 debug overlay");
        println!("          --screenshot-mobs[=plain] spawns avatars; plain omits the F3 overlay");
        println!("          --screenshot-furnace[=empty|active|full] opens a seeded furnace screen");
        println!("          --screenshot-chest[=empty|active|full] opens a seeded 27-slot chest screen");
        println!("          --screenshot-network renders a network survival frame from an in-process loopback server");
        println!("          --screenshot-name-tags renders synthetic remote-player name tags over a world");
        println!("          --screenshot-net-ui=loading|failed|disconnected|roster  multiplayer overlay audit frame");
        println!("          --screenshot-size WxH  offscreen target size (default 1280x720; e.g. 960x540)");
        println!("          --ui-scale auto|75|100|125|150  override GUI scale for this run (audits)");
        println!("front-end: a plain launch opens the title/world-selection screens;");
        println!("          --world/--seed/--data-dir/--connect and screenshot flags bypass it");
        println!("          --title or --menu forces the title screen (useful for screenshots)");
        println!("modes:    --survival (default) or --creative; an explicit CLI mode is saved");
        println!("network:  --connect HOST:PORT joins a dedicated survival server (windowed; TCP)");
        println!("          --name NAME chooses the multiplayer player name (default playerNNNN)");
        println!("          --server-password is not supported by the interim protocol yet");
        println!("controls: WASD move, Space jump, Shift crouch, Ctrl sprint, F fly, LMB mine,");
        println!("          RMB place/eat/use, C crafting, E inventory, F3 debug, Esc settings");
        println!("          multiplayer: T chat, Tab players, LMB break, RMB place");
        println!("audio:    --no-audio disables output; --mute starts muted; Esc > Audio adjusts volumes");
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
        println!("  --screenshot-settings[=TAB]  render the settings screen, optionally selecting game,");
        println!("                     display, audio, controls or textures (used with --screenshot PATH)");
        println!("  --screenshot-settings-tab TAB  alias for --screenshot-settings=TAB");
        println!("  --screenshot-crafting  render the crafting screen (used with --screenshot PATH)");
        println!("  --screenshot-debug  render the HUD and F3 debug overlay (used with --screenshot PATH)");
        println!("  --screenshot-mobs  spawn a few mobs and show the HUD plus F3 mob/day counts");
        println!("  --screenshot-furnace  place and open a seeded furnace screen");
        println!("  --screenshot-chest  place and open a seeded chest screen");
        println!("  --screenshot-items  seed ores, ingots, iron tools/armor, food, planks and 14 HP / 12 hunger");
        println!("  --screenshot-network  render network survival from an in-process loopback server");
        println!("  --screenshot-name-tags  render synthetic remote-player name tags over a world");
        println!("  --screenshot-net-ui=KIND  render loading, failed, disconnected or roster overlay");
        println!("  --screenshot-size WxH  offscreen target size (default 1280x720)");
        println!("  --ui-scale PERCENT|auto  override the persistent GUI scale (auto, 75, 100, 125 or 150)");
        println!("  --screenshot-title  render the title screen (requires --screenshot PATH)");
        println!("  --screenshot-new-world  render the new-world form (requires --screenshot PATH)");
        println!("  --screenshot-worlds  render the world list with temporary worlds");
        println!("  --screenshot-connect  render the multiplayer form with recent servers");
        println!("  --screenshot-settings-title  render host-optional settings as opened from the title");
        println!("  --title, --menu    force the front-end on a normal launch (screenshot/debug aid)");
        println!("  --connect HOST:PORT  join a dedicated server over TCP (windowed; no local world)");
        println!("  --name NAME        multiplayer player name (1-32 ASCII [A-Za-z0-9_.-]; default playerNNNN)");
        println!("                     multiplayer is server-authoritative survival over plain TCP (no TLS)");
        println!("  --survival         play survival (default; overrides and saves the settings mode)");
        println!("  --creative         play creative (overrides and saves the settings mode)");
        println!("  --mesh-workers N   section meshing worker threads (1-16, default cores - 1)");
        println!("  --texture-style S  procedural fallback style: classic (default), painterly, lush or minimal");
        println!("  --texture-pack PATH  load a Minecraft-style resource pack for this run and remember it");
        println!("  --resource-pack PATH  alias for --texture-pack");
        println!("  --no-texture-pack  forget the saved resource pack and use the procedural style");
        println!("  --ui-font PATH     load this TTF/OTF UI font for this and future runs");
        println!("  --default-ui-font  forget the saved UI font and use the bundled Monocraft");
        println!("                     (place Minecraftia.ttf in <data-dir>/fonts/ to use it automatically)");
        println!("  --render-distance N  streaming radius in chunks (overrides saved settings)");
        println!("  --no-audio         do not open an output device (headless/streaming)");
        println!("  --mute             start muted for this run; saved audio settings are unchanged");
        println!("  controls           WASD move, Space jump, Shift crouch, Ctrl sprint, F fly,");
        println!("                     LMB mine, RMB place/eat/use, C crafting, E inventory, F3 debug, Esc settings");
        println!("                     multiplayer: T chat, Tab player list, LMB break, RMB place");
        println!("  audio              Esc > Audio: Sound, Master, SFX and Music; changes apply immediately");
        return;
    }
    log::info!("{}", ob_core::version_banner("openblock"));
    if let Err(err) = ob_client::run(options) {
        log::error!("{err}");
        std::process::exit(1);
    }
}

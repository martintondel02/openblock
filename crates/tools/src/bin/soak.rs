//! Headless soak runner for the 1.0 survival host.
//!
//! The runner opens a real [`ob_server::Host`] on a temporary world and drives
//! it for a configurable number of 20 Hz ticks with a deterministic script:
//! circular flight/walking, periodic surface mining and placing, direct and
//! natural mobs, combat, eating, world streaming and periodic saves. Every tick
//! is checked for crashes-by-invariant (finite positions, caps, inventory
//! validity); progress and the metrics series are printed so a leak or a slow
//! drift is visible before the end.
//!
//! At the end the host is saved, dropped and reloaded; the state hash, tick
//! count, inventory and entity counts must match exactly.
//!
//! ```text
//! soak --quick                 # CI: 4,000 ticks (3.3 game minutes)
//! soak                         # 60,000 ticks (50 game minutes)
//! soak --ticks 200000 --keep   # longer local soak, keep the world
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ob_content::{BlockId, MobKind, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{
    CHEST_SLOTS, FurnaceSlot, GameMode, Host, HostConfig, ItemStack, MAX_HOSTILE_MOBS, MAX_PASSIVE_MOBS, MAX_STACK_SIZE,
};
use ob_sim::{BlockAction, MAX_HEALTH, MAX_HUNGER, PlayerInput};
use ob_tools::measure::{self, Stats};

/// World directory name used under the soak data root.
const WORLD_NAME: &str = "soak";
/// Ticks per second in the fixed simulation.
const TICKS_PER_SECOND: f64 = 20.0;
/// Chunks generated per streaming call.
const STREAM_BUDGET: usize = 4;
/// Chunks generated per setup/final streaming batch.
const STREAM_BATCH: usize = 4096;
/// Hard sanity cap on loaded columns (a leak detector, not a game limit).
const MAX_LOADED_COLUMNS: usize = 4096;
/// Hard sanity cap on container records (a leak detector).
const MAX_CONTAINERS: usize = 512;
/// Items below this height have despawned by the host; an item below it is a bug.
const ITEM_VOID_Y: f64 = -80.0;

const USAGE: &str = "\
usage: soak [options]
  --quick              CI profile: 4,000 ticks, radius 4, 1,000-tick progress
  --ticks N            number of host ticks (default 60,000; quick 4,000)
  --radius N           streaming radius in chunks (default 6; quick 4)
  --seed S             world seed (default 42)
  --save-every N       ticks between saves (default 10,000; quick 2,000)
  --progress-every N   ticks between progress lines (default 5,000; quick 1,000)
  --max-items N        invariant cap on live item entities (default 2,048)
  --data-dir DIR       soak data root instead of a temporary directory
  --clean              remove an existing <DIR>/worlds/soak before opening
  --keep               keep the temporary data directory for inspection
  --csv PATH           write the final summary CSV
  --markdown PATH      write the markdown summary
  --help";

/// Parsed command-line options.
#[derive(Debug, Clone)]
struct Options {
    ticks: u64,
    radius: i32,
    seed: u64,
    save_every: u64,
    progress_every: u64,
    max_items: usize,
    data_dir: Option<PathBuf>,
    clean: bool,
    keep: bool,
    csv: Option<PathBuf>,
    markdown: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ticks: 60_000,
            radius: 6,
            seed: 42,
            save_every: 10_000,
            progress_every: 5_000,
            max_items: 2048,
            data_dir: None,
            clean: false,
            keep: false,
            csv: None,
            markdown: None,
        }
    }
}

/// Scripted-session counters.
#[derive(Debug, Default, Clone)]
struct Counters {
    breaks_attempted: u64,
    breaks_changed: u64,
    break_drops: u64,
    places_attempted: u64,
    places_changed: u64,
    attacks: u64,
    hits: u64,
    kills: u64,
    mobs_spawned: u64,
    deaths: u64,
    respawns: u64,
    eats: u64,
    pickup_events: u64,
    chunks_generated: u64,
    chunks_unloaded: u64,
    dirty_sections: u64,
    saves: u64,
}

/// Measured save series and sizes.
#[derive(Debug, Default)]
struct SaveSeries {
    times_ms: Vec<f64>,
    sizes: Vec<u64>,
    first_size: u64,
    last_size: u64,
}

/// Progress sample.
#[derive(Debug, Clone)]
struct Progress {
    tick: u64,
    elapsed_s: f64,
    mobs: usize,
    items: usize,
    rss_kib: Option<u64>,
}

/// Persistent mining target so placement restores the block that was broken.
#[derive(Debug, Clone, Copy)]
struct MiningTarget {
    x: i32,
    z: i32,
    height: i32,
}

/// Runs the soak and returns a process exit code.
fn main() {
    ob_core::init_logging();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("soak: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match run(&options) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(message) => {
            eprintln!("soak: {message}");
            std::process::exit(1);
        }
    }
}

/// Parses command-line flags into [`Options`].
fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let (flags, free) = ob_core::parse_args(args);
    if !free.is_empty() {
        return Err(format!("unexpected argument '{}'", free[0]));
    }
    let mut options = Options::default();
    let mut quick = false;
    let mut explicit = [false; 6];
    for (name, value) in flags {
        match name.as_str() {
            "quick" => quick = true,
            "ticks" => {
                options.ticks = parse_u64(&name, &value)?;
                if options.ticks == 0 {
                    return Err("--ticks must be at least 1".to_string());
                }
                explicit[0] = true;
            }
            "radius" => {
                let radius: i32 = value.parse().map_err(|_| format!("invalid --radius value '{value}'"))?;
                if !(1..=32).contains(&radius) {
                    return Err("--radius must be in 1..=32 chunks".to_string());
                }
                options.radius = radius;
                explicit[1] = true;
            }
            "seed" => options.seed = parse_seed(&value)?,
            "save-every" => {
                options.save_every = parse_u64(&name, &value)?;
                if options.save_every == 0 {
                    return Err("--save-every must be at least 1 tick".to_string());
                }
                explicit[2] = true;
            }
            "progress-every" => {
                options.progress_every = parse_u64(&name, &value)?;
                if options.progress_every == 0 {
                    return Err("--progress-every must be at least 1 tick".to_string());
                }
                explicit[3] = true;
            }
            "max-items" => {
                options.max_items = parse_usize(&name, &value, 1, 100_000)?;
                explicit[4] = true;
            }
            "data-dir" => {
                if value.is_empty() {
                    return Err("--data-dir requires a path".to_string());
                }
                options.data_dir = Some(PathBuf::from(value));
            }
            "clean" => options.clean = true,
            "keep" => options.keep = true,
            "csv" => options.csv = Some(PathBuf::from(required(&name, &value)?)),
            "markdown" => options.markdown = Some(PathBuf::from(required(&name, &value)?)),
            "help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option --{other}")),
        }
    }
    if quick {
        if !explicit[0] {
            options.ticks = 4_000;
        }
        if !explicit[1] {
            options.radius = 4;
        }
        if !explicit[2] {
            options.save_every = 2_000;
        }
        if !explicit[3] {
            options.progress_every = 1_000;
        }
        if !explicit[4] {
            options.max_items = 2048;
        }
    }
    Ok(options)
}

/// Reads a required non-empty value.
fn required(name: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("--{name} requires a value"))
    } else {
        Ok(value.to_string())
    }
}

/// Parses a `u64` flag.
fn parse_u64(name: &str, value: &str) -> Result<u64, String> {
    value.parse().map_err(|_| format!("invalid --{name} value '{value}'"))
}

/// Parses a bounded `usize` flag.
fn parse_usize(name: &str, value: &str, min: usize, max: usize) -> Result<usize, String> {
    let parsed: usize = value.parse().map_err(|_| format!("invalid --{name} value '{value}'"))?;
    if !(min..=max).contains(&parsed) {
        return Err(format!("--{name} must be in {min}..={max}"));
    }
    Ok(parsed)
}

/// Parses one decimal or hexadecimal seed.
fn parse_seed(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let parsed = if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        trimmed.parse()
    };
    parsed.map_err(|_| format!("invalid seed '{value}'"))
}

/// Executes the soak; returns whether all invariants and the reload hash passed.
fn run(options: &Options) -> Result<bool, String> {
    let registry = Arc::new(Registry::base());
    let temp_guard = prepare_root(options)?;
    let root = match &options.data_dir {
        Some(path) => path.clone(),
        None => temp_guard
            .as_ref()
            .map(|guard| guard.path().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("target/soak-data")),
    };
    let config = HostConfig {
        seed: options.seed,
        world_name: WORLD_NAME.to_string(),
        data_dir: Some(root.clone()),
        render_distance_chunks: options.radius,
        generate_budget: 64,
    };
    let world_path = root.join("worlds").join(WORLD_NAME);

    println!("soak - headless scripted survival session");
    println!("cpu:    {}", measure::cpu_model());
    println!("os:     {}", measure::os_description());
    println!(
        "config: seed {}, {} ticks ({:.1} game minutes), radius {} chunks, save every {}, progress every {}",
        options.seed,
        options.ticks,
        options.ticks as f64 / TICKS_PER_SECOND / 60.0,
        options.radius,
        options.save_every,
        options.progress_every
    );
    println!("profile: {}", if cfg!(debug_assertions) { "debug" } else { "release" });
    println!("data:   {}", root.display());
    println!();

    let rss_start = measure::rss_kib();
    let open_start = Instant::now();
    let mut host = Host::open(Arc::clone(&registry), config.clone()).map_err(|error| format!("open host: {error}"))?;
    let open_ms = open_start.elapsed().as_secs_f64() * 1000.0;
    host.set_game_mode(GameMode::Survival);
    let center = host.spawn();
    prepare_player(&mut host);
    prepare_inventory(&mut host);
    let stream = stream_radius(&mut host, center, options.radius)?;
    let containers = prepare_containers(&mut host, 4, 4);
    let initial_mobs = seed_mobs(&mut host, 3, 3);
    println!(
        "setup: open {:.1} ms, streamed {} columns in {:.1} ms, {} furnaces, {} chests, {} initial mobs",
        open_ms, stream.0, stream.1, containers.0, containers.1, initial_mobs
    );

    let mut counters = Counters::default();
    let mut saves = SaveSeries::default();
    let mut progress = Vec::new();
    let mut rss_peak = rss_start;
    let mut peak_items = host.item_entities().len();
    let mut peak_mobs = host.mob_count();
    let mut mining_target: Option<MiningTarget> = None;
    let mut last_tick = host.tick_count();
    let session_start = Instant::now();
    let save = |host: &mut Host, counters: &mut Counters, saves: &mut SaveSeries| -> Result<(), String> {
        let started = Instant::now();
        host.save().map_err(|error| format!("save failed: {error}"))?;
        saves.times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        let size = directory_size(&world_path)?;
        if saves.sizes.is_empty() {
            saves.first_size = size;
        }
        saves.last_size = size;
        saves.sizes.push(size);
        counters.saves += 1;
        Ok(())
    };

    for step in 0..options.ticks {
        let tick = host.tick_count();
        if tick < last_tick {
            return Err(format!("tick count regressed: {last_tick} -> {tick}"));
        }
        last_tick = tick;

        // Deterministic movement: a slow circle with periodic pauses, jumps,
        // sprint and sneak. Flying keeps the script robust over cave mouths.
        host.player_mut().yaw = (tick as f32) * 0.012;
        host.player_mut().pitch = 0.0;
        host.player_mut().flying = true;
        let moving = (tick % 1_200) < 950;
        let quadrant = (tick / 160) % 4;
        let input = PlayerInput {
            forward: if moving { 1.0 } else { 0.0 },
            strafe: if moving {
                match quadrant {
                    0 => 0.0,
                    1 => 0.7,
                    2 => 0.0,
                    _ => -0.7,
                }
            } else {
                0.0
            },
            jump: tick % 40 == 0,
            sprint: (tick / 300) % 2 == 0,
            sneak: (tick / 700) % 5 == 0,
            fly_toggle: false,
        };
        host.tick(&input);

        // Streaming is a separate host call, like the client's frame pump.
        if tick % 10 == 0 {
            counters.chunks_generated += host.ensure_streaming(STREAM_BUDGET) as u64;
            let unloaded = host.take_unloaded_chunks();
            counters.chunks_unloaded += unloaded.len() as u64;
            counters.dirty_sections += host.take_dirty_sections().len() as u64;
        }
        if tick % 100 == 0 {
            counters.pickup_events += host.take_pickup_events().len() as u64;
        }

        // Break a surface block every 50 ticks and place it back on the next
        // tick. This exercises survival drops, inventory/placement rules and
        // the item-entity lifecycle continuously.
        if tick % 50 == 0
            && let Some(target) = pick_mining_target(&host, host.player().pos, tick)
        {
            position_at_surface(&mut host, target);
            let action = BlockAction {
                break_block: true,
                place_block: false,
                place: BlockId::AIR,
            };
            let outcome = host.apply_action_with_outcome(&action);
            counters.breaks_attempted += 1;
            if outcome.changed {
                counters.breaks_changed += 1;
                counters.break_drops += outcome.drops.len() as u64;
            }
            mining_target = Some(target);
        }
        if tick % 50 == 1
            && let Some(target) = mining_target
        {
            position_at_surface(&mut host, target);
            if ensure_selected_cobble(&mut host) {
                let action = BlockAction {
                    break_block: false,
                    place_block: true,
                    place: host.registry().id("openblock:cobblestone").unwrap_or(BlockId::AIR),
                };
                counters.places_attempted += 1;
                if host.apply_action(&action) {
                    counters.places_changed += 1;
                }
            }
        }

        // Combat: every 300 ticks, step next to a mob and swing; every 400
        // ticks top up the population through the public spawn API.
        if tick % 300 == 0
            && let Some((id, pos)) = nearest_mob(&host, host.player().pos)
            && host.player().pos.distance(pos) <= 24.0
        {
            host.player_mut().pos = WorldVec::new(pos.x + 1.5, pos.y, pos.z);
            host.player_mut().flying = true;
            host.player_mut().health = MAX_HEALTH;
            host.player_mut().dead = false;
            let outcome = host.attack_mob(id);
            counters.attacks += 1;
            if outcome.hit {
                counters.hits += 1;
            }
            if outcome.killed {
                counters.kills += 1;
            }
        }
        if tick % 400 == 0 {
            counters.mobs_spawned += top_up_mobs(&mut host, 2) as u64;
        }

        // Food and death handling.
        if tick % 500 == 0 && host.player().hunger < 16.5 && eat_apple(&mut host) {
            counters.eats += 1;
        }
        if host.is_dead() {
            counters.deaths += 1;
            host.respawn();
            counters.respawns += 1;
            host.player_mut().flying = true;
            reset_vitals(&mut host);
        }

        // Invariants are cheap at these population sizes; check every tick.
        if let Err(error) = check_invariants(&host, options.max_items) {
            return Err(format!("invariant failed at tick {tick}: {error}"));
        }
        peak_items = peak_items.max(host.item_entities().len());
        peak_mobs = peak_mobs.max(host.mob_count());

        if tick % options.save_every == 0 && tick > 0 {
            save(&mut host, &mut counters, &mut saves)?;
        }
        if tick % options.progress_every == 0 {
            let rss = measure::rss_kib();
            rss_peak = max_option(rss_peak, rss);
            progress.push(Progress {
                tick,
                elapsed_s: session_start.elapsed().as_secs_f64(),
                mobs: host.mob_count(),
                items: host.item_entities().len(),
                rss_kib: rss,
            });
            print_progress(step, options.ticks, &host, &saves, rss);
        }
    }

    let loop_wall_s = session_start.elapsed().as_secs_f64();
    // Final save with the loaded set re-centred on the final player position,
    // so the reloaded host keeps exactly the same columns.
    loop {
        let generated = host.ensure_streaming(4096);
        let _ = host.take_unloaded_chunks();
        if generated == 0 {
            break;
        }
    }
    save(&mut host, &mut counters, &mut saves)?;
    let final_save_ms = saves.times_ms.last().copied().unwrap_or(0.0);
    let hash_before = host.state_hash();
    let chunks_before = host.world().chunk_count();
    let tick_before = host.tick_count();
    let mobs_before = host.mob_count();
    let items_before = host.item_entities().len();
    let furnaces_before = host.furnaces().count();
    let chests_before = host.chests().count();
    let inventory_before = host.inventory().to_vec();
    let rss_end = measure::rss_kib();
    rss_peak = max_option(rss_peak, rss_end);
    drop(host);

    let load_start = Instant::now();
    let reloaded = Host::open(Arc::clone(&registry), config.clone());
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    let (
        load_error,
        hash_after,
        chunks_after,
        tick_after,
        mobs_after,
        items_after,
        furnaces_after,
        chests_after,
        inventory_after,
    ) = match &reloaded {
        Ok(host) => (
            None,
            Some(host.state_hash()),
            host.world().chunk_count(),
            host.tick_count(),
            host.mob_count(),
            host.item_entities().len(),
            host.furnaces().count(),
            host.chests().count(),
            host.inventory().to_vec(),
        ),
        Err(error) => (Some(error.to_string()), None, 0, 0, 0, 0, 0, 0, Vec::new()),
    };
    drop(reloaded);

    let save_stats = Stats::from_samples(&saves.times_ms);
    let ticks_per_second = if loop_wall_s > 0.0 {
        options.ticks as f64 / loop_wall_s
    } else {
        0.0
    };
    let hash_match = hash_after == Some(hash_before);
    let counts_match = chunks_after == chunks_before
        && tick_after == tick_before
        && mobs_after == mobs_before
        && items_after == items_before
        && furnaces_after == furnaces_before
        && chests_after == chests_before;
    let inventory_match = inventory_before == inventory_after;
    let verdict = load_error.is_none() && hash_match && counts_match && inventory_match;

    println!();
    println!("## Soak summary");
    println!();
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!("| ticks | {}", options.ticks);
    println!(
        "| wall time | {:.1} s ({:.1} ticks/s, {:.1}% of 20 Hz) |",
        loop_wall_s,
        ticks_per_second,
        ticks_per_second / TICKS_PER_SECOND * 100.0
    );
    println!(
        "| streaming | {} generated, {} unloaded |",
        counters.chunks_generated, counters.chunks_unloaded
    );
    println!(
        "| mining | {} / {} breaks changed, {} drops |",
        counters.breaks_changed, counters.breaks_attempted, counters.break_drops
    );
    println!(
        "| building | {} / {} places changed |",
        counters.places_changed, counters.places_attempted
    );
    println!(
        "| combat | {} attacks, {} hits, {} kills |",
        counters.attacks, counters.hits, counters.kills
    );
    println!(
        "| mobs | {} spawned, peak {} (cap {}) |",
        counters.mobs_spawned,
        peak_mobs,
        MAX_PASSIVE_MOBS + MAX_HOSTILE_MOBS
    );
    println!(
        "| items | peak {} (cap {}), {} pickup events |",
        peak_items, options.max_items, counters.pickup_events
    );
    println!(
        "| survival | {} deaths, {} respawns, {} meals |",
        counters.deaths, counters.respawns, counters.eats
    );
    println!("| dirty sections drained | {} |", counters.dirty_sections);
    println!(
        "| saves | {} (mean {:.2} ms, p95 {:.2} ms, last {:.2} ms) |",
        counters.saves, save_stats.mean_ms, save_stats.p95_ms, final_save_ms
    );
    println!(
        "| save size | {} -> {} (peak {}) |",
        measure::bytes_label(saves.first_size),
        measure::bytes_label(saves.last_size),
        measure::bytes_label(saves.sizes.iter().copied().max().unwrap_or(0))
    );
    println!(
        "| RSS | start {} -> end {} (peak {}) |",
        rss_start.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_end.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_peak.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "| save/reload hash | `{:016x}` -> `{}` |",
        hash_before,
        hash_after
            .map(|hash| format!("{hash:016x}"))
            .unwrap_or_else(|| "reload failed".to_string())
    );
    println!(
        "| reload | {} columns, {} ticks, {:.1} ms |",
        chunks_after, tick_after, load_ms
    );
    println!("| invariant verdict | {} |", if verdict { "PASS" } else { "FAIL" });
    if let Some(error) = &load_error {
        println!("| reload error | {error} |");
    }
    println!();
    println!("## Progress series");
    println!();
    println!("| Tick | Elapsed s | Mobs | Items | RSS |");
    println!("| ---: | ---: | ---: | ---: | ---: |");
    for sample in &progress {
        println!(
            "| {} | {:.1} | {} | {} | {} |",
            sample.tick,
            sample.elapsed_s,
            sample.mobs,
            sample.items,
            sample.rss_kib.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
        );
    }
    println!(
        "| {} | {:.1} | {} | {} | {} |",
        options.ticks,
        loop_wall_s,
        mobs_before,
        items_before,
        rss_end.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );

    let report = build_report(
        options,
        &counters,
        &saves,
        &save_stats,
        &progress,
        loop_wall_s,
        ticks_per_second,
        rss_start,
        rss_end,
        rss_peak,
        peak_items,
        peak_mobs,
        hash_before,
        hash_after,
        load_ms,
        chunks_after,
        tick_after,
        verdict,
    );
    if let Some(path) = &options.csv {
        measure::write_text(path, &report.csv).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("csv: {}", path.display());
    }
    if let Some(path) = &options.markdown {
        measure::write_text(path, &report.markdown)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("markdown: {}", path.display());
    }

    if !verdict {
        eprintln!("soak: save/reload round-trip failed");
    }
    if options.keep
        && let Some(guard) = temp_guard
    {
        let path = guard.keep();
        println!("kept data directory: {}", path.display());
    }
    Ok(verdict)
}

/// Creates/validates the soak data root.
fn prepare_root(options: &Options) -> Result<Option<measure::TempDir>, String> {
    match &options.data_dir {
        Some(root) => {
            std::fs::create_dir_all(root)
                .map_err(|error| format!("cannot create data directory {}: {error}", root.display()))?;
            let dir = root.join("worlds").join(WORLD_NAME);
            if dir.exists() {
                if options.clean {
                    std::fs::remove_dir_all(&dir)
                        .map_err(|error| format!("cannot clean {}: {error}", dir.display()))?;
                } else {
                    return Err(format!(
                        "{} already exists; pass --clean or use a fresh --data-dir",
                        dir.display()
                    ));
                }
            }
            Ok(None)
        }
        None => Ok(Some(
            measure::temp_dir("openblock-soak").map_err(|error| error.to_string())?,
        )),
    }
}

/// Puts the player into a stable survival state.
fn prepare_player(host: &mut Host) {
    let player = host.player_mut();
    player.flying = true;
    player.health = MAX_HEALTH;
    player.hunger = MAX_HUNGER;
    player.dead = false;
    player.velocity = WorldVec::ZERO;
}

/// Resets vitals after a respawn.
fn reset_vitals(host: &mut Host) {
    let player = host.player_mut();
    player.health = MAX_HEALTH;
    player.hunger = MAX_HUNGER;
    player.dead = false;
    player.velocity = WorldVec::ZERO;
}

/// Fills the hotbar and armor slots with the items the script consumes.
fn prepare_inventory(host: &mut Host) {
    for (name, count) in [
        ("openblock:iron_pickaxe", 1),
        ("openblock:apple", 32),
        ("openblock:cobblestone", 64),
        ("openblock:leather_helmet", 1),
        ("openblock:leather_chestplate", 1),
        ("openblock:leather_leggings", 1),
        ("openblock:leather_boots", 1),
    ] {
        if let Some(item) = host.registry().item_id(name) {
            let _ = host.give_item(item, count);
        }
    }
    for name in [
        "openblock:leather_helmet",
        "openblock:leather_chestplate",
        "openblock:leather_leggings",
        "openblock:leather_boots",
    ] {
        let Some(item) = host.registry().item_id(name) else {
            continue;
        };
        if let Some(slot) = host
            .inventory()
            .iter()
            .position(|stack| stack.is_some_and(|stack| stack.item == item))
        {
            let _ = host.equip_armor(slot);
        }
    }
}

/// Streams a radius and returns `(columns, elapsed_ms)`.
fn stream_radius(host: &mut Host, center: WorldVec, radius: i32) -> Result<(usize, f64), String> {
    let started = Instant::now();
    let mut chunks = 0usize;
    loop {
        let generated = host.ensure_streaming_around(center, radius, STREAM_BATCH);
        chunks += generated;
        if generated == 0 {
            break;
        }
    }
    Ok((chunks, started.elapsed().as_secs_f64() * 1000.0))
}

/// Places and fills a few containers, returning `(furnaces, chests)`.
fn prepare_containers(host: &mut Host, furnaces: usize, chests: usize) -> (usize, usize) {
    let center = host.spawn();
    let furnace_id = host.registry().id("openblock:furnace").unwrap_or(BlockId::AIR);
    let chest_id = host.registry().id("openblock:chest").unwrap_or(BlockId::AIR);
    let raw_iron = host.registry().item_id("openblock:raw_iron");
    let coal = host.registry().item_id("openblock:coal");
    let cobble = host.registry().item_id("openblock:cobblestone");
    let mut placed_furnaces = 0usize;
    let mut placed_chests = 0usize;
    let mut offset = 0i32;
    for index in 0..(furnaces + chests) {
        let (x, z) = grid_offset(center, &mut offset);
        let Some(pos) = surface_air_cell(host, x, z) else {
            continue;
        };
        if index < furnaces {
            if furnace_id == BlockId::AIR || !host.apply_remote_edit(pos, furnace_id) {
                continue;
            }
            placed_furnaces += 1;
            if let (Some(iron), Some(fuel)) = (raw_iron, coal) {
                let _ = host.put_furnace_slot(pos, FurnaceSlot::Input, ItemStack::new(iron, 32));
                let _ = host.put_furnace_slot(pos, FurnaceSlot::Fuel, ItemStack::new(fuel, 32));
            }
        } else {
            if chest_id == BlockId::AIR || !host.apply_remote_edit(pos, chest_id) {
                continue;
            }
            placed_chests += 1;
            for slot in 0..CHEST_SLOTS {
                if let Some(item) = cobble {
                    let _ = host.put_chest_slot(pos, slot, ItemStack::new(item, 64));
                }
            }
        }
    }
    (placed_furnaces, placed_chests)
}

/// Spawns initial passive and hostile mobs around the player.
fn seed_mobs(host: &mut Host, passive: usize, hostile: usize) -> usize {
    let center = host.spawn();
    let mut spawned = 0usize;
    for index in 0..passive {
        let kind = [MobKind::Cow, MobKind::Pig, MobKind::Sheep, MobKind::Chicken][index % 4];
        if let Some(pos) = find_spawn_position(host, center, 10.0, 30.0, index, kind as u32)
            && host.spawn_mob(kind, pos).is_some()
        {
            spawned += 1;
        }
    }
    for index in 0..hostile {
        if let Some(pos) = find_spawn_position(host, center, 12.0, 30.0, index + 64, 0x2A11)
            && host.spawn_mob(MobKind::Zombie, pos).is_some()
        {
            spawned += 1;
        }
    }
    spawned
}

/// Tops the population up to the natural caps through the direct spawn API.
fn top_up_mobs(host: &mut Host, attempts_per_category: usize) -> usize {
    let center = host.player().pos;
    let mut spawned = 0usize;
    let passive_kinds = [MobKind::Cow, MobKind::Pig, MobKind::Sheep, MobKind::Chicken];
    if host.passive_mob_count() < MAX_PASSIVE_MOBS {
        for attempt in 0..attempts_per_category {
            if host.mob_count() >= MAX_PASSIVE_MOBS + MAX_HOSTILE_MOBS {
                break;
            }
            let kind = passive_kinds[(host.mob_count() + attempt) % passive_kinds.len()];
            if let Some(pos) = find_spawn_position(host, center, 12.0, 36.0, attempt + 128, kind as u32)
                && host.spawn_mob(kind, pos).is_some()
            {
                spawned += 1;
            }
        }
    }
    if host.hostile_mob_count() < MAX_HOSTILE_MOBS && host.mob_count() < MAX_PASSIVE_MOBS + MAX_HOSTILE_MOBS {
        for attempt in 0..attempts_per_category {
            if host.mob_count() >= MAX_PASSIVE_MOBS + MAX_HOSTILE_MOBS {
                break;
            }
            if let Some(pos) = find_spawn_position(host, center, 14.0, 40.0, attempt + 256, 0x2A11)
                && host.spawn_mob(MobKind::Zombie, pos).is_some()
            {
                spawned += 1;
            }
        }
    }
    spawned
}

/// Selects the nearest live mob to a point.
fn nearest_mob(host: &Host, point: WorldVec) -> Option<(u64, WorldVec)> {
    host.mobs()
        .iter()
        .filter(|entity| entity.mob.pos.is_finite() && entity.mob.health > 0.0)
        .map(|entity| (entity.id, entity.mob.pos))
        .min_by(|a, b| {
            (a.1 - point)
                .length_squared()
                .total_cmp(&(b.1 - point).length_squared())
        })
}

/// Eats an apple if the inventory has one.
fn eat_apple(host: &mut Host) -> bool {
    let Some(apple) = host.registry().item_id("openblock:apple") else {
        return false;
    };
    let previous = host.hotbar_selection();
    let slot = host
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == apple && stack.count > 0));
    let slot = match slot {
        Some(slot) => slot,
        None => {
            if host.give_item(apple, 4) != 0 {
                return false;
            }
            let Some(slot) = host
                .inventory()
                .iter()
                .position(|stack| stack.is_some_and(|stack| stack.item == apple && stack.count > 0))
            else {
                return false;
            };
            slot
        }
    };
    host.set_hotbar_selection(slot);
    let ate = host.eat_selected();
    host.set_hotbar_selection(previous);
    ate
}

/// Ensures a cobblestone stack is selected for placement.
fn ensure_selected_cobble(host: &mut Host) -> bool {
    let Some(item) = host.registry().item_id("openblock:cobblestone") else {
        return false;
    };
    if host.selected_stack().is_some_and(|stack| stack.item == item) {
        return true;
    }
    let slot = host
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == item && stack.count > 0));
    if let Some(slot) = slot {
        host.set_hotbar_selection(slot);
        return true;
    }
    if host.give_item(item, 64) != 0 {
        return false;
    }
    let slot = host
        .inventory()
        .iter()
        .position(|stack| stack.is_some_and(|stack| stack.item == item && stack.count > 0));
    if let Some(slot) = slot {
        host.set_hotbar_selection(slot);
        true
    } else {
        false
    }
}

/// Picks a surface column near the spawn for the next break/place pair.
fn pick_mining_target(host: &Host, center: WorldVec, tick: u64) -> Option<MiningTarget> {
    let mut offset = ((tick / 50) as i32).wrapping_mul(7).rem_euclid(64);
    for _ in 0..64 {
        let (x, z) = grid_offset(center, &mut offset);
        let Some(height) = host.world().height_at(x, z) else {
            continue;
        };
        if height <= ob_math::WORLD_MIN_Y + 1 || height >= ob_math::WORLD_MAX_Y - 2 {
            continue;
        }
        let block = host.world().get_block(BlockPos::new(x, height, z));
        if block == BlockId::AIR {
            continue;
        }
        return Some(MiningTarget { x, z, height });
    }
    None
}

/// Teleports the player above a surface target, looking straight down.
fn position_at_surface(host: &mut Host, target: MiningTarget) {
    host.player_mut().pos = WorldVec::new(target.x as f64 + 0.5, target.height as f64 + 2.0, target.z as f64 + 0.5);
    host.player_mut().flying = true;
    host.player_mut().pitch = -1.55;
    host.player_mut().yaw = 0.0;
    host.player_mut().velocity = WorldVec::ZERO;
}

/// Finds an air block directly above a loaded, solid surface column.
fn surface_air_cell(host: &Host, x: i32, z: i32) -> Option<BlockPos> {
    let height = host.world().height_at(x, z)?;
    let pos = BlockPos::new(x, height + 1, z);
    if !pos.in_world() || host.world().get_block(pos) != BlockId::AIR {
        return None;
    }
    if !host.world().is_solid(BlockPos::new(x, height, z)) {
        return None;
    }
    Some(pos)
}

/// Finds a collision-free mob feet position on a loaded surface column.
fn find_spawn_position(
    host: &Host,
    center: WorldVec,
    min_distance: f64,
    max_distance: f64,
    attempt: usize,
    salt: u32,
) -> Option<WorldVec> {
    let seed =
        host.world().seed() ^ (attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(salt).rotate_left(19);
    let angle = unit_interval(seed) * std::f64::consts::TAU;
    let distance = min_distance + unit_interval(mix(seed)) * (max_distance - min_distance);
    let x = (center.x + angle.cos() * distance).floor() as i32;
    let z = (center.z + angle.sin() * distance).floor() as i32;
    let height = host.world().height_at(x, z)?;
    let top = BlockPos::new(x, height, z);
    if !host.world().is_solid(top) {
        return None;
    }
    host.world().chunk(top.chunk())?;
    if host.world().get_block(top.offset(0, 1, 0)) != BlockId::AIR
        || host.world().get_block(top.offset(0, 2, 0)) != BlockId::AIR
    {
        return None;
    }
    Some(WorldVec::new(x as f64 + 0.5, height as f64 + 1.0, z as f64 + 0.5))
}

/// Deterministic outward grid offset around the spawn column.
fn grid_offset(center: WorldVec, offset: &mut i32) -> (i32, i32) {
    let cx = center.x.floor() as i32;
    let cz = center.z.floor() as i32;
    loop {
        *offset += 1;
        let ring = ((*offset as f64).sqrt() as i32).max(1);
        let span = ring * 2 + 1;
        let slot = *offset % (span * span);
        let dx = slot % span - ring;
        let dz = slot / span - ring;
        if dx * dx + dz * dz <= ring * ring + 1 {
            return (cx + dx * 3, cz + dz * 3);
        }
    }
}

/// Checks the session invariants.
fn check_invariants(host: &Host, max_items: usize) -> Result<(), String> {
    let player = host.player();
    if !player.pos.is_finite() || !player.velocity.is_finite() {
        return Err("player position/velocity is not finite".to_string());
    }
    if !player.health.is_finite() || !player.hunger.is_finite() {
        return Err("player vitals are not finite".to_string());
    }
    if player.pos.y < ITEM_VOID_Y - 64.0 {
        return Err(format!("player fell below the world: y={}", player.pos.y));
    }

    let max_mobs = MAX_PASSIVE_MOBS + MAX_HOSTILE_MOBS;
    if host.mob_count() > max_mobs {
        return Err(format!("mob cap exceeded: {} > {max_mobs}", host.mob_count()));
    }
    if host.item_entities().len() > max_items {
        return Err(format!(
            "item cap exceeded: {} > {max_items}",
            host.item_entities().len()
        ));
    }
    if host.furnaces().count() + host.chests().count() > MAX_CONTAINERS {
        return Err("container cap exceeded".to_string());
    }
    if host.world().chunk_count() > MAX_LOADED_COLUMNS {
        return Err(format!(
            "loaded column cap exceeded: {} > {MAX_LOADED_COLUMNS}",
            host.world().chunk_count()
        ));
    }
    let registry = host.registry();
    for (slot, stack) in host.inventory().iter().enumerate() {
        if let Some(stack) = stack {
            validate_stack(registry, *stack, "inventory", slot)?;
        }
    }
    for stack in host.equipment().iter().flatten() {
        validate_stack(registry, *stack, "equipment", 0)?;
    }
    for (index, entity) in host.item_entities().iter().enumerate() {
        if !entity.pos.is_finite() || !entity.velocity.is_finite() {
            return Err(format!("item {index} position/velocity is not finite"));
        }
        if entity.pos.y < ITEM_VOID_Y {
            return Err(format!(
                "item {} fell below the void line: y={}",
                entity.id, entity.pos.y
            ));
        }
        validate_stack(registry, entity.stack, "item entity", index)?;
        if !entity.age.is_finite() || entity.age < 0.0 {
            return Err(format!("item {} has an invalid age", entity.id));
        }
    }
    for entity in host.mobs() {
        if !entity.mob.pos.is_finite() {
            return Err(format!("mob {} position is not finite", entity.id));
        }
        if !entity.mob.health.is_finite() || entity.mob.health < 0.0 {
            return Err(format!("mob {} health is invalid", entity.id));
        }
        if !entity.mob.age.is_finite() || entity.mob.age < 0.0 {
            return Err(format!("mob {} age is invalid", entity.id));
        }
    }
    Ok(())
}

/// Validates one stack's item id and count.
fn validate_stack(registry: &Registry, stack: ItemStack, area: &str, index: usize) -> Result<(), String> {
    if stack.count == 0 || stack.count > MAX_STACK_SIZE {
        return Err(format!("{area}[{index}] has invalid count {}", stack.count));
    }
    if stack.item.0 as usize >= registry.item_len() {
        return Err(format!("{area}[{index}] has invalid item id {}", stack.item.0));
    }
    if let Some(durability) = stack.durability
        && durability == 0
    {
        return Err(format!("{area}[{index}] has zero durability"));
    }
    Ok(())
}

/// Prints one progress line.
fn print_progress(step: u64, total: u64, host: &Host, saves: &SaveSeries, rss: Option<u64>) {
    let percent = if total == 0 {
        0.0
    } else {
        step as f64 * 100.0 / total as f64
    };
    println!(
        "progress: {}/{} ({:.1}%) | mobs {} | items {} | columns {} | saves {} | last save {} | RSS {}",
        step,
        total,
        percent,
        host.mob_count(),
        host.item_entities().len(),
        host.world().chunk_count(),
        saves.sizes.len(),
        measure::bytes_label(saves.last_size),
        rss.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );
}

/// Sums all regular files in a directory one level deep.
fn directory_size(dir: &Path) -> Result<u64, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut total = 0u64;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

/// Keeps the larger of two optional values.
fn max_option(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

/// Deterministic 64-bit mix.
fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Maps a 64-bit value to `[0, 1)`.
fn unit_interval(value: u64) -> f64 {
    (value >> 11) as f64 / (1u64 << 53) as f64
}

/// Text report bundle.
struct Report {
    csv: String,
    markdown: String,
}

/// Builds CSV and markdown summaries.
#[allow(clippy::too_many_arguments)]
fn build_report(
    options: &Options,
    counters: &Counters,
    saves: &SaveSeries,
    save_stats: &Stats,
    progress: &[Progress],
    wall_s: f64,
    ticks_per_second: f64,
    rss_start: Option<u64>,
    rss_end: Option<u64>,
    rss_peak: Option<u64>,
    peak_items: usize,
    peak_mobs: usize,
    hash_before: u64,
    hash_after: Option<u64>,
    load_ms: f64,
    chunks_after: usize,
    tick_after: u64,
    verdict: bool,
) -> Report {
    let mut csv = String::from("metric,value\n");
    csv.push_str(&format!("ticks,{}\n", options.ticks));
    csv.push_str(&format!("wall_s,{wall_s:.3}\n"));
    csv.push_str(&format!("ticks_per_second,{ticks_per_second:.2}\n"));
    csv.push_str(&format!("breaks,{}\n", counters.breaks_changed));
    csv.push_str(&format!("places,{}\n", counters.places_changed));
    csv.push_str(&format!("attacks,{}\n", counters.attacks));
    csv.push_str(&format!("kills,{}\n", counters.kills));
    csv.push_str(&format!("deaths,{}\n", counters.deaths));
    csv.push_str(&format!("peak_items,{peak_items}\n"));
    csv.push_str(&format!("peak_mobs,{peak_mobs}\n"));
    csv.push_str(&format!("saves,{}\n", counters.saves));
    csv.push_str(&format!("save_mean_ms,{:.3}\n", save_stats.mean_ms));
    csv.push_str(&format!("save_p95_ms,{:.3}\n", save_stats.p95_ms));
    csv.push_str(&format!("save_first_bytes,{}\n", saves.first_size));
    csv.push_str(&format!("save_last_bytes,{}\n", saves.last_size));
    csv.push_str(&format!(
        "rss_start_kib,{}\n",
        rss_start.map_or(String::new(), |value| value.to_string())
    ));
    csv.push_str(&format!(
        "rss_end_kib,{}\n",
        rss_end.map_or(String::new(), |value| value.to_string())
    ));
    csv.push_str(&format!("hash_before,{hash_before:016x}\n"));
    csv.push_str(&format!(
        "hash_after,{}\n",
        hash_after.map_or(String::new(), |hash| format!("{hash:016x}"))
    ));
    csv.push_str(&format!("reload_ms,{load_ms:.3}\n"));
    csv.push_str(&format!("reload_chunks,{chunks_after}\n"));
    csv.push_str(&format!("reload_ticks,{tick_after}\n"));
    csv.push_str(&format!("verdict,{verdict}\n"));

    let mut markdown = String::from("# Soak summary\n\n");
    markdown.push_str(&format!(
        "- CPU: {}\n- OS: {}\n- Seed: {}\n- Ticks: {}\n- Radius: {} chunks\n- Profile: {}\n",
        measure::cpu_model(),
        measure::os_description(),
        options.seed,
        options.ticks,
        options.radius,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));
    markdown.push_str("\n| Metric | Value |\n| --- | ---: |\n");
    markdown.push_str(&format!(
        "| Wall time / ticks/s | {:.1} s / {:.1} |\n",
        wall_s, ticks_per_second
    ));
    markdown.push_str(&format!(
        "| Mining / building | {} / {} changed |\n",
        counters.breaks_changed, counters.places_changed
    ));
    markdown.push_str(&format!(
        "| Combat / kills / deaths | {} / {} / {} |\n",
        counters.attacks, counters.kills, counters.deaths
    ));
    markdown.push_str(&format!("| Peak items / mobs | {} / {} |\n", peak_items, peak_mobs));
    markdown.push_str(&format!(
        "| Saves | {} (mean {:.2} ms, p95 {:.2} ms) |\n",
        counters.saves, save_stats.mean_ms, save_stats.p95_ms
    ));
    markdown.push_str(&format!(
        "| Save size | {} -> {} |\n",
        measure::bytes_label(saves.first_size),
        measure::bytes_label(saves.last_size)
    ));
    markdown.push_str(&format!(
        "| RSS start / end / peak | {} / {} / {} |\n",
        rss_start.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_end.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_peak.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    ));
    markdown.push_str(&format!(
        "| Save/reload hash | `{:016x}` -> `{}` |\n",
        hash_before,
        hash_after
            .map(|hash| format!("{hash:016x}"))
            .unwrap_or_else(|| "failed".to_string())
    ));
    markdown.push_str(&format!("| Invariants | {} |\n", if verdict { "PASS" } else { "FAIL" }));
    markdown.push_str("\n## Progress\n\n");
    markdown.push_str("| Tick | Elapsed s | Mobs | Items |\n| ---: | ---: | ---: | ---: |\n");
    for sample in progress {
        markdown.push_str(&format!(
            "| {} | {:.1} | {} | {} |\n",
            sample.tick, sample.elapsed_s, sample.mobs, sample.items
        ));
    }
    Report { csv, markdown }
}

//! Authoritative server-simulation benchmark for the OpenBlock 1.0 envelope.
//!
//! The tool opens a real [`ob_server::Host`] on a temporary world, streams a
//! fixed area, then times batches of 20 Hz ticks across increasingly populated
//! scenes:
//!
//! 1. `idle` - no moving player, no natural passive spawns, no items, no containers.
//! 2. `player` - one simulated player flying a deterministic circle.
//! 3. `mobs` - natural passive/hostile caps reached through a day/night pre-roll
//!    (quick mode fills both caps directly instead).
//! 4. `items` - dropped item entities from real block breaks plus a stress batch.
//! 5. `containers` - lit furnaces and full chests ticked by the host.
//! 6. `streaming` - a separate measurement of cold chunk generation (not part of
//!    the tick samples, because `Host::tick` itself does not stream).
//!
//! Every tick is measured individually; the report carries mean/p50/p95/p99/max,
//! the marginal cost over `idle`, process RSS before/after each phase, save
//! latency/size and a save/reload state-hash check.
//!
//! ```text
//! server_bench --quick
//! server_bench --ticks 1000 --radius 10 --csv target/server.csv
//! ```

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ob_content::{BlockId, MobKind, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{
    CHEST_SLOTS, DAY_LENGTH_TICKS, FurnaceSlot, GameMode, Host, HostConfig, ItemStack, MAX_HOSTILE_MOBS,
    MAX_PASSIVE_MOBS, MAX_STACK_SIZE, NIGHT_START_TICK,
};
use ob_sim::{BlockAction, MAX_HEALTH, MAX_HUNGER, PlayerInput};
use ob_tools::measure::{self, Stats};

/// World directory name used under the benchmark data root.
const WORLD_NAME: &str = "server-bench";
/// Tick budget target from the 1.0 performance envelope (`PERF-TICK-009`).
const TICK_P99_TARGET_MS: f64 = 35.0;
/// Radius used by the passive-spawn suppression pass, in blocks.
const SPAWN_SUPPRESS_RADIUS: i32 = 48;
/// Chunks generated per `ensure_streaming_around` batch while draining.
const STREAM_BATCH: usize = 4096;

const USAGE: &str = "\
usage: server_bench [options]
  --quick              CI profile: 300 ticks, radius 5, small entity batches
  --ticks N            measured ticks per phase (default 1000, quick 300)
  --radius N           streaming radius in chunks (default 10, quick 5)
  --seed S             world seed (default 42)
  --items N            dropped item entities in the item phase (default 128, quick 32)
  --furnaces N         lit furnaces in the container phase (default 32, quick 8)
  --chests N           full chests in the container phase (default 16, quick 4)
  --direct-mobs        skip the day/night pre-roll and fill mob caps directly
  --data-dir DIR       benchmark data root instead of a temporary directory
  --clean              remove an existing <DIR>/worlds/server-bench before opening
  --keep               keep the temporary data directory for inspection
  --csv PATH           write the phase/summary CSV
  --markdown PATH      write the markdown report
  --help";

/// Parsed command-line options.
#[derive(Debug, Clone)]
struct Options {
    ticks: u64,
    radius: i32,
    seed: u64,
    item_count: usize,
    furnace_count: usize,
    chest_count: usize,
    direct_mobs: bool,
    data_dir: Option<PathBuf>,
    clean: bool,
    keep: bool,
    csv: Option<PathBuf>,
    markdown: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ticks: 1000,
            radius: 10,
            seed: 42,
            item_count: 128,
            furnace_count: 32,
            chest_count: 16,
            direct_mobs: false,
            data_dir: None,
            clean: false,
            keep: false,
            csv: None,
            markdown: None,
        }
    }
}

/// One measured phase.
struct PhaseResult {
    name: &'static str,
    detail: String,
    stats: Stats,
    ticks: u64,
    rss_after_kib: Option<u64>,
}

impl PhaseResult {
    /// Whether the phase's p99 stays inside the 35 ms 1.0 tick envelope.
    fn passes_envelope(&self) -> bool {
        self.stats.p99_ms <= TICK_P99_TARGET_MS
    }
}

/// Cold-streaming measurement.
struct StreamResult {
    chunks: usize,
    elapsed_ms: f64,
}

impl StreamResult {
    /// Milliseconds per generated chunk.
    fn ms_per_chunk(&self) -> f64 {
        if self.chunks == 0 {
            0.0
        } else {
            self.elapsed_ms / self.chunks as f64
        }
    }

    /// Generated chunks per second.
    fn chunks_per_second(&self) -> f64 {
        if self.elapsed_ms <= 0.0 {
            0.0
        } else {
            self.chunks as f64 * 1000.0 / self.elapsed_ms
        }
    }
}

/// Save and reload measurements.
struct PersistenceResult {
    save_stats: Stats,
    save_bytes: u64,
    sidecars: Vec<(String, u64)>,
    load_ms: f64,
    hash_before: u64,
    hash_after: Option<u64>,
    load_error: Option<String>,
    expected_ticks: u64,
    loaded_ticks: u64,
    chunks_before: usize,
    chunks_after: usize,
    mobs_before: usize,
    mobs_after: usize,
    items_before: usize,
    items_after: usize,
    furnaces_before: usize,
    furnaces_after: usize,
    chests_before: usize,
    chests_after: usize,
}

/// Runs the benchmark and returns a process exit code.
fn main() {
    ob_core::init_logging();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("server_bench: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match run(&options) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(message) => {
            eprintln!("server_bench: {message}");
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
    let mut explicit = [false; 5];
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
            "items" => {
                options.item_count = parse_usize(&name, &value, 0, 2048)?;
                explicit[2] = true;
            }
            "furnaces" => {
                options.furnace_count = parse_usize(&name, &value, 0, 128)?;
                explicit[3] = true;
            }
            "chests" => {
                options.chest_count = parse_usize(&name, &value, 0, 128)?;
                explicit[4] = true;
            }
            "direct-mobs" => options.direct_mobs = true,
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
            options.ticks = 300;
        }
        if !explicit[1] {
            options.radius = 5;
        }
        if !explicit[2] {
            options.item_count = 32;
        }
        if !explicit[3] {
            options.furnace_count = 8;
        }
        if !explicit[4] {
            options.chest_count = 4;
        }
        options.direct_mobs = true;
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

/// Parses one decimal or `0x` hexadecimal seed.
fn parse_seed(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let parsed = if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        trimmed.parse()
    };
    parsed.map_err(|_| format!("invalid seed '{value}'"))
}

/// Executes the benchmark; returns whether every envelope check passed.
fn run(options: &Options) -> Result<bool, String> {
    let registry = Arc::new(Registry::base());
    let temp_guard = match &options.data_dir {
        Some(root) => {
            std::fs::create_dir_all(root)
                .map_err(|error| format!("cannot create data directory {}: {error}", root.display()))?;
            let world_dir = world_dir(root);
            if world_dir.exists() {
                if options.clean {
                    std::fs::remove_dir_all(&world_dir).map_err(|error| {
                        format!("cannot clean existing world directory {}: {error}", world_dir.display())
                    })?;
                } else {
                    return Err(format!(
                        "{} already exists; pass --clean or use a fresh --data-dir",
                        world_dir.display()
                    ));
                }
            }
            None
        }
        None => Some(measure::temp_dir("openblock-server-bench").map_err(|error| error.to_string())?),
    };
    let root = match &options.data_dir {
        Some(path) => path.clone(),
        None => temp_guard
            .as_ref()
            .map(|guard| guard.path().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("target/server-bench-data")),
    };

    println!("server_bench - authoritative Host tick benchmark");
    println!("cpu:    {}", measure::cpu_model());
    println!("os:     {}", measure::os_description());
    println!(
        "config: seed {}, radius {} chunks, {} ticks/phase, {} items, {} furnaces, {} chests, mobs {}",
        options.seed,
        options.radius,
        options.ticks,
        options.item_count,
        options.furnace_count,
        options.chest_count,
        if options.direct_mobs {
            "direct-fill"
        } else {
            "natural day/night"
        }
    );
    println!("profile: {}", if cfg!(debug_assertions) { "debug" } else { "release" });
    println!("data:   {}", root.display());
    println!();

    let config = HostConfig {
        seed: options.seed,
        world_name: WORLD_NAME.to_string(),
        data_dir: Some(root.clone()),
        render_distance_chunks: options.radius,
        generate_budget: 64,
    };
    let open_start = Instant::now();
    let mut host = Host::open(Arc::clone(&registry), config.clone()).map_err(|error| format!("open host: {error}"))?;
    let open_ms = open_start.elapsed().as_secs_f64() * 1000.0;
    host.set_game_mode(GameMode::Survival);
    prepare_player(&mut host);
    prepare_inventory(&mut host);
    let rss_after_open = measure::rss_kib();

    let center = host.spawn();
    let initial_stream = stream_radius(&mut host, center, options.radius)?;
    let rss_after_stream = measure::rss_kib();
    let mut phases = Vec::new();

    // A clean idle baseline: remove grass in the passive-spawn ring so the
    // 20 Hz mob spawner cannot fill the world with cows before we measure.
    let suppressed_grass = suppress_grass_spawns(&mut host, center);
    println!(
        "setup: open {:.1} ms, streamed {} columns in {:.1} ms ({:.2} ms/chunk), suppressed {} grass tops",
        open_ms,
        initial_stream.chunks,
        initial_stream.elapsed_ms,
        initial_stream.ms_per_chunk(),
        suppressed_grass.len()
    );

    phases.push(run_phase(&mut host, "idle", options.ticks, false));
    phases.push(run_phase(&mut host, "player", options.ticks, true));

    let mobs = if options.direct_mobs {
        restore_grass_spawns(&mut host, &suppressed_grass);
        fill_mob_caps(&mut host)
    } else {
        // Keep passive spawning suppressed until late in the day so the animals
        // are young enough not to hit their 10-minute despawn age during the
        // measured 1,000-tick phase.
        wait_for_day_phase(&mut host, 11_000);
        restore_grass_spawns(&mut host, &suppressed_grass);
        natural_mob_pre_roll(&mut host)
    };
    println!(
        "mobs: {} passive, {} hostile ({})",
        host.passive_mob_count(),
        host.hostile_mob_count(),
        mobs
    );
    phases.push(run_phase(&mut host, "mobs", options.ticks, false));

    let items = prepare_items(&mut host, options.item_count);
    println!(
        "items: {} live ({} break drops + {} spawned), {} break actions changed the world",
        host.item_entities().len(),
        items.break_drops,
        items.spawned,
        items.breaks_changed
    );
    phases.push(run_phase(&mut host, "items", options.ticks, false));

    let containers = prepare_containers(&mut host, options.furnace_count, options.chest_count);
    println!(
        "containers: {} furnaces, {} chests ({} furnace slots filled, {} chest slots filled)",
        containers.furnaces, containers.chests, containers.furnace_slots, containers.chest_slots
    );
    phases.push(run_phase(&mut host, "containers", options.ticks, true));

    // Cold generation is measured outside the tick samples; Host::tick never
    // generates chunks.
    let fresh_center = WorldVec::new(center.x + 512.0, center.y, center.z + 512.0);
    let fresh_stream = stream_radius(&mut host, fresh_center, 4)?;
    // The fresh area is a streaming measurement, not part of the 2,025-column
    // reference world; unload it before measuring memory, save size and reload.
    let removed = host.unload_chunks_outside(&[center], options.radius);
    let _ = host.take_unloaded_chunks();
    println!(
        "streaming: {} fresh columns generated in {:.1} ms, {} removed again; {} columns resident",
        fresh_stream.chunks,
        fresh_stream.elapsed_ms,
        removed,
        host.world().chunk_count()
    );

    let (persistence, host) = persistence_check(host, &root, &registry, &config, center)?;
    let rss_end = measure::rss_kib();

    print_phases(&phases);
    print_streaming(&initial_stream, &fresh_stream);
    print_memory(&phases, rss_after_open, rss_after_stream, rss_end, &host);
    print_persistence(&persistence);
    println!();
    let idle_mean = phases
        .iter()
        .find(|phase| phase.name == "idle")
        .map_or(0.0, |phase| phase.stats.mean_ms);
    print_marginal(&phases, idle_mean);
    println!();
    println!("notes:");
    println!("  tick samples exclude chunk generation; `Host::tick` does not stream.");
    println!("  p99 target: {TICK_P99_TARGET_MS:.1} ms (PERF-TICK-009 / 1.0 single-player baseline).");
    println!("  RSS is the Linux /proc/self/status VmRSS proxy for the whole process, not only the host.");
    println!("  multiplayer bots/network capacity are a follow-up (PERF-SRV-002, MS-15).");

    let report = build_report(
        options,
        &phases,
        &initial_stream,
        &fresh_stream,
        &persistence,
        rss_after_open,
        rss_after_stream,
        rss_end,
        &host,
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

    let mut passed = true;
    for phase in &phases {
        if !phase.passes_envelope() {
            passed = false;
            eprintln!(
                "server_bench: {} p99 {:.3} ms exceeds {TICK_P99_TARGET_MS:.1} ms",
                phase.name, phase.stats.p99_ms
            );
        }
    }
    if !persistence.hash_matches() {
        passed = false;
        eprintln!("server_bench: save/reload state hash did not match");
    }
    if let Some(error) = &persistence.load_error {
        passed = false;
        eprintln!("server_bench: reload failed: {error}");
    }
    if options.keep
        && let Some(guard) = temp_guard
    {
        let path = guard.keep();
        println!("kept data directory: {}", path.display());
    }
    Ok(passed)
}

/// Puts the host player into a stable survival state for the benchmark.
fn prepare_player(host: &mut Host) {
    let player = host.player_mut();
    player.flying = true;
    player.health = MAX_HEALTH;
    player.hunger = MAX_HUNGER;
    player.dead = false;
    player.velocity = WorldVec::ZERO;
}

/// Fills the hotbar and equipped-armor slots so every persistence sidecar is non-trivial.
fn prepare_inventory(host: &mut Host) {
    for (name, count) in [
        ("openblock:iron_pickaxe", 1),
        ("openblock:apple", 16),
        ("openblock:coal", 64),
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

/// Resets vitals during long phases so the benchmark measures simulation cost,
/// not a death/respawn cycle.
fn reset_vitals(host: &mut Host) {
    prepare_player(host);
}

/// Drains a streaming radius around `center`, returning the generated count.
fn stream_radius(host: &mut Host, center: WorldVec, radius: i32) -> Result<StreamResult, String> {
    let started = Instant::now();
    let mut chunks = 0usize;
    loop {
        let generated = host.ensure_streaming_around(center, radius, STREAM_BATCH);
        chunks += generated;
        if generated == 0 {
            break;
        }
    }
    Ok(StreamResult {
        chunks,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Runs one measured tick phase.
fn run_phase(host: &mut Host, name: &'static str, ticks: u64, moving: bool) -> PhaseResult {
    let start_tick = host.tick_count();
    let mut samples = Vec::with_capacity(ticks as usize);
    for step in 0..ticks {
        if moving {
            // A slow circle keeps the player inside the streamed area.
            host.player_mut().yaw = (start_tick + step) as f32 * 0.02;
        }
        let input = moving_input(step, moving);
        let started = Instant::now();
        host.tick(&input);
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        if (step + 1) % 100 == 0 {
            reset_vitals(host);
        }
    }
    let stats = Stats::from_samples(&samples);
    let detail = describe_population(host);
    PhaseResult {
        name,
        detail,
        stats,
        ticks,
        rss_after_kib: measure::rss_kib(),
    }
}

/// Deterministic player input pattern for moving phases.
fn moving_input(step: u64, moving: bool) -> PlayerInput {
    if !moving {
        return PlayerInput::default();
    }
    let quadrant = (step / 128) % 4;
    PlayerInput {
        forward: 1.0,
        strafe: match quadrant {
            0 => 0.0,
            1 => 0.8,
            2 => 0.0,
            _ => -0.8,
        },
        jump: step.is_multiple_of(64),
        sprint: (step / 256).is_multiple_of(2),
        sneak: false,
        fly_toggle: false,
    }
}

/// Human-readable population summary for a phase row.
fn describe_population(host: &Host) -> String {
    format!(
        "{} mobs, {} items, {} furnaces, {} chests",
        host.mob_count(),
        host.item_entities().len(),
        host.furnaces().count(),
        host.chests().count()
    )
}

/// Removes grass tops inside the passive-spawn range, returning the originals.
fn suppress_grass_spawns(host: &mut Host, center: WorldVec) -> Vec<(BlockPos, BlockId)> {
    let grass = host.registry().id("openblock:grass").unwrap_or(BlockId::AIR);
    let dirt = host.registry().id("openblock:dirt").unwrap_or(BlockId::AIR);
    if grass == BlockId::AIR || dirt == BlockId::AIR {
        return Vec::new();
    }
    let cx = center.x.floor() as i32;
    let cz = center.z.floor() as i32;
    let mut originals = Vec::new();
    for z in (cz - SPAWN_SUPPRESS_RADIUS)..=(cz + SPAWN_SUPPRESS_RADIUS) {
        for x in (cx - SPAWN_SUPPRESS_RADIUS)..=(cx + SPAWN_SUPPRESS_RADIUS) {
            let Some(height) = host.world().height_at(x, z) else {
                continue;
            };
            let pos = BlockPos::new(x, height, z);
            if host.world().get_block(pos) == grass {
                originals.push((pos, grass));
                host.world_mut().set_block(pos, dirt);
            }
        }
    }
    originals
}

/// Restores grass tops removed by [`suppress_grass_spawns`].
fn restore_grass_spawns(host: &mut Host, originals: &[(BlockPos, BlockId)]) {
    for &(pos, block) in originals {
        host.world_mut().set_block(pos, block);
    }
}

/// Advances idle ticks (with passive spawning suppressed) until the day phase
/// reaches `target_phase`, wrapping to the next day when necessary.
fn wait_for_day_phase(host: &mut Host, target_phase: u64) {
    let phase = host.tick_count() % DAY_LENGTH_TICKS;
    let ticks = if phase < target_phase {
        target_phase - phase
    } else {
        DAY_LENGTH_TICKS - phase + target_phase
    };
    let idle = PlayerInput::default();
    for step in 0..ticks {
        host.tick(&idle);
        if (step + 1) % 100 == 0 {
            reset_vitals(host);
        }
    }
}

/// Brings both natural mob caps to their limits directly (quick mode).
fn fill_mob_caps(host: &mut Host) -> &'static str {
    let center = host.player().pos;
    for kind in [MobKind::Cow, MobKind::Pig, MobKind::Sheep, MobKind::Chicken] {
        let mut attempt = 0usize;
        while host.passive_mob_count() < MAX_PASSIVE_MOBS && attempt < 256 {
            if let Some(pos) = find_spawn_position(host, center, 12.0, 40.0, attempt, kind as u32) {
                let _ = host.spawn_mob(kind, pos);
            }
            attempt += 1;
        }
    }
    let mut attempt = 0usize;
    while host.hostile_mob_count() < MAX_HOSTILE_MOBS && attempt < 256 {
        if let Some(pos) = find_spawn_position(host, center, 16.0, 48.0, attempt, 0x2A11) {
            let _ = host.spawn_mob(MobKind::Zombie, pos);
        }
        attempt += 1;
    }
    "direct-fill"
}

/// Pre-rolls ticks until natural passive and hostile caps are populated.
fn natural_mob_pre_roll(host: &mut Host) -> &'static str {
    let phase = host.tick_count() % DAY_LENGTH_TICKS;
    let target_phase = NIGHT_START_TICK + 100;
    let ticks_to_night = if phase < target_phase {
        target_phase - phase
    } else if phase >= NIGHT_START_TICK + 2_000 {
        DAY_LENGTH_TICKS + target_phase - phase
    } else {
        0
    };
    let idle = PlayerInput::default();
    for step in 0..ticks_to_night {
        host.tick(&idle);
        if (step + 1) % 100 == 0 {
            reset_vitals(host);
        }
    }
    // Give the 20-tick spawn cadence a little extra time to fill hostiles.
    let mut extra = 0u64;
    while host.hostile_mob_count() < MAX_HOSTILE_MOBS && extra < 600 {
        host.tick(&idle);
        extra += 1;
    }
    let missing_passive = MAX_PASSIVE_MOBS.saturating_sub(host.passive_mob_count());
    let missing_hostile = MAX_HOSTILE_MOBS.saturating_sub(host.hostile_mob_count());
    if missing_passive > 0 || missing_hostile > 0 {
        fill_mob_caps(host);
        "natural+fallback"
    } else {
        "natural"
    }
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
    let feet = WorldVec::new(x as f64 + 0.5, height as f64 + 1.0, z as f64 + 0.5);
    host.world().chunk(top.chunk())?;
    if host.world().get_block(top.offset(0, 1, 0)) != BlockId::AIR
        || host.world().get_block(top.offset(0, 2, 0)) != BlockId::AIR
    {
        return None;
    }
    Some(feet)
}

/// Result of the item-preparation phase.
struct ItemsPrepared {
    breaks_changed: usize,
    break_drops: usize,
    spawned: usize,
}

/// Creates real break drops and a stress batch of item entities.
fn prepare_items(host: &mut Host, target_items: usize) -> ItemsPrepared {
    host.set_game_mode(GameMode::Survival);
    prepare_player(host);
    let center = host.spawn();
    let breaks = (target_items / 3).max(8);
    let mut breaks_changed = 0usize;
    let mut break_drops = 0usize;
    let mut offset = 0i32;
    for i in 0..breaks {
        let (x, z) = grid_offset(center, &mut offset);
        let Some(height) = host.world().height_at(x, z) else {
            continue;
        };
        host.player_mut().pos = WorldVec::new(x as f64 + 0.5, height as f64 + 2.0, z as f64 + 0.5);
        host.player_mut().flying = true;
        host.player_mut().pitch = -1.55;
        host.player_mut().yaw = 0.0;
        let action = BlockAction {
            break_block: true,
            place_block: false,
            place: BlockId::AIR,
        };
        let outcome = host.apply_action_with_outcome(&action);
        if outcome.changed {
            breaks_changed += 1;
            break_drops += outcome.drops.len();
        }
        if i % 16 == 0 {
            reset_vitals(host);
        }
    }

    let mut spawned = 0usize;
    let mut live = host.item_entities().len();
    let mut attempt = 0usize;
    while live < target_items && attempt < target_items * 8 {
        let (x, z) = grid_offset(center, &mut offset);
        if let Some(height) = host.world().height_at(x, z) {
            let pos = WorldVec::new(
                x as f64 + 0.5,
                height as f64 + 2.0 + (attempt % 6) as f64,
                z as f64 + 0.5,
            );
            if let Some(stack) = item_stack(host.registry(), attempt) {
                host.spawn_item_entity(pos, stack);
                spawned += 1;
                live += 1;
            }
        }
        attempt += 1;
    }

    // Move the player far enough above the drops that survival pickup does not
    // delete the population we want to measure.
    host.player_mut().pos = WorldVec::new(center.x + 0.5, center.y + 40.0, center.z + 0.5);
    host.player_mut().flying = true;
    host.player_mut().pitch = 0.0;
    ItemsPrepared {
        breaks_changed,
        break_drops,
        spawned,
    }
}

/// Builds one deterministic dropped stack for the item stress batch.
fn item_stack(registry: &Registry, index: usize) -> Option<ItemStack> {
    const NAMES: [&str; 6] = [
        "openblock:cobblestone",
        "openblock:dirt",
        "openblock:coal",
        "openblock:iron_ingot",
        "openblock:planks",
        "openblock:apple",
    ];
    let id = registry.item_id(NAMES[index % NAMES.len()])?;
    let max = registry.item(id).max_stack.clamp(1, MAX_STACK_SIZE);
    Some(ItemStack::new(id, max.min(16)))
}

/// Container-preparation result.
struct ContainersPrepared {
    furnaces: usize,
    chests: usize,
    furnace_slots: usize,
    chest_slots: usize,
}

/// Places and fills furnaces and chests through the public authoritative edit API.
fn prepare_containers(host: &mut Host, furnaces: usize, chests: usize) -> ContainersPrepared {
    let center = host.spawn();
    let furnace_id = host.registry().id("openblock:furnace").unwrap_or(BlockId::AIR);
    let chest_id = host.registry().id("openblock:chest").unwrap_or(BlockId::AIR);
    let raw_iron = host.registry().item_id("openblock:raw_iron");
    let coal = host.registry().item_id("openblock:coal");
    let cobble = host.registry().item_id("openblock:cobblestone");
    let dirt = host.registry().item_id("openblock:dirt");
    let mut result = ContainersPrepared {
        furnaces: 0,
        chests: 0,
        furnace_slots: 0,
        chest_slots: 0,
    };
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
            result.furnaces += 1;
            if let (Some(iron), Some(fuel)) = (raw_iron, coal) {
                if host
                    .put_furnace_slot(pos, FurnaceSlot::Input, ItemStack::new(iron, 64))
                    .is_none()
                {
                    result.furnace_slots += 1;
                }
                if host
                    .put_furnace_slot(pos, FurnaceSlot::Fuel, ItemStack::new(fuel, 64))
                    .is_none()
                {
                    result.furnace_slots += 1;
                }
            }
        } else {
            if chest_id == BlockId::AIR || !host.apply_remote_edit(pos, chest_id) {
                continue;
            }
            result.chests += 1;
            for slot in 0..CHEST_SLOTS {
                let item = if slot % 2 == 0 { cobble } else { dirt };
                let Some(item) = item else {
                    continue;
                };
                if host.put_chest_slot(pos, slot, ItemStack::new(item, 64)).is_none() {
                    result.chest_slots += 1;
                }
            }
        }
    }
    result
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

/// A deterministic outward grid offset around the spawn column.
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

/// The exact column set resident before the save: two Chebyshev squares.
fn desired_chunks(center: WorldVec, radius: i32) -> HashSet<ob_math::ChunkPos> {
    let mut desired = HashSet::new();
    let cx = center.x.floor() as i32 >> 4;
    let cz = center.z.floor() as i32 >> 4;
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            desired.insert(ob_math::ChunkPos::new(cx + dx, cz + dz));
        }
    }
    desired
}

/// Drops every loaded column that is not in the expected pre-save set.
fn retain_chunks(host: &mut Host, desired: &HashSet<ob_math::ChunkPos>) {
    let extras: Vec<ob_math::ChunkPos> = host
        .world()
        .chunks()
        .map(|(pos, _)| *pos)
        .filter(|pos| !desired.contains(pos))
        .collect();
    for pos in extras {
        host.world_mut().remove_chunk(pos);
    }
}

/// Measures repeated saves, file sizes and a full save/reload cycle.
fn persistence_check(
    mut host: Host,
    root: &Path,
    registry: &Arc<Registry>,
    config: &HostConfig,
    center: WorldVec,
) -> Result<(PersistenceResult, Host), String> {
    let mut save_samples = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        host.save().map_err(|error| format!("save failed: {error}"))?;
        save_samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let dir = world_dir(root);
    let (save_bytes, sidecars) = directory_files(&dir)?;
    let save_stats = Stats::from_samples(&save_samples);

    let hash_before = host.state_hash();
    let expected_ticks = host.tick_count();
    let chunks_before = host.world().chunk_count();
    let mobs_before = host.mob_count();
    let items_before = host.item_entities().len();
    let furnaces_before = host.furnaces().count();
    let chests_before = host.chests().count();
    drop(host);

    let load_start = Instant::now();
    let reopened = Host::open(Arc::clone(registry), config.clone());
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;
    let (host, hash_after, loaded_ticks, chunks_after, mobs_after, items_after, furnaces_after, chests_after, error) =
        match reopened {
            Ok(mut host) => {
                // `Host::open` unloads columns outside the 2-chunk load radius
                // around spawn. Rebuild exactly the two resident squares that
                // existed before the save, then compare hashes over the same
                // world set.
                let desired = desired_chunks(center, config.render_distance_chunks);
                retain_chunks(&mut host, &desired);
                let mut guard = 0usize;
                while host.ensure_streaming_around(center, config.render_distance_chunks, STREAM_BATCH) > 0 {
                    guard += 1;
                    if guard > 64 {
                        break;
                    }
                }
                let hash = host.state_hash();
                let ticks = host.tick_count();
                let chunks = host.world().chunk_count();
                let mobs = host.mob_count();
                let items = host.item_entities().len();
                let furnaces = host.furnaces().count();
                let chests = host.chests().count();
                (host, Some(hash), ticks, chunks, mobs, items, furnaces, chests, None)
            }
            Err(error) => {
                let placeholder = Host::open(
                    Arc::clone(registry),
                    HostConfig {
                        seed: config.seed,
                        world_name: format!("{}-reload-failed", config.world_name),
                        data_dir: config.data_dir.clone(),
                        render_distance_chunks: config.render_distance_chunks,
                        generate_budget: config.generate_budget,
                    },
                );
                match placeholder {
                    Ok(host) => (host, None, 0, 0, 0, 0, 0, 0, Some(error.to_string())),
                    Err(second) => {
                        return Err(format!("reload failed ({error}); fallback host failed ({second})"));
                    }
                }
            }
        };

    Ok((
        PersistenceResult {
            save_stats,
            save_bytes,
            sidecars,
            load_ms,
            hash_before,
            hash_after,
            load_error: error,
            expected_ticks,
            loaded_ticks,
            chunks_before,
            chunks_after,
            mobs_before,
            mobs_after,
            items_before,
            items_after,
            furnaces_before,
            furnaces_after,
            chests_before,
            chests_after,
        },
        host,
    ))
}

impl PersistenceResult {
    /// Whether the reloaded host reproduces the exact state hash.
    fn hash_matches(&self) -> bool {
        self.hash_after == Some(self.hash_before)
    }
}

/// The world directory for the fixed benchmark world name.
fn world_dir(root: &Path) -> PathBuf {
    root.join("worlds").join(WORLD_NAME)
}

/// Sums file sizes in one world directory and returns a sorted breakdown.
fn directory_files(dir: &Path) -> Result<(u64, Vec<(String, u64)>), String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let entries = std::fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            continue;
        }
        let size = metadata.len();
        total += size;
        files.push((entry.file_name().to_string_lossy().into_owned(), size));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((total, files))
}

/// Prints the phase table.
fn print_phases(phases: &[PhaseResult]) {
    println!();
    println!("## Tick phases");
    println!();
    println!(
        "| Scenario | Ticks | Mean ms | p50 | p95 | p99 | Max | TPS | p99 <= {:.0} ms | Population |",
        TICK_P99_TARGET_MS
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: | --- |");
    for phase in phases {
        println!(
            "| {} | {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.1} | {} | {} |",
            phase.name,
            phase.ticks,
            phase.stats.mean_ms,
            phase.stats.p50_ms,
            phase.stats.p95_ms,
            phase.stats.p99_ms,
            phase.stats.max_ms,
            phase.stats.per_second(),
            if phase.passes_envelope() { "PASS" } else { "FAIL" },
            phase.detail
        );
    }
}

/// Prints the streaming measurements.
fn print_streaming(initial: &StreamResult, fresh: &StreamResult) {
    println!();
    println!("## World streaming (outside the tick samples)");
    println!();
    println!("| Measurement | Chunks | Wall ms | ms/chunk | Chunks/s |");
    println!("| --- | ---: | ---: | ---: | ---: |");
    println!(
        "| initial radius stream (cold) | {} | {:.1} | {:.3} | {:.1} |",
        initial.chunks,
        initial.elapsed_ms,
        initial.ms_per_chunk(),
        initial.chunks_per_second()
    );
    println!(
        "| fresh 9x9 area, 32 chunks away | {} | {:.1} | {:.3} | {:.1} |",
        fresh.chunks,
        fresh.elapsed_ms,
        fresh.ms_per_chunk(),
        fresh.chunks_per_second()
    );
}

/// Prints the RSS proxy table.
fn print_memory(
    phases: &[PhaseResult],
    after_open: Option<u64>,
    after_stream: Option<u64>,
    end: Option<u64>,
    host: &Host,
) {
    println!();
    println!("## Memory proxy (process VmRSS)");
    println!();
    println!("| Point | RSS | Delta |");
    println!("| --- | ---: | ---: |");
    let first = after_open;
    let delta = |value: Option<u64>| match (value, first) {
        (Some(value), Some(first)) => format!("{:+.1} MiB", (value as f64 - first as f64) / 1024.0),
        _ => "n/a".to_string(),
    };
    let label = |value: Option<u64>| value.map(measure::mib).unwrap_or_else(|| "n/a".to_string());
    println!("| after host open | {} | — |", label(after_open));
    println!(
        "| after initial streaming | {} | {} |",
        label(after_stream),
        delta(after_stream)
    );
    for phase in phases {
        println!(
            "| after {} | {} | {} |",
            phase.name,
            label(phase.rss_after_kib),
            delta(phase.rss_after_kib)
        );
    }
    println!("| after save/reload | {} | {} |", label(end), delta(end));
    let heap = host.memory_bytes() as u64;
    println!(
        "| host world heap ({} columns) | {} ({} bytes, {:.2} MB decimal) | — |",
        host.world().chunk_count(),
        measure::bytes_label(heap),
        heap,
        heap as f64 / 1_000_000.0
    );
}

/// Prints the save/reload table.
fn print_persistence(result: &PersistenceResult) {
    println!();
    println!("## Save and reload");
    println!();
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!(
        "| save time (3 runs, mean/p95/max) | {:.3} / {:.3} / {:.3} ms |",
        result.save_stats.mean_ms, result.save_stats.p95_ms, result.save_stats.max_ms
    );
    println!(
        "| total world directory | {} |",
        measure::bytes_label(result.save_bytes)
    );
    for (name, bytes) in &result.sidecars {
        println!("| {name} | {} |", measure::bytes_label(*bytes));
    }
    println!("| reload time | {:.3} ms |", result.load_ms);
    println!(
        "| reloaded tick count | {} (expected {}) |",
        result.loaded_ticks, result.expected_ticks
    );
    println!(
        "| state hash | `{:016x}` -> `{}` |",
        result.hash_before,
        result
            .hash_after
            .map(|hash| format!("{hash:016x}"))
            .unwrap_or_else(|| "reload failed".to_string())
    );
    println!("| chunks | {} -> {} |", result.chunks_before, result.chunks_after);
    println!(
        "| mobs / items / furnaces / chests | {} / {} / {} / {} -> {} / {} / {} / {} |",
        result.mobs_before,
        result.items_before,
        result.furnaces_before,
        result.chests_before,
        result.mobs_after,
        result.items_after,
        result.furnaces_after,
        result.chests_after
    );
    println!(
        "| verdict | {} |",
        if result.hash_matches() && result.load_error.is_none() {
            "PASS"
        } else {
            "FAIL"
        }
    );
}

/// Prints marginal per-phase cost over the idle baseline.
fn print_marginal(phases: &[PhaseResult], idle_mean: f64) {
    println!("## Marginal tick cost over idle");
    println!();
    println!("| Scenario | Mean delta ms | Mean ms | Percent of idle |");
    println!("| --- | ---: | ---: | ---: |");
    for phase in phases.iter().filter(|phase| phase.name != "idle") {
        let delta = phase.stats.mean_ms - idle_mean;
        let percent = if idle_mean > 0.0 {
            phase.stats.mean_ms / idle_mean * 100.0
        } else {
            0.0
        };
        println!(
            "| {} | {:+.4} | {:.4} | {:.1}% |",
            phase.name, delta, phase.stats.mean_ms, percent
        );
    }
}

/// Text report bundle written by [`build_report`].
struct Report {
    csv: String,
    markdown: String,
}

/// Builds CSV and markdown reports from the measured phases.
#[allow(clippy::too_many_arguments)]
fn build_report(
    options: &Options,
    phases: &[PhaseResult],
    initial: &StreamResult,
    fresh: &StreamResult,
    persistence: &PersistenceResult,
    rss_after_open: Option<u64>,
    rss_after_stream: Option<u64>,
    rss_end: Option<u64>,
    host: &Host,
) -> Report {
    let mut csv = String::from("scenario,ticks,mean_ms,p50_ms,p95_ms,p99_ms,max_ms,tps,population,passes_35ms\n");
    for phase in phases {
        csv.push_str(&format!(
            "{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.2},\"{}\",{}\n",
            phase.name,
            phase.ticks,
            phase.stats.mean_ms,
            phase.stats.p50_ms,
            phase.stats.p95_ms,
            phase.stats.p99_ms,
            phase.stats.max_ms,
            phase.stats.per_second(),
            phase.detail.replace('"', "'"),
            phase.passes_envelope()
        ));
    }
    csv.push_str(&format!(
        "stream_initial,{},{:.4},,,,,{:.2},cold radius stream,true\n",
        initial.chunks,
        initial.elapsed_ms,
        initial.chunks_per_second()
    ));
    csv.push_str(&format!(
        "stream_fresh,{},{:.4},,,,,{:.2},fresh 9x9 area,true\n",
        fresh.chunks,
        fresh.elapsed_ms,
        fresh.chunks_per_second()
    ));
    csv.push_str(&format!(
        "save,,{:.4},{:.4},{:.4},,,,{} bytes,true\n",
        persistence.save_stats.mean_ms,
        persistence.save_stats.p95_ms,
        persistence.save_stats.max_ms,
        persistence.save_bytes
    ));
    csv.push_str(&format!(
        "reload,,{:.4},,,,,{},,hash_match={}\n",
        persistence.load_ms,
        persistence.loaded_ticks,
        persistence.hash_matches()
    ));

    let mut markdown = String::from("# Server simulation benchmark\n\n");
    markdown.push_str(&format!(
        "- CPU: {}\n- OS: {}\n- Seed: {}\n- Radius: {} chunks\n- Ticks/phase: {}\n- Profile: {}\n",
        measure::cpu_model(),
        measure::os_description(),
        options.seed,
        options.radius,
        options.ticks,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));
    markdown.push_str("\n## Tick phases\n\n");
    markdown.push_str("| Scenario | Ticks | Mean ms | p50 | p95 | p99 | Max | TPS | p99 <= 35 ms | Population |\n");
    markdown.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | :---: | --- |\n");
    for phase in phases {
        markdown.push_str(&format!(
            "| {} | {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.1} | {} | {} |\n",
            phase.name,
            phase.ticks,
            phase.stats.mean_ms,
            phase.stats.p50_ms,
            phase.stats.p95_ms,
            phase.stats.p99_ms,
            phase.stats.max_ms,
            phase.stats.per_second(),
            if phase.passes_envelope() { "PASS" } else { "FAIL" },
            phase.detail
        ));
    }
    markdown.push_str("\n## Streaming\n\n");
    markdown.push_str("| Measurement | Chunks | Wall ms | ms/chunk | Chunks/s |\n");
    markdown.push_str("| --- | ---: | ---: | ---: | ---: |\n");
    markdown.push_str(&format!(
        "| initial radius (cold) | {} | {:.1} | {:.3} | {:.1} |\n",
        initial.chunks,
        initial.elapsed_ms,
        initial.ms_per_chunk(),
        initial.chunks_per_second()
    ));
    markdown.push_str(&format!(
        "| fresh 9x9 area | {} | {:.1} | {:.3} | {:.1} |\n",
        fresh.chunks,
        fresh.elapsed_ms,
        fresh.ms_per_chunk(),
        fresh.chunks_per_second()
    ));
    markdown.push_str("\n## Memory and persistence\n\n");
    markdown.push_str("| Metric | Value |\n| --- | ---: |\n");
    markdown.push_str(&format!(
        "| RSS after open / stream / end | {} / {} / {} |\n",
        rss_after_open.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_after_stream.map(measure::mib).unwrap_or_else(|| "n/a".to_string()),
        rss_end.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    ));
    markdown.push_str(&format!(
        "| Host world heap ({} chunks) | {} |\n",
        host.world().chunk_count(),
        measure::bytes_label(host.memory_bytes() as u64)
    ));
    markdown.push_str(&format!(
        "| Save mean / p95 / max | {:.3} / {:.3} / {:.3} ms |\n",
        persistence.save_stats.mean_ms, persistence.save_stats.p95_ms, persistence.save_stats.max_ms
    ));
    markdown.push_str(&format!(
        "| World directory | {} |\n",
        measure::bytes_label(persistence.save_bytes)
    ));
    markdown.push_str(&format!("| Reload | {:.3} ms |\n", persistence.load_ms));
    markdown.push_str(&format!(
        "| Save/reload hash | `{:016x}` -> `{}` ({}) |\n",
        persistence.hash_before,
        persistence
            .hash_after
            .map(|hash| format!("{hash:016x}"))
            .unwrap_or_else(|| "failed".to_string()),
        if persistence.hash_matches() { "PASS" } else { "FAIL" }
    ));
    Report { csv, markdown }
}

/// Deterministic 64-bit mix used by the direct mob-spawn fallback.
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

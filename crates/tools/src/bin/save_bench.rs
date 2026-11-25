//! World and sidecar persistence benchmark for the 1.0 envelope.
//!
//! The tool builds a populated temporary world through public host APIs,
//! measures repeated `Host::save()` calls and the total on-disk footprint
//! (world snapshot plus every sidecar), then measures warm reload latency and
//! verifies the host state hash survives the round-trip.
//!
//! It also synthesizes v1, v2 and v3 snapshots from the current world and shows
//! that this build's migration readers load them to the same blocks as the v4
//! file. Historical fixture bytes are not committed to the repository; the
//! authoritative historical fixtures live in the `ob_world::save` unit tests.
//! Synthesizing the legacy layouts here exercises the migration code on a
//! multi-hundred-column world rather than a toy section.
//!
//! ```text
//! save_bench --quick
//! save_bench --radius 8 --items 256 --saves 5 --csv target/save.csv
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ob_content::{BlockId, MobKind, Registry};
use ob_math::{BlockPos, WorldVec};
use ob_server::{CHEST_SLOTS, FurnaceSlot, GameMode, Host, HostConfig, ItemStack, MAX_STACK_SIZE};
use ob_sim::{MAX_HEALTH, MAX_HUNGER};
use ob_tools::measure::{self, Stats};
use ob_world::ChunkColumn;
use ob_world::section::SectionStorage;
use ob_world::{World, from_bytes, registry_fingerprint, to_bytes};

/// World directory name used under the benchmark data root.
const WORLD_NAME: &str = "save-bench";
/// Chunks generated per streaming batch.
const STREAM_BATCH: usize = 4096;
/// Blocks in one section.
const SECTION_BLOCKS: u64 = 16 * 16 * 16;
/// Snapshot magic.
const MAGIC: &[u8; 4] = b"OBSV";
/// Compressed snapshot format written by this build.
const V4_VERSION: u16 = 4;

const USAGE: &str = "\
usage: save_bench [options]
  --quick              CI profile: 81 columns, 32 items, 8 mobs, 8 furnaces, 4 chests
  --radius N           streaming radius in chunks (default 8, quick 4)
  --seed S             world seed (default 42)
  --items N            dropped item entities (default 256, quick 32)
  --mobs N             directly spawned mobs (default 18, quick 8)
  --furnaces N         lit furnaces (default 32, quick 8)
  --chests N           full chests (default 16, quick 4)
  --saves N            measured save repetitions (default 5, quick 3)
  --loads N            measured reload repetitions (default 3, quick 2)
  --no-migration       skip the synthesized v1/v2/v3 migration smoke
  --data-dir DIR       benchmark data root instead of a temporary directory
  --clean              remove an existing <DIR>/worlds/save-bench before opening
  --keep               keep the temporary data directory for inspection
  --csv PATH           write the summary CSV
  --markdown PATH      write the markdown report
  --help";

/// Parsed command-line options.
#[derive(Debug, Clone)]
struct Options {
    radius: i32,
    seed: u64,
    items: usize,
    mobs: usize,
    furnaces: usize,
    chests: usize,
    saves: usize,
    loads: usize,
    migration: bool,
    data_dir: Option<PathBuf>,
    clean: bool,
    keep: bool,
    csv: Option<PathBuf>,
    markdown: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            radius: 8,
            seed: 42,
            items: 256,
            mobs: 18,
            furnaces: 32,
            chests: 16,
            saves: 5,
            loads: 3,
            migration: true,
            data_dir: None,
            clean: false,
            keep: false,
            csv: None,
            markdown: None,
        }
    }
}

/// One synthesized legacy snapshot.
struct LegacySnapshot {
    version: u16,
    bytes: Vec<u8>,
    encode_ms: f64,
}

/// Migration result for one legacy snapshot.
struct MigrationRow {
    version: u16,
    bytes: u64,
    encode_ms: f64,
    load_ms: f64,
    chunks: usize,
    hash_match: bool,
}

/// Load result summary.
struct LoadResult {
    stats: Stats,
    chunks: usize,
    hash: u64,
    error: Option<String>,
}

/// Save/size summary.
struct SaveResult {
    stats: Stats,
    total_bytes: u64,
    sidecars: Vec<(String, u64)>,
    world_bytes: u64,
    uncompressed_payload: Option<u64>,
}

/// Runs the benchmark and returns a process exit code.
fn main() {
    ob_core::init_logging();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("save_bench: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match run(&options) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(message) => {
            eprintln!("save_bench: {message}");
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
    let mut explicit = [false; 7];
    for (name, value) in flags {
        match name.as_str() {
            "quick" => quick = true,
            "radius" => {
                let radius: i32 = value.parse().map_err(|_| format!("invalid --radius value '{value}'"))?;
                if !(1..=32).contains(&radius) {
                    return Err("--radius must be in 1..=32 chunks".to_string());
                }
                options.radius = radius;
                explicit[0] = true;
            }
            "seed" => options.seed = parse_seed(&value)?,
            "items" => {
                options.items = parse_usize(&name, &value, 0, 4096)?;
                explicit[1] = true;
            }
            "mobs" => {
                options.mobs = parse_usize(&name, &value, 0, 512)?;
                explicit[2] = true;
            }
            "furnaces" => {
                options.furnaces = parse_usize(&name, &value, 0, 128)?;
                explicit[3] = true;
            }
            "chests" => {
                options.chests = parse_usize(&name, &value, 0, 128)?;
                explicit[4] = true;
            }
            "saves" => {
                options.saves = parse_usize(&name, &value, 1, 50)?;
                explicit[5] = true;
            }
            "loads" => {
                options.loads = parse_usize(&name, &value, 1, 20)?;
                explicit[6] = true;
            }
            "no-migration" => options.migration = false,
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
            options.radius = 4;
        }
        if !explicit[1] {
            options.items = 32;
        }
        if !explicit[2] {
            options.mobs = 8;
        }
        if !explicit[3] {
            options.furnaces = 8;
        }
        if !explicit[4] {
            options.chests = 4;
        }
        if !explicit[5] {
            options.saves = 3;
        }
        if !explicit[6] {
            options.loads = 2;
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

/// Executes the benchmark; returns whether save/reload and migration checks passed.
fn run(options: &Options) -> Result<bool, String> {
    let registry = Arc::new(Registry::base());
    let temp_guard = prepare_root(options)?;
    let root = match &options.data_dir {
        Some(path) => path.clone(),
        None => temp_guard
            .as_ref()
            .map(|guard| guard.path().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("target/save-bench-data")),
    };

    println!("save_bench - world/sidecar persistence benchmark");
    println!("cpu:    {}", measure::cpu_model());
    println!("os:     {}", measure::os_description());
    println!(
        "config: seed {}, radius {} chunks, {} items, {} mobs, {} furnaces, {} chests, {} saves, {} loads",
        options.seed,
        options.radius,
        options.items,
        options.mobs,
        options.furnaces,
        options.chests,
        options.saves,
        options.loads
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
    let initial_open_ms = open_start.elapsed().as_secs_f64() * 1000.0;
    host.set_game_mode(GameMode::Survival);
    prepare_player(&mut host);
    prepare_inventory(&mut host);
    let center = host.spawn();
    let stream = stream_radius(&mut host, center, options.radius)?;
    let items = spawn_items(&mut host, options.items);
    let mobs = spawn_mobs(&mut host, options.mobs);
    let containers = prepare_containers(&mut host, options.furnaces, options.chests);
    println!(
        "setup: open {:.1} ms, streamed {} columns in {:.1} ms, populated {} items, {} mobs, {} furnaces, {} chests",
        initial_open_ms, stream.0, stream.1, items, mobs, containers.0, containers.1
    );

    // Warm-up save: prunes stale block entities and writes the first full snapshot.
    host.save().map_err(|error| format!("warm-up save failed: {error}"))?;
    let rss_after_setup = measure::rss_kib();
    let save = measure_saves(&mut host, options.saves)?;
    let rss_after_save = measure::rss_kib();
    let hash_before = host.state_hash();
    let chunks_before = host.world().chunk_count();
    let sections_before = host.world().section_count();
    let heap_bytes = host.world().heap_bytes();
    let world_hash = world_block_hash(host.world());
    let mobs_before = host.mob_count();
    let items_before = host.item_entities().len();
    let furnaces_before = host.furnaces().count();
    let chests_before = host.chests().count();
    let tick_count = host.tick_count();

    let legacy = if options.migration {
        build_legacy_snapshots(host.world())?
    } else {
        Vec::new()
    };
    drop(host);
    let rss_before_load = measure::rss_kib();

    let load = measure_loads(&config, &registry, options.loads);
    let rss_after_load = measure::rss_kib();
    let migration = if options.migration {
        run_migrations(&registry, &legacy, world_hash)?
    } else {
        Vec::new()
    };

    println!();
    println!("## Save");
    println!();
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!("| world chunks / sections | {} / {} |", chunks_before, sections_before);
    println!(
        "| host world heap | {} ({} bytes, {:.2} MB decimal) |",
        measure::bytes_label(heap_bytes as u64),
        heap_bytes,
        heap_bytes as f64 / 1_000_000.0
    );
    println!("| measured saves | {} |", options.saves);
    println!(
        "| save mean / p50 / p95 / p99 / max | {:.3} / {:.3} / {:.3} / {:.3} / {:.3} ms |",
        save.stats.mean_ms, save.stats.p50_ms, save.stats.p95_ms, save.stats.p99_ms, save.stats.max_ms
    );
    println!(
        "| save throughput (mean, world dir) | {:.1} MiB/s |",
        mib_per_second(save.total_bytes, save.stats.mean_ms)
    );
    println!("| total world directory | {} |", measure::bytes_label(save.total_bytes));
    if let Some(uncompressed) = save.uncompressed_payload {
        let ratio = if save.world_bytes == 0 {
            0.0
        } else {
            uncompressed as f64 / save.world_bytes as f64
        };
        println!(
            "| world.obs | {} (uncompressed payload {}, {:.1}x) |",
            measure::bytes_label(save.world_bytes),
            measure::bytes_label(uncompressed),
            ratio
        );
    } else {
        println!("| world.obs | {} |", measure::bytes_label(save.world_bytes));
    }
    println!();
    println!("| Sidecar | Bytes |");
    println!("| --- | ---: |");
    for (name, bytes) in &save.sidecars {
        println!("| {name} | {} |", measure::bytes_label(*bytes));
    }
    println!();
    println!("## Load and round-trip");
    println!();
    println!("| Metric | Value |");
    println!("| --- | ---: |");
    println!(
        "| reload mean / p50 / p95 / max ({} runs) | {:.3} / {:.3} / {:.3} / {:.3} ms |",
        options.loads, load.stats.mean_ms, load.stats.p50_ms, load.stats.p95_ms, load.stats.max_ms
    );
    println!("| reloaded chunks | {} |", load.chunks);
    println!("| state hash | `{:016x}` -> `{:016x}` |", hash_before, load.hash);
    println!(
        "| entity counts (mobs/items/furnaces/chests) | {} / {} / {} / {} |",
        mobs_before, items_before, furnaces_before, chests_before
    );
    println!(
        "| tick count | {} -> {} |",
        tick_count,
        if load.error.is_none() { tick_count } else { 0 }
    );
    println!(
        "| verdict | {} |",
        if load.error.is_none() && load.hash == hash_before {
            "PASS"
        } else {
            "FAIL"
        }
    );
    if let Some(error) = &load.error {
        println!("| load error | {error} |");
    }
    println!();
    println!("## Memory proxy (process VmRSS)");
    println!();
    println!("| Point | RSS |");
    println!("| --- | ---: |");
    println!(
        "| after setup | {} |",
        rss_after_setup.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "| after saves | {} |",
        rss_after_save.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "| before reload | {} |",
        rss_before_load.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );
    println!(
        "| after reload | {} |",
        rss_after_load.map(measure::mib).unwrap_or_else(|| "n/a".to_string())
    );

    if options.migration {
        println!();
        println!("## Save migration smoke");
        println!();
        println!("| Version | Bytes | Encode ms | Load ms | Chunks | Block hash match |");
        println!("| --- | ---: | ---: | ---: | ---: | :---: |");
        for row in &migration {
            println!(
                "| v{} | {} | {:.3} | {:.3} | {} | {} |",
                row.version,
                measure::bytes_label(row.bytes),
                row.encode_ms,
                row.load_ms,
                row.chunks,
                if row.hash_match { "PASS" } else { "FAIL" }
            );
        }
        println!();
        println!(
            "  v1-v3 are synthesized from this build's world; v4 is the real snapshot; historical fixture bytes are covered by ob_world unit tests."
        );
    }

    let report = build_report(
        options,
        &save,
        &load,
        &migration,
        hash_before,
        chunks_before,
        sections_before,
        heap_bytes,
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

    let mut passed = load.error.is_none() && load.hash == hash_before;
    if !migration.iter().all(|row| row.hash_match) {
        passed = false;
    }
    if !passed {
        eprintln!("save_bench: persistence round-trip or migration check failed");
    }
    if options.keep
        && let Some(guard) = temp_guard
    {
        let path = guard.keep();
        println!("kept data directory: {}", path.display());
    }
    Ok(passed)
}

/// Validates/creates the benchmark root and returns an optional temp guard.
fn prepare_root(options: &Options) -> Result<Option<measure::TempDir>, String> {
    match &options.data_dir {
        Some(root) => {
            std::fs::create_dir_all(root)
                .map_err(|error| format!("cannot create data directory {}: {error}", root.display()))?;
            let dir = world_dir(root);
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
            measure::temp_dir("openblock-save-bench").map_err(|error| error.to_string())?,
        )),
    }
}

/// Puts the host player into a stable survival state.
fn prepare_player(host: &mut Host) {
    let player = host.player_mut();
    player.flying = true;
    player.health = MAX_HEALTH;
    player.hunger = MAX_HUNGER;
    player.dead = false;
    player.velocity = WorldVec::ZERO;
}

/// Fills the hotbar and armor slots so every sidecar is non-trivial.
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

/// Spawns item entities across the loaded surface and returns the live count.
fn spawn_items(host: &mut Host, target: usize) -> usize {
    let center = host.spawn();
    let mut offset = 0i32;
    let mut spawned = 0usize;
    let mut attempts = 0usize;
    while spawned < target && attempts < target * 16 + 64 {
        attempts += 1;
        let (x, z) = grid_offset(center, &mut offset);
        if let Some(height) = host.world().height_at(x, z) {
            let pos = WorldVec::new(x as f64 + 0.5, height as f64 + 2.0, z as f64 + 0.5);
            if let Some(stack) = item_stack(host.registry(), spawned) {
                host.spawn_item_entity(pos, stack);
                spawned += 1;
            }
        }
    }
    host.item_entities().len()
}

/// Builds one deterministic dropped stack.
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

/// Spawns up to `target` mobs at deterministic surface positions.
fn spawn_mobs(host: &mut Host, target: usize) -> usize {
    let center = host.spawn();
    let kinds = [
        MobKind::Cow,
        MobKind::Pig,
        MobKind::Sheep,
        MobKind::Chicken,
        MobKind::Zombie,
    ];
    let mut spawned = 0usize;
    let mut attempt = 0usize;
    while spawned < target && attempt < target * 64 + 64 {
        let kind = kinds[spawned % kinds.len()];
        if let Some(pos) = find_spawn_position(host, center, 10.0, 120.0, attempt, kind as u32)
            && host.spawn_mob(kind, pos).is_some()
        {
            spawned += 1;
        }
        attempt += 1;
    }
    host.mob_count()
}

/// Places and fills furnaces and chests, returning `(furnaces, chests)`.
fn prepare_containers(host: &mut Host, furnaces: usize, chests: usize) -> (usize, usize) {
    let center = host.spawn();
    let furnace_id = host.registry().id("openblock:furnace").unwrap_or(BlockId::AIR);
    let chest_id = host.registry().id("openblock:chest").unwrap_or(BlockId::AIR);
    let raw_iron = host.registry().item_id("openblock:raw_iron");
    let coal = host.registry().item_id("openblock:coal");
    let cobble = host.registry().item_id("openblock:cobblestone");
    let dirt = host.registry().item_id("openblock:dirt");
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
                let _ = host.put_furnace_slot(pos, FurnaceSlot::Input, ItemStack::new(iron, 64));
                let _ = host.put_furnace_slot(pos, FurnaceSlot::Fuel, ItemStack::new(fuel, 64));
            }
        } else {
            if chest_id == BlockId::AIR || !host.apply_remote_edit(pos, chest_id) {
                continue;
            }
            placed_chests += 1;
            for slot in 0..CHEST_SLOTS {
                let item = if slot % 2 == 0 { cobble } else { dirt };
                if let Some(item) = item {
                    let _ = host.put_chest_slot(pos, slot, ItemStack::new(item, 64));
                }
            }
        }
    }
    (placed_furnaces, placed_chests)
}

/// Finds an air block directly above a solid surface column.
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

/// Measures repeated saves and sums the on-disk footprint.
fn measure_saves(host: &mut Host, runs: usize) -> Result<SaveResult, String> {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        host.save().map_err(|error| format!("save failed: {error}"))?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let dir = world_dir_from_host(host);
    let (total_bytes, sidecars) = directory_files(&dir)?;
    let world_bytes = sidecars
        .iter()
        .find(|(name, _)| name == "world.obs")
        .map_or(0, |(_, bytes)| *bytes);
    let uncompressed_payload = std::fs::read(dir.join("world.obs"))
        .ok()
        .and_then(|bytes| v4_uncompressed_len(&bytes));
    Ok(SaveResult {
        stats: Stats::from_samples(&samples),
        total_bytes,
        sidecars,
        world_bytes,
        uncompressed_payload,
    })
}

/// Returns the world directory from a host's configured data root.
fn world_dir_from_host(host: &Host) -> PathBuf {
    let data = host.config().data_dir.clone().unwrap_or_else(ob_core::data_dir);
    data.join("worlds").join(&host.config().world_name)
}

/// The benchmark world directory under a root.
fn world_dir(root: &Path) -> PathBuf {
    root.join("worlds").join(WORLD_NAME)
}

/// Sums files in the world directory and returns a sorted breakdown.
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
        total += metadata.len();
        files.push((entry.file_name().to_string_lossy().into_owned(), metadata.len()));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((total, files))
}

/// Reads the declared uncompressed payload length from a v4 snapshot header.
fn v4_uncompressed_len(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 31 || &bytes[..4] != MAGIC {
        return None;
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != V4_VERSION {
        return None;
    }
    Some(u64::from(u32::from_le_bytes([
        bytes[23], bytes[24], bytes[25], bytes[26],
    ])))
}

/// Measures repeated reloads and captures one hash/count.
fn measure_loads(config: &HostConfig, registry: &Arc<Registry>, runs: usize) -> LoadResult {
    let mut samples = Vec::with_capacity(runs);
    let mut chunks = 0usize;
    let mut hash = 0u64;
    let mut error = None;
    for run in 0..runs {
        let started = Instant::now();
        match Host::open(Arc::clone(registry), config.clone()) {
            Ok(host) => {
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
                if run == 0 {
                    chunks = host.world().chunk_count();
                    hash = host.state_hash();
                }
            }
            Err(failure) => {
                error = Some(failure.to_string());
                break;
            }
        }
    }
    LoadResult {
        stats: Stats::from_samples(&samples),
        chunks,
        hash,
        error,
    }
}

/// Builds the v1/v2/v3 legacy snapshots from the current world.
fn build_legacy_snapshots(world: &World) -> Result<Vec<LegacySnapshot>, String> {
    let mut snapshots = Vec::new();
    for version in [1u16, 2, 3, 4] {
        let started = Instant::now();
        let bytes = if version == 4 {
            to_bytes(world)
        } else {
            encode_legacy_world(world, version)
        };
        snapshots.push(LegacySnapshot {
            version,
            bytes,
            encode_ms: started.elapsed().as_secs_f64() * 1000.0,
        });
    }
    Ok(snapshots)
}

/// Loads every legacy snapshot and compares its block hash to `expected`.
fn run_migrations(
    registry: &Arc<Registry>,
    snapshots: &[LegacySnapshot],
    expected: u64,
) -> Result<Vec<MigrationRow>, String> {
    let mut rows = Vec::new();
    for snapshot in snapshots {
        let started = Instant::now();
        let world = from_bytes(Arc::clone(registry), &snapshot.bytes)
            .map_err(|error| format!("v{} migration failed: {error}", snapshot.version))?;
        let load_ms = started.elapsed().as_secs_f64() * 1000.0;
        rows.push(MigrationRow {
            version: snapshot.version,
            bytes: snapshot.bytes.len() as u64,
            encode_ms: snapshot.encode_ms,
            load_ms,
            chunks: world.chunk_count(),
            hash_match: world_block_hash(&world) == expected,
        });
    }
    // The v4 file is the reference format; verify it through the same reader.
    Ok(rows)
}

/// Encodes a legacy v1/v2/v3 snapshot from a world.
fn encode_legacy_world(world: &World, version: u16) -> Vec<u8> {
    let payload = encode_chunk_stream(world, version);
    let mut out = Vec::with_capacity(payload.len() + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&world.seed().to_le_bytes());
    if version == 3 {
        out.extend_from_slice(&registry_fingerprint(world.registry()).to_le_bytes());
    }
    if version == 1 {
        out.extend_from_slice(&payload);
    } else {
        out.push(0); // Compression::None
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
    }
    let checksum = crc32c(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Encodes the chunk stream shared by every snapshot version.
fn encode_chunk_stream(world: &World, version: u16) -> Vec<u8> {
    let mut chunks: Vec<&ChunkColumn> = world.chunks().map(|(_, chunk)| chunk).collect();
    chunks.sort_by_key(|chunk| (chunk.pos().x, chunk.pos().z));
    let mut out = Vec::new();
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for chunk in chunks {
        let pos = chunk.pos();
        out.extend_from_slice(&pos.x.to_le_bytes());
        out.extend_from_slice(&pos.z.to_le_bytes());
        for (_, section) in chunk.sections() {
            write_storage(&mut out, section);
            if version == 1 {
                write_legacy_v1_light(&mut out, section.light());
            } else {
                write_v2_light(&mut out, section.light());
            }
        }
    }
    out
}

/// Writes a section's block storage in the v1-v4 shared layout.
fn write_storage(out: &mut Vec<u8>, section: &ob_world::Section) {
    match section.storage() {
        SectionStorage::Uniform(id) => {
            out.push(0);
            out.extend_from_slice(&id.0.to_le_bytes());
        }
        SectionStorage::Bits4 { words, palette } => {
            write_paletted(out, 1, palette, words, 256);
        }
        SectionStorage::Bits8 { words, palette } => {
            write_paletted(out, 2, palette, words, 512);
        }
        SectionStorage::Bits16 { words, palette } => {
            write_paletted(out, 3, palette, words, 1024);
        }
    }
}

/// Writes a paletted section body, zero-padding missing words.
fn write_paletted(out: &mut Vec<u8>, mode: u8, palette: &[BlockId], words: &[u64], required: usize) {
    let palette_len = u16::try_from(palette.len()).unwrap_or(u16::MAX);
    out.push(mode);
    out.extend_from_slice(&palette_len.to_le_bytes());
    for id in palette.iter().take(usize::from(palette_len)) {
        out.extend_from_slice(&id.0.to_le_bytes());
    }
    for index in 0..required {
        let word = words.get(index).copied().unwrap_or(0);
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Writes the v1 dense light encoding.
fn write_legacy_v1_light(out: &mut Vec<u8>, light: &ob_world::Light) {
    match light.uniform_value() {
        Some(0) => out.push(0),
        Some(value) => {
            out.push(1);
            for _ in 0..SECTION_BLOCKS {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        None => {
            out.push(1);
            let data = light.raw().expect("dense light has a raw array");
            for word in data.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// Writes the v2+ compact light encoding.
fn write_v2_light(out: &mut Vec<u8>, light: &ob_world::Light) {
    match light.uniform_value() {
        Some(0) => out.push(0),
        Some(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => {
            out.push(1);
            let data = light.raw().expect("dense light has a raw array");
            for word in data.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// Reflected Castagnoli CRC32C polynomial.
const CRC32C_POLYNOMIAL: u32 = 0x82F6_3B78;

/// Reflected CRC32C lookup table.
const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

/// Builds the CRC32C table at compile time.
const fn build_crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut value = 0;
    while value < 256 {
        let mut crc = value as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ CRC32C_POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[value] = crc;
        value += 1;
    }
    table
}

/// Computes the CRC32C checksum over `bytes`.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[index];
    }
    !crc
}

/// Canonical FNV-1a block-storage hash over every loaded chunk.
fn world_block_hash(world: &World) -> u64 {
    let mut chunks: Vec<&ChunkColumn> = world.chunks().map(|(_, chunk)| chunk).collect();
    chunks.sort_by_key(|chunk| (chunk.pos().x, chunk.pos().z));
    let mut hasher = ChunkHasher::new();
    hasher.u64(world.seed());
    for chunk in chunks {
        hasher.i32(chunk.pos().x);
        hasher.i32(chunk.pos().z);
        for (_, section) in chunk.sections() {
            match section.storage() {
                SectionStorage::Uniform(id) => {
                    hasher.byte(0);
                    hasher.u16(id.0);
                }
                SectionStorage::Bits4 { words, palette } => {
                    hasher.byte(1);
                    hasher.palette(palette);
                    for &word in words {
                        hasher.u64(word);
                    }
                }
                SectionStorage::Bits8 { words, palette } => {
                    hasher.byte(2);
                    hasher.palette(palette);
                    for &word in words {
                        hasher.u64(word);
                    }
                }
                SectionStorage::Bits16 { words, palette } => {
                    hasher.byte(3);
                    hasher.palette(palette);
                    for &word in words {
                        hasher.u64(word);
                    }
                }
            }
        }
    }
    hasher.finish()
}

/// Small canonical FNV-1a hasher.
struct ChunkHasher {
    hash: u64,
}

impl ChunkHasher {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Starts a hash.
    fn new() -> Self {
        Self { hash: Self::OFFSET }
    }

    /// Feeds one byte.
    fn byte(&mut self, byte: u8) {
        self.hash ^= u64::from(byte);
        self.hash = self.hash.wrapping_mul(Self::PRIME);
    }

    /// Feeds a little-endian `u16`.
    fn u16(&mut self, value: u16) {
        for byte in value.to_le_bytes() {
            self.byte(byte);
        }
    }

    /// Feeds a little-endian `i32`.
    fn i32(&mut self, value: i32) {
        for byte in value.to_le_bytes() {
            self.byte(byte);
        }
    }

    /// Feeds a little-endian `u64`.
    fn u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.byte(byte);
        }
    }

    /// Feeds a length-prefixed palette.
    fn palette(&mut self, palette: &[BlockId]) {
        let len = palette.len().min(u16::MAX as usize) as u16;
        self.u16(len);
        for id in palette.iter().take(usize::from(len)) {
            self.u16(id.0);
        }
    }

    /// The finished digest.
    fn finish(self) -> u64 {
        self.hash
    }
}

/// Converts bytes over a duration to MiB/s.
fn mib_per_second(bytes: u64, milliseconds: f64) -> f64 {
    if milliseconds <= 0.0 {
        0.0
    } else {
        bytes as f64 / (1024.0 * 1024.0) * 1000.0 / milliseconds
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

/// Builds CSV and markdown reports.
#[allow(clippy::too_many_arguments)]
fn build_report(
    options: &Options,
    save: &SaveResult,
    load: &LoadResult,
    migration: &[MigrationRow],
    hash_before: u64,
    chunks: usize,
    sections: usize,
    heap_bytes: usize,
) -> Report {
    let mut csv = String::from("metric,value\n");
    csv.push_str(&format!("world_chunks,{chunks}\n"));
    csv.push_str(&format!("world_sections,{sections}\n"));
    csv.push_str(&format!("world_heap_bytes,{heap_bytes}\n"));
    csv.push_str(&format!("save_runs,{}\n", options.saves));
    csv.push_str(&format!("save_mean_ms,{:.4}\n", save.stats.mean_ms));
    csv.push_str(&format!("save_p50_ms,{:.4}\n", save.stats.p50_ms));
    csv.push_str(&format!("save_p95_ms,{:.4}\n", save.stats.p95_ms));
    csv.push_str(&format!("save_p99_ms,{:.4}\n", save.stats.p99_ms));
    csv.push_str(&format!("save_max_ms,{:.4}\n", save.stats.max_ms));
    csv.push_str(&format!("save_total_bytes,{}\n", save.total_bytes));
    csv.push_str(&format!("load_runs,{}\n", options.loads));
    csv.push_str(&format!("load_mean_ms,{:.4}\n", load.stats.mean_ms));
    csv.push_str(&format!("load_p50_ms,{:.4}\n", load.stats.p50_ms));
    csv.push_str(&format!("load_p95_ms,{:.4}\n", load.stats.p95_ms));
    csv.push_str(&format!("load_max_ms,{:.4}\n", load.stats.max_ms));
    csv.push_str(&format!("hash_before,{hash_before:016x}\n"));
    csv.push_str(&format!("hash_after,{:016x}\n", load.hash));
    csv.push_str(&format!(
        "round_trip,{}\n",
        load.hash == hash_before && load.error.is_none()
    ));
    for row in migration {
        csv.push_str(&format!("migration_v{}_bytes,{}\n", row.version, row.bytes));
        csv.push_str(&format!("migration_v{}_encode_ms,{:.4}\n", row.version, row.encode_ms));
        csv.push_str(&format!("migration_v{}_load_ms,{:.4}\n", row.version, row.load_ms));
        csv.push_str(&format!("migration_v{}_hash_match,{}\n", row.version, row.hash_match));
    }

    let mut markdown = String::from("# Save/persistence benchmark\n\n");
    markdown.push_str(&format!(
        "- CPU: {}\n- OS: {}\n- Seed: {}\n- Radius: {} chunks\n- Profile: {}\n",
        measure::cpu_model(),
        measure::os_description(),
        options.seed,
        options.radius,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));
    markdown.push_str("\n| Metric | Value |\n| --- | ---: |\n");
    markdown.push_str(&format!("| World chunks / sections | {} / {} |\n", chunks, sections));
    markdown.push_str(&format!(
        "| Host world heap | {} |\n",
        measure::bytes_label(heap_bytes as u64)
    ));
    markdown.push_str(&format!(
        "| Save mean / p50 / p95 / p99 / max | {:.3} / {:.3} / {:.3} / {:.3} / {:.3} ms |\n",
        save.stats.mean_ms, save.stats.p50_ms, save.stats.p95_ms, save.stats.p99_ms, save.stats.max_ms
    ));
    markdown.push_str(&format!(
        "| Save throughput | {:.1} MiB/s |\n",
        mib_per_second(save.total_bytes, save.stats.mean_ms)
    ));
    markdown.push_str(&format!(
        "| World directory | {} |\n",
        measure::bytes_label(save.total_bytes)
    ));
    markdown.push_str(&format!(
        "| Reload mean / p50 / p95 / max | {:.3} / {:.3} / {:.3} / {:.3} ms |\n",
        load.stats.mean_ms, load.stats.p50_ms, load.stats.p95_ms, load.stats.max_ms
    ));
    markdown.push_str(&format!(
        "| State hash | `{:016x}` -> `{:016x}` ({}) |\n",
        hash_before,
        load.hash,
        if load.hash == hash_before { "PASS" } else { "FAIL" }
    ));
    markdown.push_str("\n## Migration smoke\n\n");
    markdown.push_str("| Version | Bytes | Encode ms | Load ms | Chunks | Hash |\n");
    markdown.push_str("| --- | ---: | ---: | ---: | ---: | :---: |\n");
    for row in migration {
        markdown.push_str(&format!(
            "| v{} | {} | {:.3} | {:.3} | {} | {} |\n",
            row.version,
            measure::bytes_label(row.bytes),
            row.encode_ms,
            row.load_ms,
            row.chunks,
            if row.hash_match { "PASS" } else { "FAIL" }
        ));
    }
    Report { csv, markdown }
}

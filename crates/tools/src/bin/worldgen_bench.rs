//! Deterministic world-generation throughput and content benchmark.
//!
//! The binary generates a square of chunks around the origin for three fixed
//! seeds, measures each `Generator::generate_chunk` call individually, and
//! reports nearest-rank p50/p95/p99/max latency, chunks per second, block
//! counts and cave/ore densities. It also regenerates a fixed subset of
//! chunks and compares a canonical FNV-1a block hash, so a throughput run also
//! proves the generator is still deterministic.
//!
//! The generation call is deliberately serial: `Host::ensure_streaming` runs
//! chunks on the rayon pool and is measured by `server_bench`; this tool
//! isolates the per-chunk CPU cost of the terrain/cave/ore pipeline itself.
//!
//! ```text
//! worldgen_bench --quick                 # 6x6 chunks x 3 seeds, CI smoke
//! worldgen_bench                         # 12x12 chunks x 3 seeds
//! worldgen_bench --full                  # 40x40 chunks x 3 seeds
//! worldgen_bench --csv target/gen.csv --markdown target/gen.md
//! ```

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use ob_content::{BlockId, Registry};
use ob_math::ChunkPos;
use ob_tools::measure::{self, Stats};
use ob_world::ChunkColumn;
use ob_world::section::SectionStorage;
use ob_worldgen::Generator;

/// Default square side length in chunks.
const DEFAULT_SIZE: i32 = 12;
/// Square side length used by `--quick`.
const QUICK_SIZE: i32 = 6;
/// Square side length used by `--full`.
const FULL_SIZE: i32 = 40;
/// Blocks in one 16^3 section.
const SECTION_BLOCKS: u64 = 16 * 16 * 16;

const USAGE: &str = "\
usage: worldgen_bench [options]
  --quick               6x6 chunks x 3 seeds (CI smoke)
  --full                40x40 chunks x 3 seeds
  --size N              square side length in chunks (default 12)
  --seeds A,B,...       comma-separated world seeds (default 1,42,0xc0ffee0012345678)
  --no-cave-stats       skip the diagnostic cave/ravine re-generation
  --csv PATH            write a per-seed summary CSV
  --markdown PATH       write a markdown summary table
  --max-mean-ms MS      fail when the aggregate mean exceeds MS
  --max-p95-ms MS       fail when the aggregate p95 exceeds MS
  --help
environment: OPENBLOCK_WORLDGEN_CAVE_STATS=0 disables cave stats too";

/// Parsed command-line options.
#[derive(Debug, Clone)]
struct Options {
    size: i32,
    seeds: Vec<u64>,
    cave_stats: bool,
    csv: Option<PathBuf>,
    markdown: Option<PathBuf>,
    max_mean_ms: Option<f64>,
    max_p95_ms: Option<f64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            size: DEFAULT_SIZE,
            seeds: vec![1, 42, 0xc0ff_ee00_1234_5678],
            cave_stats: true,
            csv: None,
            markdown: None,
            max_mean_ms: None,
            max_p95_ms: None,
        }
    }
}

/// Block ids counted by name for the content sanity table.
#[derive(Debug, Clone, Copy)]
struct TrackedBlocks {
    stone: BlockId,
    dirt: BlockId,
    grass: BlockId,
    water: BlockId,
    log: BlockId,
    leaves: BlockId,
    coal: BlockId,
    iron: BlockId,
    copper: BlockId,
}

impl TrackedBlocks {
    fn from_registry(registry: &Registry) -> Self {
        let lookup = |name: &str| registry.id(name).unwrap_or(BlockId::AIR);
        Self {
            stone: lookup("openblock:stone"),
            dirt: lookup("openblock:dirt"),
            grass: lookup("openblock:grass"),
            water: lookup("openblock:water"),
            log: lookup("openblock:log"),
            leaves: lookup("openblock:leaves"),
            coal: lookup("openblock:coal_ore"),
            iron: lookup("openblock:iron_ore"),
            copper: lookup("openblock:copper_ore"),
        }
    }
}

/// One seed's accumulated measurements.
#[derive(Debug, Clone)]
struct SeedRun {
    seed: u64,
    chunks: u64,
    samples_ms: Vec<f64>,
    total_blocks: u64,
    non_air: u64,
    counts: Vec<u64>,
    carved_blocks: u64,
    entrance_columns: u64,
    ore_blocks: u64,
    cave_stats_sampled: bool,
    wall_ms: f64,
}

/// Runs the benchmark and returns a process exit code.
fn main() {
    ob_core::init_logging();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("worldgen_bench: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(&options) {
        eprintln!("worldgen_bench: {error}");
        std::process::exit(1);
    }
}

/// Parses command-line flags into [`Options`].
fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let (flags, free) = ob_core::parse_args(args);
    if !free.is_empty() {
        return Err(format!("unexpected argument '{}'", free[0]));
    }
    let mut options = Options::default();
    let mut size_flag = false;
    let mut quick = false;
    let mut full = false;
    for (name, value) in flags {
        match name.as_str() {
            "quick" => quick = true,
            "full" => full = true,
            "size" => {
                let parsed = value
                    .parse::<i32>()
                    .map_err(|_| format!("invalid --size value '{value}'"))?;
                if !(1..=64).contains(&parsed) {
                    return Err("--size must be in 1..=64".to_string());
                }
                options.size = parsed;
                size_flag = true;
            }
            "seeds" => options.seeds = parse_seeds(&value)?,
            "seed" => options.seeds = vec![parse_seed(&value)?],
            "no-cave-stats" => options.cave_stats = false,
            "csv" => options.csv = Some(PathBuf::from(non_empty(&name, &value)?)),
            "markdown" => options.markdown = Some(PathBuf::from(non_empty(&name, &value)?)),
            "max-mean-ms" => options.max_mean_ms = Some(parse_ms(&name, &value)?),
            "max-p95-ms" => options.max_p95_ms = Some(parse_ms(&name, &value)?),
            "help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option --{other}")),
        }
    }
    if quick && full {
        return Err("--quick and --full are mutually exclusive".to_string());
    }
    if quick && !size_flag {
        options.size = QUICK_SIZE;
    }
    if full && !size_flag {
        options.size = FULL_SIZE;
    }
    if std::env::var("OPENBLOCK_WORLDGEN_CAVE_STATS").is_ok_and(|value| value == "0") {
        options.cave_stats = false;
    }
    Ok(options)
}

/// Reads a required non-empty flag value.
fn non_empty(name: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("--{name} requires a value"))
    } else {
        Ok(value.to_string())
    }
}

/// Parses a non-negative finite millisecond threshold.
fn parse_ms(name: &str, value: &str) -> Result<f64, String> {
    let parsed: f64 = value.parse().map_err(|_| format!("invalid --{name} value '{value}'"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!("--{name} must be a finite non-negative number"));
    }
    Ok(parsed)
}

/// Parses one seed, accepting decimal or `0x` hexadecimal.
fn parse_seed(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let parsed = if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        trimmed.parse()
    };
    parsed.map_err(|_| format!("invalid seed '{value}'"))
}

/// Parses a comma-separated seed list, de-duplicating while preserving order.
fn parse_seeds(value: &str) -> Result<Vec<u64>, String> {
    let mut seeds = Vec::new();
    for part in value.split(',') {
        let seed = parse_seed(part)?;
        if !seeds.contains(&seed) {
            seeds.push(seed);
        }
    }
    if seeds.is_empty() {
        return Err("--seeds needs at least one seed".to_string());
    }
    Ok(seeds)
}

/// Executes the benchmark.
fn run(options: &Options) -> Result<(), String> {
    let registry = Registry::base();
    let tracked = TrackedBlocks::from_registry(&registry);
    let start_coord = -(options.size / 2);
    let end_coord = start_coord + options.size;
    let chunks_per_seed = (options.size as u64) * (options.size as u64);
    let positions: Vec<ChunkPos> = (start_coord..end_coord)
        .flat_map(|z| (start_coord..end_coord).map(move |x| ChunkPos::new(x, z)))
        .collect();

    println!("worldgen_bench - deterministic chunk generation");
    println!("cpu:  {}", measure::cpu_model());
    println!("os:   {}", measure::os_description());
    println!(
        "grid: {}x{} chunks x {} seeds = {} chunks; cave stats {}",
        options.size,
        options.size,
        options.seeds.len(),
        chunks_per_seed * options.seeds.len() as u64,
        if options.cave_stats { "on" } else { "off" }
    );
    println!();

    let wall_start = Instant::now();
    let mut runs = Vec::with_capacity(options.seeds.len());
    let mut overall_samples = Vec::new();
    for &seed in &options.seeds {
        let generator = Generator::new(seed);
        let mut samples = Vec::with_capacity(positions.len());
        let mut counts = vec![0u64; registry.len().max(1) + 1];
        let mut hashes = BTreeMap::new();
        let mut carved_blocks = 0u64;
        let mut entrance_columns = 0u64;
        let mut total_blocks = 0u64;
        let seed_start = Instant::now();

        for &pos in &positions {
            let started = Instant::now();
            let chunk = generator.generate_chunk(pos, &registry);
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            total_blocks += SECTION_BLOCKS * chunk.sections().count() as u64;
            count_chunk(&chunk, &mut counts);
            if is_determinism_sample(pos, start_coord, end_coord) {
                hashes.insert(pos, chunk_hash(&chunk));
            }
            if options.cave_stats {
                let stats = generator.cave_debug_stats(pos, &registry);
                carved_blocks += u64::from(stats.cave_blocks) + u64::from(stats.ravine_blocks);
                entrance_columns += u64::from(stats.entrance_columns);
            }
        }

        let non_air = total_blocks.saturating_sub(counts[0]);
        let ore_blocks = tracked.ore_count(&counts);
        let mut seed_run = SeedRun {
            seed,
            chunks: chunks_per_seed,
            samples_ms: samples,
            total_blocks,
            non_air,
            counts,
            carved_blocks,
            entrance_columns,
            ore_blocks,
            cave_stats_sampled: options.cave_stats,
            wall_ms: seed_start.elapsed().as_secs_f64() * 1000.0,
        };

        // Determinism: regenerate the fixed subset and compare canonical hashes.
        let mut matches = 0usize;
        let mut mismatches = Vec::new();
        for (&pos, &expected) in &hashes {
            let actual = chunk_hash(&generator.generate_chunk(pos, &registry));
            if actual == expected {
                matches += 1;
            } else {
                mismatches.push(format!("({}, {}) {expected:016x} != {actual:016x}", pos.x, pos.z));
            }
        }
        seed_run.wall_ms = seed_start.elapsed().as_secs_f64() * 1000.0;

        let stats = Stats::from_samples(&seed_run.samples_ms);
        overall_samples.extend_from_slice(&seed_run.samples_ms);
        print_seed(&seed_run, &stats, &tracked, matches, &mismatches);
        runs.push(seed_run);
        if !mismatches.is_empty() {
            return Err(format!(
                "seed {seed} is not deterministic ({} mismatched sample chunks)",
                mismatches.len()
            ));
        }
    }

    let aggregate = Stats::from_samples(&overall_samples);
    println!();
    println!(
        "aggregate: {} chunks, mean {:.3} ms/chunk, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}, {:.1} chunks/s",
        aggregate.count,
        aggregate.mean_ms,
        aggregate.p50_ms,
        aggregate.p95_ms,
        aggregate.p99_ms,
        aggregate.max_ms,
        aggregate.per_second()
    );
    println!("wall: {:.1} ms", wall_start.elapsed().as_secs_f64() * 1000.0);

    // Different seeds must not silently share a hash at the origin.
    let origin_hashes: Vec<(u64, u64)> = runs
        .iter()
        .map(|run| {
            let generator = Generator::new(run.seed);
            let chunk = generator.generate_chunk(ChunkPos::new(0, 0), &registry);
            (run.seed, chunk_hash(&chunk))
        })
        .collect();
    let mut distinct: Vec<u64> = origin_hashes.iter().map(|(_, hash)| *hash).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() != origin_hashes.len() {
        return Err("two seeds produced the same origin chunk hash".to_string());
    }
    println!("determinism: sampled chunks and cross-seed origin hashes match");

    if let Some(path) = &options.csv {
        let document = csv_document(&runs);
        measure::write_text(path, &document).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("csv: {}", path.display());
    }
    if let Some(path) = &options.markdown {
        let document = markdown_document(&runs, &tracked);
        measure::write_text(path, &document).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("markdown: {}", path.display());
    }

    let mut failed = false;
    if let Some(limit) = options.max_mean_ms {
        let pass = aggregate.mean_ms <= limit;
        println!(
            "mean gate: {:.3} ms/chunk vs {limit:.3} ms -> {}",
            aggregate.mean_ms,
            if pass { "PASS" } else { "FAIL" }
        );
        failed |= !pass;
    }
    if let Some(limit) = options.max_p95_ms {
        let pass = aggregate.p95_ms <= limit;
        println!(
            "p95 gate: {:.3} ms/chunk vs {limit:.3} ms -> {}",
            aggregate.p95_ms,
            if pass { "PASS" } else { "FAIL" }
        );
        failed |= !pass;
    }
    if failed {
        return Err("one or more requested timing gates failed".to_string());
    }
    Ok(())
}

/// Prints the per-seed table row and content sanity metrics.
fn print_seed(run: &SeedRun, stats: &Stats, tracked: &TrackedBlocks, matches: usize, mismatches: &[String]) {
    let carve_pct = if run.non_air + run.carved_blocks == 0 {
        0.0
    } else {
        run.carved_blocks as f64 * 100.0 / (run.non_air + run.carved_blocks) as f64
    };
    println!("seed {} (0x{:x})", run.seed, run.seed);
    println!(
        "  time:   mean {:.3} ms/chunk, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}, {:.1} chunks/s (gen {:.1} ms, wall {:.1} ms)",
        stats.mean_ms,
        stats.p50_ms,
        stats.p95_ms,
        stats.p99_ms,
        stats.max_ms,
        stats.per_second(),
        stats.total_ms,
        run.wall_ms
    );
    println!(
        "  blocks: {} total, {} non-air, {} air ({:.1}%)",
        run.total_blocks,
        run.non_air,
        run.total_blocks - run.non_air,
        (run.total_blocks - run.non_air) as f64 * 100.0 / run.total_blocks.max(1) as f64
    );
    println!(
        "  content: stone {}, dirt {}, grass {}, water {}, log {}, leaves {}",
        tracked.count(&run.counts, tracked.stone),
        tracked.count(&run.counts, tracked.dirt),
        tracked.count(&run.counts, tracked.grass),
        tracked.count(&run.counts, tracked.water),
        tracked.count(&run.counts, tracked.log),
        tracked.count(&run.counts, tracked.leaves)
    );
    println!(
        "  ores:    coal {}, iron {}, copper {} ({} total, {:.2} per 1,000 non-air blocks)",
        tracked.count(&run.counts, tracked.coal),
        tracked.count(&run.counts, tracked.iron),
        tracked.count(&run.counts, tracked.copper),
        run.ore_blocks,
        run.ore_blocks as f64 * 1000.0 / run.non_air.max(1) as f64
    );
    if run.cave_stats_sampled {
        println!(
            "  caves:   {} carved blocks ({:.2}% of pre-carve solids), {} entrance columns, {:.1} carved/chunk",
            run.carved_blocks,
            carve_pct,
            run.entrance_columns,
            run.carved_blocks as f64 / run.chunks.max(1) as f64
        );
    } else {
        println!("  caves:   not sampled (--no-cave-stats)");
    }
    println!(
        "  determinism: {matches}/{} sampled chunks matched{}",
        matches,
        if mismatches.is_empty() {
            String::new()
        } else {
            format!(" (first mismatch: {})", mismatches[0])
        }
    );
}

impl TrackedBlocks {
    /// Count of one block id in a count vector.
    fn count(&self, counts: &[u64], id: BlockId) -> u64 {
        counts.get(id.0 as usize).copied().unwrap_or(0)
    }

    /// Total coal, iron and copper block count.
    fn ore_count(&self, counts: &[u64]) -> u64 {
        self.count(counts, self.coal) + self.count(counts, self.iron) + self.count(counts, self.copper)
    }
}

/// Whether a chunk belongs to the fixed determinism sample set.
fn is_determinism_sample(pos: ChunkPos, start: i32, end: i32) -> bool {
    let last = end - 1;
    let spawn = pos.x.abs() <= 2 && pos.z.abs() <= 2;
    let edge = pos.x == start || pos.z == start || pos.x == last || pos.z == last;
    let origin = pos.x == 0 && pos.z == 0;
    spawn || edge || origin
}

/// Adds one chunk's per-block counts, reading palette words instead of every block.
fn count_chunk(chunk: &ChunkColumn, counts: &mut [u64]) {
    for (_, section) in chunk.sections() {
        match section.storage() {
            SectionStorage::Uniform(id) => add_count(counts, *id, SECTION_BLOCKS),
            SectionStorage::Bits4 { words, palette } => {
                count_paletted(counts, words, palette, 4, 16);
            }
            SectionStorage::Bits8 { words, palette } => {
                count_paletted(counts, words, palette, 8, 8);
            }
            SectionStorage::Bits16 { words, palette } => {
                count_paletted(counts, words, palette, 16, 4);
            }
        }
    }
}

/// Counts palette indices for one packed storage mode.
fn count_paletted(counts: &mut [u64], words: &[u64], palette: &[BlockId], bits: u8, per_word: u32) {
    let mask = (1u64 << bits) - 1;
    for &word in words {
        for slot in 0..per_word {
            let palette_index = ((word >> (slot * u32::from(bits))) & mask) as usize;
            if let Some(id) = palette.get(palette_index) {
                add_count(counts, *id, 1);
            }
        }
    }
}

/// Adds `amount` to one block id's slot, ignoring ids outside the session registry.
fn add_count(counts: &mut [u64], id: BlockId, amount: u64) {
    if let Some(slot) = counts.get_mut(id.0 as usize) {
        *slot += amount;
    }
}

/// FNV-1a hash of a chunk's canonical block storage (mode, palette and words).
fn chunk_hash(chunk: &ChunkColumn) -> u64 {
    let mut hasher = ChunkHasher::new();
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
    hasher.finish()
}

/// Tiny canonical FNV-1a hasher used by the determinism sample check.
struct ChunkHasher {
    hash: u64,
}

impl ChunkHasher {
    /// FNV-1a offset basis.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    /// FNV-1a 64-bit prime.
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Starts a canonical chunk hash.
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

/// Builds the summary CSV document.
fn csv_document(runs: &[SeedRun]) -> String {
    let mut out = String::from(
        "seed,chunks,mean_ms,p50_ms,p95_ms,p99_ms,max_ms,total_ms,chunks_per_s,total_blocks,non_air,carved_blocks,entrance_columns,carve_pct,coal,iron,copper,ore_blocks,ore_per_1000_non_air,deterministic\n",
    );
    for run in runs {
        let stats = Stats::from_samples(&run.samples_ms);
        let tracked = TrackedBlocks::from_registry(&Registry::base());
        let carve_pct = if run.non_air + run.carved_blocks == 0 {
            0.0
        } else {
            run.carved_blocks as f64 * 100.0 / (run.non_air + run.carved_blocks) as f64
        };
        out.push_str(&format!(
            "{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.2},{},{},{},{},{:.4},{},{},{},{},{:.4},true\n",
            run.seed,
            run.chunks,
            stats.mean_ms,
            stats.p50_ms,
            stats.p95_ms,
            stats.p99_ms,
            stats.max_ms,
            stats.total_ms,
            stats.per_second(),
            run.total_blocks,
            run.non_air,
            run.carved_blocks,
            run.entrance_columns,
            carve_pct,
            tracked.count(&run.counts, tracked.coal),
            tracked.count(&run.counts, tracked.iron),
            tracked.count(&run.counts, tracked.copper),
            run.ore_blocks,
            run.ore_blocks as f64 * 1000.0 / run.non_air.max(1) as f64
        ));
    }
    let aggregate = Stats::from_samples(&all_samples(runs));
    let tracked = TrackedBlocks::from_registry(&Registry::base());
    let total_blocks: u64 = runs.iter().map(|run| run.total_blocks).sum();
    let non_air: u64 = runs.iter().map(|run| run.non_air).sum();
    let carved: u64 = runs.iter().map(|run| run.carved_blocks).sum();
    let entrances: u64 = runs.iter().map(|run| run.entrance_columns).sum();
    let carve_pct = if non_air + carved == 0 {
        0.0
    } else {
        carved as f64 * 100.0 / (non_air + carved) as f64
    };
    let ore_blocks: u64 = runs.iter().map(|run| run.ore_blocks).sum();
    let coal: u64 = runs.iter().map(|run| tracked.count(&run.counts, tracked.coal)).sum();
    let iron: u64 = runs.iter().map(|run| tracked.count(&run.counts, tracked.iron)).sum();
    let copper: u64 = runs.iter().map(|run| tracked.count(&run.counts, tracked.copper)).sum();
    out.push_str(&format!(
        "all,{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.2},{},{},{},{},{:.4},{},{},{},{},{:.4},true\n",
        aggregate.count,
        aggregate.mean_ms,
        aggregate.p50_ms,
        aggregate.p95_ms,
        aggregate.p99_ms,
        aggregate.max_ms,
        aggregate.total_ms,
        aggregate.per_second(),
        total_blocks,
        non_air,
        carved,
        entrances,
        carve_pct,
        coal,
        iron,
        copper,
        ore_blocks,
        ore_blocks as f64 * 1000.0 / non_air.max(1) as f64
    ));
    out
}

/// Builds the markdown summary table.
fn markdown_document(runs: &[SeedRun], tracked: &TrackedBlocks) -> String {
    let mut out = String::from("# Worldgen benchmark\n\n");
    out.push_str("| Seed | Chunks | Mean ms/chunk | p50 | p95 | p99 | Max | Chunks/s | Non-air blocks | Carved blocks | Coal | Iron | Copper |\n");
    out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for run in runs {
        let stats = Stats::from_samples(&run.samples_ms);
        out.push_str(&format!(
            "| `0x{:x}` | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.1} | {} | {} | {} | {} | {} |\n",
            run.seed,
            run.chunks,
            stats.mean_ms,
            stats.p50_ms,
            stats.p95_ms,
            stats.p99_ms,
            stats.max_ms,
            stats.per_second(),
            run.non_air,
            run.carved_blocks,
            tracked.count(&run.counts, tracked.coal),
            tracked.count(&run.counts, tracked.iron),
            tracked.count(&run.counts, tracked.copper)
        ));
    }
    let aggregate = Stats::from_samples(&all_samples(runs));
    out.push_str(&format!(
        "| aggregate | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.1} | | | | | |\n",
        aggregate.count,
        aggregate.mean_ms,
        aggregate.p50_ms,
        aggregate.p95_ms,
        aggregate.p99_ms,
        aggregate.max_ms,
        aggregate.per_second()
    ));
    out
}

/// Flattens every per-chunk sample.
fn all_samples(runs: &[SeedRun]) -> Vec<f64> {
    runs.iter().flat_map(|run| run.samples_ms.iter().copied()).collect()
}

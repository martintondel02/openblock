//! The `openblock-server` headless host and dedicated multiplayer server.
//!
//! Modes:
//!   openblock-server                        20 Hz headless host (console: help/status/save/stop)
//!   openblock-server --listen 127.0.0.1:25565
//!                                           dedicated TCP server (interim JSON protocol v2)
//!   openblock-server --smoke                generate, tick, save, reload and verify determinism
//!   openblock-server --ticks N              run N headless ticks and save
//!   openblock-server --version              print version and protocol information
//!
//! The dedicated server is deliberately unencrypted and unauthenticated in this
//! milestone: bind it to loopback or a trusted LAN. See
//! `docs/spec/18-networking-multiplayer.md` for the protocol and
//! `docs/spec/19-server-administration.md` for the CLI, config and commands.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_content::Registry;
use ob_protocol::{MAX_FRAME_LEN, PROTOCOL_VERSION, sanitise_motd};
use ob_server::net::{
    DEFAULT_CHUNKS_PER_TICK, DEFAULT_INTEREST_RADIUS_CHUNKS, DEFAULT_MAX_PLAYERS, DEFAULT_MOTD,
    DEFAULT_SAVE_INTERVAL_SECONDS, MAX_INTEREST_RADIUS_CHUNKS, NetConfig, ServerFileConfig, ServerMetrics,
    load_server_config,
};
use ob_server::{DedicatedServer, GameMode, Host, HostConfig};
use ob_sim::PlayerInput;

const TICK: Duration = Duration::from_millis(50);

fn main() {
    ob_core::init_logging();
    let (flags, free) = ob_core::parse_args(std::env::args().skip(1));
    let has = |name: &str| flags.iter().any(|(n, _)| n == name);
    let value = |name: &str| {
        flags
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };

    if has("version") {
        print_version();
        return;
    }
    if has("help") || !free.is_empty() {
        print_help();
        return;
    }

    for name in [
        "config",
        "listen",
        "world",
        "seed",
        "data-dir",
        "max-players",
        "motd",
        "render-distance",
        "interest-radius",
        "save-interval",
        "metrics-json",
        "metrics-interval",
        "pack",
        "ticks",
    ] {
        if has(name) && value(name).is_none() {
            exit_usage(&format!("--{name} requires a value"));
        }
    }

    let smoke = has("smoke");
    let ticks_only = has("ticks");
    let no_listen = has("no-listen");
    if has("creative") && has("survival") {
        exit_usage("--creative and --survival are mutually exclusive");
    }

    // 1. Optional flat server.toml. CLI flags below take precedence.
    let file = match value("config") {
        Some(path) => match load_server_config(Path::new(&path)) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("openblock-server: {path}: {error}");
                std::process::exit(2);
            }
        },
        None => ServerFileConfig::default(),
    };

    // 2. Resolve values: CLI > file > default.
    let seed = match resolve_seed(&value("seed"), file.seed, smoke) {
        Ok(seed) => seed,
        Err(message) => exit_usage(&message),
    };
    let world_name = value("world")
        .or_else(|| file.world.clone())
        .unwrap_or_else(|| "world".to_string());
    let data_dir = value("data-dir").map(PathBuf::from).or_else(|| file.data_dir.clone());
    let render_distance = match resolve_i32(
        value("render-distance").as_deref(),
        file.render_distance,
        6,
        "render-distance",
    ) {
        Ok(value) => value.clamp(1, 32),
        Err(message) => exit_usage(&message),
    };
    let interest_radius = match resolve_i32(
        value("interest-radius").as_deref(),
        file.interest_radius.or(file.render_distance),
        DEFAULT_INTEREST_RADIUS_CHUNKS,
        "interest-radius",
    ) {
        Ok(value) => value,
        Err(message) => exit_usage(&message),
    };
    let max_players = match resolve_u64(
        value("max-players").as_deref(),
        file.max_players.map(u64::from),
        u64::from(DEFAULT_MAX_PLAYERS),
        "max-players",
    ) {
        Ok(value) if (1..=500).contains(&value) => value as u16,
        Ok(_) => exit_usage("max-players must be in 1..=500"),
        Err(message) => exit_usage(&message),
    };
    let save_interval = match resolve_u64(
        value("save-interval").as_deref(),
        file.save_interval_seconds,
        DEFAULT_SAVE_INTERVAL_SECONDS,
        "save-interval",
    ) {
        Ok(value) if value <= 86_400 => value,
        Ok(_) => exit_usage("save-interval must be at most 86400 seconds"),
        Err(message) => exit_usage(&message),
    };
    let metrics_json = value("metrics-json").map(PathBuf::from);
    let metrics_interval = match resolve_u64(value("metrics-interval").as_deref(), None, 5, "metrics-interval") {
        Ok(value) if (1..=3600).contains(&value) => value,
        Ok(_) => exit_usage("metrics-interval must be in 1..=3600 seconds"),
        Err(message) => exit_usage(&message),
    };
    if !(1..=MAX_INTEREST_RADIUS_CHUNKS).contains(&interest_radius) {
        exit_usage(&format!("interest-radius must be in 1..={MAX_INTEREST_RADIUS_CHUNKS}"));
    }
    let motd = sanitise_motd(
        &value("motd")
            .or_else(|| file.motd.clone())
            .unwrap_or_else(|| DEFAULT_MOTD.to_string()),
    );
    let game_mode = if has("creative") {
        GameMode::Creative
    } else if has("survival") {
        GameMode::Survival
    } else {
        match file.game_mode.as_deref() {
            Some("creative") => GameMode::Creative,
            Some("survival") | None => GameMode::Survival,
            Some(other) => exit_usage(&format!(
                "game_mode must be \"survival\" or \"creative\", got '{other}'"
            )),
        }
    };

    let listen = if smoke || ticks_only || no_listen {
        None
    } else {
        match resolve_listen(value("listen"), file.listen.as_deref()) {
            Ok(listen) => listen,
            Err(message) => exit_usage(&message),
        }
    };

    if metrics_json.is_some() && listen.is_none() {
        exit_usage("--metrics-json requires --listen (dedicated server mode)");
    }

    let config = HostConfig {
        seed,
        world_name: world_name.clone(),
        data_dir,
        render_distance_chunks: render_distance,
        generate_budget: 4,
    };
    let registry = match value("pack") {
        Some(path) => match ob_content::registry_from_pack(Path::new(&path)) {
            Ok(registry) => Arc::new(registry),
            Err(err) => {
                eprintln!("openblock-server: failed to load content pack '{path}': {err}");
                std::process::exit(2);
            }
        },
        None => Arc::new(Registry::base()),
    };
    log::info!("content: {} block types", registry.len());
    log::info!("{}", server_banner());
    log::info!("networking: interim JSON/TCP protocol v{PROTOCOL_VERSION} (max frame {MAX_FRAME_LEN} bytes)");

    let mut host = match Host::open(Arc::clone(&registry), config.clone()) {
        Ok(host) => host,
        Err(err) => {
            log::error!("failed to open host: {err}");
            std::process::exit(1);
        }
    };
    host.ensure_streaming(16);
    log::info!(
        "world '{}' seed {} loaded: {} chunks, {} sections, {} KiB",
        config.world_name,
        host.world().seed(),
        host.world().chunk_count(),
        host.world().section_count(),
        host.memory_bytes() / 1024
    );

    if smoke {
        std::process::exit(run_smoke(host));
    }
    if let Some(ticks) = value("ticks") {
        let ticks = match ticks.parse::<u64>() {
            Ok(ticks) => ticks,
            Err(_) => exit_usage("invalid --ticks value"),
        };
        std::process::exit(run_ticks(host, ticks));
    }

    if let Some(listen) = listen {
        let net_config = NetConfig {
            listen,
            max_players,
            motd,
            interest_radius_chunks: interest_radius,
            chunks_per_tick: DEFAULT_CHUNKS_PER_TICK,
            save_interval_seconds: save_interval,
            whitelist: file.whitelist.unwrap_or_default(),
            game_mode,
        };
        if let Err(error) = net_config.validate() {
            exit_usage(&error);
        }
        match DedicatedServer::start(host, net_config) {
            Ok(server) => std::process::exit(run_network(server, save_interval, metrics_json, metrics_interval)),
            Err(error) => {
                log::error!("could not start the network server on {listen}: {error}");
                std::process::exit(1);
            }
        }
    }

    // No listener: preserve the original headless single-player host mode.
    run_dedicated(&mut host);
}

/// Version banner shared with the engine core.
///
/// `ob_core::PROTOCOL_VERSION` and `ob_protocol::PROTOCOL_VERSION` are kept in
/// lockstep; a server integration test asserts they agree.
fn server_banner() -> String {
    ob_core::version_banner("openblock-server")
}

/// Prints version and protocol information.
fn print_version() {
    println!("{}", server_banner());
    println!("save format: {}", ob_core::SAVE_FORMAT_VERSION);
    println!("protocol: {PROTOCOL_VERSION} (interim JSON over TCP)");
    println!("max frame: {MAX_FRAME_LEN} bytes");
    println!("networking: enabled with --listen ADDR; disabled by default");
}

/// Prints the CLI. `--help` is the source of truth for flags; the spec table is
/// kept in sync manually.
fn print_help() {
    println!("usage: openblock-server [OPTIONS]");
    println!();
    println!("Modes (default: 20 Hz headless host):");
    println!("  --listen ADDR            start the dedicated TCP server (e.g. 127.0.0.1:25565)");
    println!("  --no-listen              disable a listener configured in server.toml");
    println!("  --smoke                  generate, tick, save, reload and verify determinism");
    println!("  --ticks N                run N headless ticks, save and exit");
    println!("  --version                print version and protocol information");
    println!("  --help                   print this help");
    println!();
    println!("World:");
    println!("  --world NAME             world directory name (default: world)");
    println!("  --seed N                 world seed (default: random; 0 for --smoke if unset)");
    println!("  --data-dir DIR           data directory root");
    println!("  --render-distance N      single-player streaming radius in chunks (default: 6)");
    println!("  --pack PATH              load a JSON content pack (default: base blocks)");
    println!();
    println!("Dedicated server:");
    println!("  --config PATH            flat server.toml configuration (CLI overrides it)");
    println!("  --max-players N          player cap, 1..=500 (default: 8)");
    println!("  --motd TEXT              message of the day, at most 128 bytes");
    println!("  --interest-radius N      per-client chunk radius, 1..=32 (default: 6)");
    println!("  --save-interval SECONDS  autosave interval, 0 disables (default: 60)");
    println!("  --metrics-json PATH      write periodic JSON metrics snapshots (listener mode)");
    println!("  --metrics-interval SEC   seconds between metrics snapshots (default: 5)");
    println!("  --survival               default game mode for new network players (default)");
    println!("  --creative               creative default for network players (no item use)");
    println!();
    println!("server.toml keys: world, seed, data_dir, listen, max_players, motd,");
    println!("                  render_distance, interest_radius, save_interval_seconds, whitelist,");
    println!("                  game_mode (\"survival\" or \"creative\")");
}

/// Prints a usage error and exits with status 2.
fn exit_usage(message: &str) -> ! {
    eprintln!("openblock-server: {message}");
    std::process::exit(2);
}

/// Resolves the seed from CLI, file and mode defaults.
fn resolve_seed(cli: &Option<String>, file: Option<u64>, smoke: bool) -> Result<u64, String> {
    match cli {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| format!("invalid --seed value '{value}'")),
        None => Ok(file.unwrap_or_else(|| {
            if smoke {
                0
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0)
            }
        })),
    }
}

/// Resolves an integer CLI/file/default value.
fn resolve_i32(cli: Option<&str>, file: Option<i32>, default: i32, name: &str) -> Result<i32, String> {
    match cli {
        Some(value) => value
            .parse::<i32>()
            .map_err(|_| format!("invalid --{name} value '{value}'")),
        None => Ok(file.unwrap_or(default)),
    }
}

/// Resolves an unsigned integer CLI/file/default value.
fn resolve_u64(cli: Option<&str>, file: Option<u64>, default: u64, name: &str) -> Result<u64, String> {
    match cli {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| format!("invalid --{name} value '{value}'")),
        None => Ok(file.unwrap_or(default)),
    }
}

/// Resolves the optional listener address.
fn resolve_listen(cli: Option<String>, file: Option<&str>) -> Result<Option<SocketAddr>, String> {
    match cli.or_else(|| file.map(str::to_string)) {
        Some(text) => text
            .parse::<SocketAddr>()
            .map(Some)
            .map_err(|_| format!("invalid listen address '{text}' (expected IP:port)")),
        None => Ok(None),
    }
}

/// Existing deterministic smoke test, unchanged apart from the host APIs.
fn run_smoke(mut host: Host) -> i32 {
    log::info!("smoke test: generating, ticking, saving, reloading");
    for _ in 0..3 {
        host.ensure_streaming(8);
    }
    for i in 0..200u64 {
        let input = PlayerInput {
            forward: if i % 40 < 20 { 1.0 } else { 0.0 },
            ..PlayerInput::default()
        };
        host.tick(&input);
        if i % 10 == 0 {
            host.ensure_streaming(2);
        }
    }
    let mut guard = 0;
    while host.ensure_streaming(8) > 0 {
        guard += 1;
        if guard > 200 {
            eprintln!("smoke: streaming did not converge");
            break;
        }
    }
    let hash_before = host.state_hash();
    if let Err(err) = host.save() {
        log::error!("smoke: save failed: {err}");
        return 1;
    }
    let config = host.config().clone();
    let registry = Arc::clone(host.registry());
    drop(host);
    let mut host = match Host::open(Arc::clone(&registry), config) {
        Ok(fresh) => fresh,
        Err(err) => {
            log::error!("smoke: reopen failed: {err}");
            return 1;
        }
    };
    while host.ensure_streaming(8) > 0 {}
    let hash_after = host.state_hash();
    if hash_before != hash_after {
        log::error!("smoke FAILED: state hash changed across save/load ({hash_before:016x} != {hash_after:016x})");
        return 1;
    }
    log::info!("smoke OK: state hash {hash_before:016x}");
    println!("smoke OK hash={hash_before:016x}");
    0
}

/// Runs a fixed number of ticks and saves, preserving the pre-network mode.
fn run_ticks(mut host: Host, ticks: u64) -> i32 {
    for _ in 0..ticks {
        host.tick(&PlayerInput::default());
        host.ensure_streaming(16);
    }
    if let Err(err) = host.save() {
        log::error!("save failed: {err}");
        return 1;
    }
    log::info!("ran {} ticks; state hash {:016x}", ticks, host.state_hash());
    0
}

/// Writes one atomic JSON metrics snapshot, logging and continuing on failure.
fn write_metrics_snapshot(path: &Path, server: &DedicatedServer) {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result =
        std::fs::write(&temporary, metrics_json(&server.metrics())).and_then(|()| std::fs::rename(&temporary, path));
    if let Err(error) = result {
        log::warn!("could not write metrics JSON {}: {error}", path.display());
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Formats a [`ServerMetrics`] snapshot as the documented JSON object.
fn metrics_json(metrics: &ServerMetrics) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let mut out = String::with_capacity(2048);
    out.push('{');
    let mut first = true;
    json_string_field(&mut out, &mut first, "schema", "openblock.server-metrics/1");
    json_u64_field(&mut out, &mut first, "timestamp_unix_ms", timestamp);
    json_f64_field(&mut out, &mut first, "uptime_seconds", metrics.uptime_seconds);
    json_string_field(&mut out, &mut first, "listen", &metrics.listen);
    json_string_field(&mut out, &mut first, "world", &metrics.world_name);
    json_u64_field(&mut out, &mut first, "seed", metrics.seed);
    json_string_field(&mut out, &mut first, "game_mode", metrics.game_mode);
    json_u64_field(&mut out, &mut first, "max_players", u64::from(metrics.max_players));
    json_u64_field(
        &mut out,
        &mut first,
        "interest_radius_chunks",
        metrics.interest_radius_chunks as u64,
    );
    json_u64_field(&mut out, &mut first, "chunks_per_tick", metrics.chunks_per_tick as u64);
    json_u64_field(
        &mut out,
        &mut first,
        "save_interval_seconds",
        metrics.save_interval_seconds,
    );
    json_u64_field(&mut out, &mut first, "tick_count", metrics.tick_count);
    json_f64_field(&mut out, &mut first, "tps", f64::from(metrics.tps));
    json_u64_field(&mut out, &mut first, "tick_samples", metrics.tick_samples as u64);
    json_f64_field(&mut out, &mut first, "tick_p50_ms", metrics.tick_p50_ms);
    json_f64_field(&mut out, &mut first, "tick_p95_ms", metrics.tick_p95_ms);
    json_f64_field(&mut out, &mut first, "tick_p99_ms", metrics.tick_p99_ms);
    json_f64_field(&mut out, &mut first, "tick_max_ms", metrics.tick_max_ms);
    json_u64_field(&mut out, &mut first, "tick_overruns", metrics.tick_overruns);
    json_u64_field(&mut out, &mut first, "tick_over_35ms", metrics.tick_over_envelope);
    json_u64_field(
        &mut out,
        &mut first,
        "players_connected",
        metrics.players_connected as u64,
    );
    json_u64_field(&mut out, &mut first, "chunks_queued", metrics.chunks_queued);
    json_u64_field(&mut out, &mut first, "chunk_bytes_queued", metrics.chunk_bytes_queued);
    json_u64_field(&mut out, &mut first, "bytes_in", metrics.bytes_in);
    json_u64_field(&mut out, &mut first, "bytes_out", metrics.bytes_out);
    json_u64_field(&mut out, &mut first, "moves", metrics.moves);
    json_u64_field(&mut out, &mut first, "edits_attempted", metrics.edits_attempted);
    json_u64_field(&mut out, &mut first, "edits_accepted", metrics.edits_accepted);
    json_u64_field(&mut out, &mut first, "edits_rejected", metrics.edits_rejected);
    json_u64_field(&mut out, &mut first, "chats", metrics.chats);
    json_u64_field(&mut out, &mut first, "pings", metrics.pings);
    json_u64_field(&mut out, &mut first, "chunk_requests", metrics.chunk_requests);
    json_u64_field(&mut out, &mut first, "actions_rejected", metrics.actions_rejected);
    json_u64_field(&mut out, &mut first, "mobs", metrics.mobs as u64);
    json_u64_field(&mut out, &mut first, "item_entities", metrics.item_entities as u64);
    json_u64_field(&mut out, &mut first, "loaded_columns", metrics.loaded_columns as u64);
    json_option_u64_field(&mut out, &mut first, "rss_kib", metrics.rss_kib);
    json_option_u64_field(&mut out, &mut first, "threads", metrics.threads);
    json_option_u64_field(&mut out, &mut first, "open_fds", metrics.open_fds);
    out.push_str("}\n");
    out
}

/// Appends `"key":` with the comma placement owned by `first`.
fn json_key(out: &mut String, first: &mut bool, key: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    json_string(out, key);
    out.push(':');
}

/// Appends a JSON string with the characters that can appear in paths escaped.
fn json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

fn json_string_field(out: &mut String, first: &mut bool, key: &str, value: &str) {
    json_key(out, first, key);
    json_string(out, value);
}

fn json_u64_field(out: &mut String, first: &mut bool, key: &str, value: u64) {
    json_key(out, first, key);
    out.push_str(&value.to_string());
}

fn json_option_u64_field(out: &mut String, first: &mut bool, key: &str, value: Option<u64>) {
    json_key(out, first, key);
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("null"),
    }
}

fn json_f64_field(out: &mut String, first: &mut bool, key: &str, value: f64) {
    json_key(out, first, key);
    if value.is_finite() {
        out.push_str(&format!("{value:.3}"));
    } else {
        out.push_str("null");
    }
}

/// Network mode: start the tick/accept threads, then serve the admin console and
/// signals until `stop`. `save_interval` is only logged here (the net layer
/// owns autosave).
fn run_network(
    server: DedicatedServer,
    save_interval: u64,
    metrics_json: Option<PathBuf>,
    metrics_interval: u64,
) -> i32 {
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        if let Err(err) = ctrlc::set_handler(move || {
            flag.store(true, Ordering::SeqCst);
        }) {
            log::warn!("could not install the signal handler: {err}");
        }
    }
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line.trim().to_string()).is_err() {
                break;
            }
        }
    });

    log::info!(
        "dedicated server listening on {} (max {} players, {} chunk interest, autosave {} s)",
        server.local_addr(),
        server.max_players(),
        server.interest_radius_chunks(),
        save_interval
    );
    log::info!("commands: help | list | say <msg> | kick <id|name> [reason] | save | tps | stop");
    let mut next_metrics = Instant::now() + Duration::from_secs(metrics_interval);
    if let Some(path) = &metrics_json {
        write_metrics_snapshot(path, &server);
        log::info!(
            "metrics: writing a JSON snapshot to {} every {} s",
            path.display(),
            metrics_interval
        );
    }
    let mut exit_code = 0;
    'run: loop {
        if shutdown.load(Ordering::SeqCst) {
            log::info!("shutdown signal received");
            break;
        }
        if let Some(path) = &metrics_json
            && Instant::now() >= next_metrics
        {
            write_metrics_snapshot(path, &server);
            next_metrics = Instant::now() + Duration::from_secs(metrics_interval);
        }
        loop {
            match rx.try_recv() {
                Ok(command) => {
                    if handle_console_command(&server, &command).stop {
                        break 'run;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            log::info!("shutdown signal received");
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if let Some(path) = &metrics_json {
        write_metrics_snapshot(path, &server);
    }
    log::info!("stopping: disconnecting clients and saving the world");
    match server.stop() {
        Ok(()) => log::info!("world saved; shutdown complete"),
        Err(error) => {
            log::error!("shutdown save failed: {error}");
            exit_code = 1;
        }
    }
    exit_code
}

/// Result of one console command.
struct CommandOutcome {
    /// Whether the main loop should stop.
    stop: bool,
}

/// Handles one console line for network mode.
fn handle_console_command(server: &DedicatedServer, command: &str) -> CommandOutcome {
    let command = command.trim();
    if command.is_empty() {
        return CommandOutcome { stop: false };
    }
    let (verb, rest) = command
        .split_once(' ')
        .map(|(verb, rest)| (verb, rest.trim()))
        .unwrap_or((command, ""));
    match verb {
        "stop" | "quit" | "exit" => {
            log::info!("stopping");
            CommandOutcome { stop: true }
        }
        "help" => {
            log::info!("commands: help | list | say <msg> | kick <id|name> [reason] | save | tps | stop");
            CommandOutcome { stop: false }
        }
        "list" => {
            let players = server.players();
            if players.is_empty() {
                log::info!("no players connected");
            } else {
                for player in players {
                    log::info!(
                        "  {} '{}' at {:.1},{:.1},{:.1}",
                        player.id,
                        player.name,
                        player.pos[0],
                        player.pos[1],
                        player.pos[2]
                    );
                }
            }
            CommandOutcome { stop: false }
        }
        "say" => {
            if rest.is_empty() {
                log::warn!("usage: say <message>");
            } else if !server.broadcast_chat(rest) {
                log::warn!("could not broadcast chat (empty/too long, or server stopping)");
            }
            CommandOutcome { stop: false }
        }
        "kick" => {
            let mut parts = rest.splitn(2, ' ');
            let target = parts.next().unwrap_or("").trim();
            if target.is_empty() {
                log::warn!("usage: kick <id|name> [reason]");
            } else {
                let reason = parts.next().unwrap_or("kicked_by_admin").trim();
                if server.kick(target, reason) {
                    log::info!("kicked {target}");
                } else {
                    log::warn!("no connected player matches '{target}'");
                }
            }
            CommandOutcome { stop: false }
        }
        "save" => {
            match server.save() {
                Ok(()) => log::info!("saved"),
                Err(error) => log::error!("save failed: {error}"),
            }
            CommandOutcome { stop: false }
        }
        "tps" => {
            let players = server.player_count();
            log::info!(
                "tps {:.1}, players {}, ticks {}",
                server.tps(),
                players,
                server.tick_count()
            );
            CommandOutcome { stop: false }
        }
        other => {
            log::warn!("unknown command '{other}' (try help)");
            CommandOutcome { stop: false }
        }
    }
}

/// Original no-listener headless mode: 20 Hz host loop with file-only world.
fn run_dedicated(host: &mut Host) {
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        if let Err(err) = ctrlc::set_handler(move || {
            flag.store(true, Ordering::SeqCst);
        }) {
            log::warn!("could not install the signal handler: {err}");
        }
    }
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line.trim().to_string()).is_err() {
                break;
            }
        }
    });

    log::info!("commands: status | save | stop | help");
    let started = Instant::now();
    let mut last_tick = Instant::now();
    let mut last_save = Instant::now();
    let mut last_status = Instant::now();
    let mut ticks_in_window = 0u32;
    let mut tps = 0f32;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            log::info!("shutdown signal received");
            if let Err(err) = host.save() {
                log::error!("save on shutdown failed: {err}");
            }
            log::info!("saved; ran {} ticks", host.tick_count());
            return;
        }
        let now = Instant::now();
        if now.duration_since(last_tick) >= TICK {
            last_tick += TICK;
            host.tick(&PlayerInput::default());
            host.ensure_streaming(16);
            ticks_in_window += 1;
            if now.duration_since(last_status) >= Duration::from_secs(5) {
                tps = ticks_in_window as f32 / now.duration_since(last_status).as_secs_f32();
                ticks_in_window = 0;
                last_status = now;
                log::debug!(
                    "tps {:.1}, chunks {}, ticks {}",
                    tps,
                    host.world().chunk_count(),
                    host.tick_count()
                );
            }
            if now.duration_since(last_save) >= Duration::from_secs(30) {
                if let Err(err) = host.save() {
                    log::error!("autosave failed: {err}");
                }
                last_save = now;
            }
        }
        while let Ok(command) = rx.try_recv() {
            match command.as_str() {
                "stop" | "quit" | "exit" => {
                    log::info!("stopping");
                    if let Err(err) = host.save() {
                        log::error!("save on stop failed: {err}");
                    }
                    log::info!(
                        "ran {} ticks in {:.1} s",
                        host.tick_count(),
                        started.elapsed().as_secs_f64()
                    );
                    return;
                }
                "save" => match host.save() {
                    Ok(()) => log::info!("saved"),
                    Err(err) => log::error!("save failed: {err}"),
                },
                "status" => log::info!(
                    "world '{}' seed {} chunks {} sections {} ticks {} tps {:.1}",
                    host.config().world_name,
                    host.world().seed(),
                    host.world().chunk_count(),
                    host.world().section_count(),
                    host.tick_count(),
                    tps
                ),
                "help" => log::info!("commands: status | save | stop | help"),
                other => log::warn!("unknown command '{other}' (try help)"),
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

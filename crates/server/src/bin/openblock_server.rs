//! The `openblock-server` headless host binary.
//!
//! v0.0.1 modes:
//!   openblock-server                    run the host at 20 Hz (stdin: help/status/save/stop)
//!   openblock-server --smoke            generate, tick, save, reload and verify determinism
//!   openblock-server --ticks N          run N headless ticks and save
//!   openblock-server --version          print version information
//!
//! Networking (QUIC, MS-06) is not wired up yet; the host itself is the exact
//! simulation the client embeds.

use std::io::BufRead;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ob_content::Registry;
use ob_server::{Host, HostConfig};
use ob_sim::PlayerInput;

const TICK: Duration = Duration::from_millis(50);

fn main() {
    ob_core::init_logging();
    let (flags, free) = ob_core::parse_args(std::env::args().skip(1));
    let has = |name: &str| flags.iter().any(|(n, _)| n == name);
    let value = |name: &str| {
        flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };

    if has("version") {
        println!("{}", ob_core::version_banner("openblock-server"));
        println!("save format: {}", ob_core::SAVE_FORMAT_VERSION);
        println!("networking: not yet implemented (MS-06)");
        return;
    }
    if has("help") || !free.is_empty() {
        println!("usage: openblock-server [--world NAME] [--seed N] [--data-dir DIR]");
        println!("                         [--render-distance N] [--smoke] [--ticks N]");
        println!("                         [--version] [--help]");
        return;
    }

    let seed = value("seed")
        .map(|v| v.parse::<u64>())
        .transpose()
        .unwrap_or_else(|_| {
            eprintln!("openblock-server: invalid --seed value");
            std::process::exit(2);
        })
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    let config = HostConfig {
        seed,
        world_name: value("world").unwrap_or_else(|| "world".to_string()),
        data_dir: value("data-dir").map(Into::into),
        render_distance_chunks: value("render-distance").and_then(|v| v.parse().ok()).unwrap_or(6),
        generate_budget: 4,
    };
    let registry = Arc::new(Registry::base());
    log::info!("{}", ob_core::version_banner("openblock-server"));
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
    log::info!("this build has no networking yet (QUIC multiplayer arrives at MS-06); running headless host only");

    if has("smoke") {
        let code = run_smoke(host);
        std::process::exit(code);
    }
    if let Some(ticks) = value("ticks") {
        let ticks = match ticks.parse::<u64>() {
            Ok(ticks) => ticks,
            Err(_) => {
                eprintln!("openblock-server: invalid --ticks value");
                std::process::exit(2);
            }
        };
        for _ in 0..ticks {
            host.tick(&PlayerInput::default());
            host.ensure_streaming(2);
        }
        if let Err(err) = host.save() {
            log::error!("save failed: {err}");
            std::process::exit(1);
        }
        log::info!("ran {} ticks; state hash {:016x}", ticks, host.state_hash());
        return;
    }

    run_dedicated(&mut host);
}

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

fn run_dedicated(host: &mut Host) {
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
        let now = Instant::now();
        if now.duration_since(last_tick) >= TICK {
            last_tick += TICK;
            host.tick(&PlayerInput::default());
            host.ensure_streaming(4);
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

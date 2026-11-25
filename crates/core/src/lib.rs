//! Engine core: logging, version information and platform data directories.
//!
//! v0.0.1 keeps this crate deliberately small. The scheduler, service
//! registry and diagnostics described in `docs/spec/02-engine-core.md` arrive
//! with the engine milestones; the parts that both binaries need now live here.

#![allow(missing_docs)] // TODO(MS-04): tighten once the engine core API stabilises

use std::path::PathBuf;

pub mod config;

pub use log::{debug, error, info, warn};

/// Product version of the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Save format version understood by this build.
pub const SAVE_FORMAT_VERSION: u16 = 4;
/// Interim wire protocol version implemented by `ob_protocol`.
///
/// `ob_core` is a foundation crate and cannot depend on `ob_protocol`, so this
/// is a plain constant rather than a re-export. It must track
/// `ob_protocol::PROTOCOL_VERSION`; the server integration tests assert that
/// both values agree.
pub const PROTOCOL_VERSION: u16 = 4;

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => "ERROR",
            log::Level::Warn => "WARN ",
            log::Level::Info => "INFO ",
            log::Level::Debug => "DEBUG",
            log::Level::Trace => "TRACE",
        };
        eprintln!("[{level}] {}: {}", record.target(), record.args());
    }

    fn flush(&self) {}
}

static LOGGER: SimpleLogger = SimpleLogger;

/// Initialises the global logger from `OPENBLOCK_LOG` (default `info`).
pub fn init_logging() {
    let level = std::env::var("OPENBLOCK_LOG").unwrap_or_else(|_| "info".to_string());
    let filter = match level.as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        "off" => log::LevelFilter::Off,
        _ => log::LevelFilter::Info,
    };
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(filter);
}

/// Returns the platform data directory for OpenBlock.
///
/// Resolution order: `OPENBLOCK_DATA_DIR`, `XDG_DATA_HOME/openblock`,
/// `~/.local/share/openblock`, then `./run/data` as a last resort. Matches
/// `docs/spec/22-platforms-and-distribution.md` (Fedora-first XDG paths).
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENBLOCK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("openblock");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/openblock");
    }
    PathBuf::from("run/data")
}

/// Short version banner shared by both binaries.
pub fn version_banner(product: &str) -> String {
    format!(
        "{product} {} (save format {}, protocol {}, {}/{})",
        VERSION,
        SAVE_FORMAT_VERSION,
        PROTOCOL_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Parses `--flag value` style arguments into a simple map plus free arguments.
pub fn parse_args(args: impl Iterator<Item = String>) -> (Vec<(String, String)>, Vec<String>) {
    let mut flags = Vec::new();
    let mut free = Vec::new();
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            if let Some((name, value)) = name.split_once('=') {
                flags.push((name.to_string(), value.to_string()));
            } else if iter.peek().is_some_and(|next| !next.starts_with("--")) {
                let value = iter.next().unwrap_or_default();
                flags.push((name.to_string(), value));
            } else {
                flags.push((name.to_string(), String::new()));
            }
        } else {
            free.push(arg);
        }
    }
    (flags, free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_free_args() {
        let args = vec![
            "--seed".to_string(),
            "42".to_string(),
            "--smoke".to_string(),
            "--world=test".to_string(),
        ];
        let (flags, free) = parse_args(args.into_iter());
        assert!(free.is_empty());
        assert_eq!(flags[0], ("seed".to_string(), "42".to_string()));
        assert_eq!(flags[1], ("smoke".to_string(), String::new()));
        assert_eq!(flags[2], ("world".to_string(), "test".to_string()));
    }

    #[test]
    fn version_banner_mentions_version() {
        assert!(version_banner("openblock").contains(VERSION));
    }

    #[test]
    fn protocol_version_matches_the_implemented_wire_protocol() {
        assert_eq!(PROTOCOL_VERSION, 4);
        assert!(version_banner("openblock").contains("protocol 4"));
    }
}

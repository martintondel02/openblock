//! Small measurement helpers shared by the CPU benchmark and soak binaries.
//!
//! The module deliberately has no external dependencies: it computes
//! nearest-rank percentiles from millisecond samples, reads the Linux process
//! RSS proxy out of `/proc/self/status`, and creates unique, self-cleaning
//! temporary data directories. Everything here is deterministic apart from the
//! sampled wall-clock measurements themselves.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One summary of a millisecond sample series.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// Number of samples in the series.
    pub count: usize,
    /// Smallest sample, in milliseconds.
    pub min_ms: f64,
    /// Arithmetic mean, in milliseconds.
    pub mean_ms: f64,
    /// Nearest-rank median, in milliseconds.
    pub p50_ms: f64,
    /// Nearest-rank 95th percentile, in milliseconds.
    pub p95_ms: f64,
    /// Nearest-rank 99th percentile, in milliseconds.
    pub p99_ms: f64,
    /// Largest sample, in milliseconds.
    pub max_ms: f64,
    /// Sum of every sample, in milliseconds.
    pub total_ms: f64,
}

impl Stats {
    /// Summarises a sample series.
    ///
    /// An empty series yields all zeroes. Percentiles use the nearest-rank
    /// definition required by `docs/spec/25-performance-budgets.md` §3.7:
    /// rank `ceil(p/100 * n)`, clamped to `1..=n`.
    pub fn from_samples(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted: Vec<f64> = samples.iter().copied().filter(|value| value.is_finite()).collect();
        if sorted.is_empty() {
            return Self::default();
        }
        sorted.sort_by(f64::total_cmp);
        let count = sorted.len();
        let total_ms: f64 = sorted.iter().sum();
        Self {
            count,
            min_ms: sorted[0],
            mean_ms: total_ms / count as f64,
            p50_ms: percentile_sorted(&sorted, 50.0),
            p95_ms: percentile_sorted(&sorted, 95.0),
            p99_ms: percentile_sorted(&sorted, 99.0),
            max_ms: sorted[count - 1],
            total_ms,
        }
    }

    /// Mean throughput implied by the mean sample, in operations per second.
    ///
    /// Returns `0.0` for an empty or zero-mean series.
    pub fn per_second(&self) -> f64 {
        if self.mean_ms <= 0.0 {
            0.0
        } else {
            1000.0 / self.mean_ms
        }
    }
}

/// Nearest-rank percentile of an already-sorted millisecond series.
///
/// `percentile` is clamped to `0..=100`; an empty slice yields `0.0`.
pub fn percentile_sorted(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let percentile = percentile.clamp(0.0, 100.0);
    let rank = (percentile / 100.0 * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted[index]
}

/// Reads the current process RSS from `/proc/self/status`, in KiB.
///
/// Returns `None` on platforms without procfs or when the field is absent.
pub fn rss_kib() -> Option<u64> {
    read_status_kib("VmRSS:")
}

/// Reads the process peak RSS (`VmHWM`) from `/proc/self/status`, in KiB.
pub fn peak_rss_kib() -> Option<u64> {
    read_status_kib("VmHWM:")
}

/// Reads one kilobyte field from `/proc/self/status`.
fn read_status_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Converts a KiB value to a human-readable MiB string.
pub fn mib(kib: u64) -> String {
    format!("{:.1} MiB", kib as f64 / 1024.0)
}

/// Formats a byte count as `B`, `KiB` or `MiB`.
pub fn bytes_label(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Returns the CPU model name from `/proc/cpuinfo` (or `unknown`).
pub fn cpu_model() -> String {
    let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "unknown".to_string();
    };
    for line in cpuinfo.lines() {
        if let Some(rest) = line.strip_prefix("model name")
            && let Some(value) = rest.split_once(':').map(|(_, value)| value.trim())
            && !value.is_empty()
        {
            return value.to_string();
        }
    }
    "unknown".to_string()
}

/// Returns a short OS description suitable for a benchmark header.
pub fn os_description() -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{} {} ({})", std::env::consts::OS, kernel, std::env::consts::ARCH)
}

/// Creates a unique temporary directory that is removed on drop.
///
/// The name combines `prefix`, the process id and a monotonic counter so
/// parallel benchmark runs cannot share a world directory.
pub fn temp_dir(prefix: &str) -> io::Result<TempDir> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("{prefix}-{}-{stamp:x}-{sequence}", std::process::id());
    let path = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&path)?;
    Ok(TempDir { path })
}

/// A temporary directory removed recursively when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// The directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consumes the guard and returns the path, leaving the directory behind.
    ///
    /// Useful for failed-run diagnostics; callers are then responsible for
    /// deleting it.
    pub fn keep(self) -> PathBuf {
        let path = self.path.clone();
        std::mem::forget(self);
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes `contents` to `path`, creating parent directories when needed.
pub fn write_text(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Formats `stats` as one markdown table row beginning with `label`.
///
/// The columns are `label | samples | mean | p50 | p95 | p99 | max`.
pub fn markdown_stats_row(label: &str, stats: &Stats) -> String {
    format!(
        "| {label} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
        stats.count, stats.mean_ms, stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.max_ms
    )
}

/// Formats a duration in milliseconds with a fixed three-decimal precision.
pub fn ms(value: f64) -> String {
    format!("{value:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_the_spec_example() {
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile_sorted(&sorted, 50.0), 50.0);
        assert_eq!(percentile_sorted(&sorted, 95.0), 95.0);
        assert_eq!(percentile_sorted(&sorted, 99.0), 99.0);
        assert_eq!(percentile_sorted(&sorted, 100.0), 100.0);
        assert_eq!(percentile_sorted(&sorted, 0.0), 1.0);
    }

    #[test]
    fn stats_describe_a_small_series() {
        let stats = Stats::from_samples(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(stats.count, 4);
        assert_eq!(stats.mean_ms, 2.5);
        assert_eq!(stats.p50_ms, 2.0);
        assert_eq!(stats.p95_ms, 4.0);
        assert_eq!(stats.max_ms, 4.0);
        assert_eq!(stats.per_second(), 400.0);
    }

    #[test]
    fn empty_and_non_finite_samples_are_tolerated() {
        let stats = Stats::from_samples(&[]);
        assert_eq!(stats, Stats::default());
        let stats = Stats::from_samples(&[f64::NAN, f64::INFINITY]);
        assert_eq!(stats, Stats::default());
    }

    #[test]
    fn temp_directory_is_created_and_removed() {
        let dir = temp_dir("ob-measure-test").expect("temp dir");
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        std::fs::write(path.join("marker"), b"x").expect("write");
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn byte_labels_are_readable() {
        assert_eq!(bytes_label(12), "12 B");
        assert_eq!(bytes_label(2048), "2.0 KiB");
        assert_eq!(bytes_label(3 * 1024 * 1024), "3.00 MiB");
    }

    #[test]
    fn rss_readers_return_sane_values_on_linux() {
        if cfg!(target_os = "linux") {
            let rss = rss_kib().expect("VmRSS");
            assert!(rss > 0);
            let peak = peak_rss_kib().expect("VmHWM");
            assert!(peak >= rss);
        }
    }
}

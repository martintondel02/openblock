//! Background save worker: encode + compress + stage + fsync + rename.
//!
//! The tick thread captures a [`WorldSnapshot`] (shared `Arc` columns, O(columns)
//! pointer copies) and serializes the small sidecars while it holds the host
//! lock. Everything expensive — the v4 world encoding, zstd compression, staged
//! writes and `fsync`s — runs on this worker. The worker never touches the live
//! [`World`], so a save cannot overlap a tick or another writer.
//!
//! ## Commit order (kept from the 1.0.1 audit)
//!
//! Every payload is written to `<target>.tmp` and `fsync`ed first. Only after
//! **all** temporary files exist are they renamed over the targets. Sidecars are
//! renamed first and `world.obs` last, so a crash leaves the previous snapshot
//! paired with the previous sidecars or a new snapshot with new sidecars —
//! never a truncated snapshot. The group is not transactional across a crash,
//! which is the documented spec 17 §3.5 limitation that a journal or region file
//! would fix.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ob_world::{SaveError, WorldSnapshot};

/// Result of one committed save.
#[derive(Debug, Clone)]
pub struct SaveReport {
    /// Bytes in the committed `world.obs`.
    pub world_bytes: u64,
    /// Bytes in every committed sidecar.
    pub sidecar_bytes: u64,
    /// Total bytes written by this save.
    pub total_bytes: u64,
    /// Number of world columns captured.
    pub chunks: usize,
    /// Time spent serializing and compressing the world snapshot.
    pub encode_ms: f64,
    /// Time spent staging, fsyncing and renaming every file.
    pub write_ms: f64,
    /// Total worker wall time, including thread startup.
    pub elapsed_ms: f64,
}

/// Failure from a background save.
#[derive(Debug)]
pub(crate) enum SaveFailure {
    /// The world snapshot could not be encoded.
    World(SaveError),
    /// The staged write, fsync or rename failed.
    Io(io::Error),
}

impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::World(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SaveFailure {}

impl From<SaveError> for SaveFailure {
    fn from(error: SaveError) -> Self {
        Self::World(error)
    }
}

impl From<io::Error> for SaveFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Immutable inputs for one background save.
pub(crate) struct SaveJob {
    /// World directory (created before staging if missing).
    pub dir: PathBuf,
    /// World snapshot captured under the host lock.
    pub snapshot: WorldSnapshot,
    /// Serialized sidecars in commit order (`world.obs` is appended last).
    pub sidecars: Vec<(&'static str, Vec<u8>)>,
    /// Light semantics version to persist in `world_meta.txt` **after** the
    /// world snapshot commits. The marker must never claim a migration the
    /// committed `world.obs` does not have, so it is written only once every
    /// rename succeeded.
    pub light_version: u32,
}

/// Starts a named save thread and returns its join handle.
pub(crate) fn spawn(job: SaveJob) -> io::Result<std::thread::JoinHandle<Result<SaveReport, SaveFailure>>> {
    std::thread::Builder::new()
        .name("ob-save".to_string())
        .spawn(move || run(job))
}

/// Worker body; all errors are returned to the host instead of panicking.
fn run(job: SaveJob) -> Result<SaveReport, SaveFailure> {
    let started = Instant::now();
    let encode_started = Instant::now();
    let world = job.snapshot.to_bytes_checked()?;
    let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;

    let write_started = Instant::now();
    std::fs::create_dir_all(&job.dir)?;
    commit(&job.dir, &job.sidecars, &world)?;
    write_light_meta(&job.dir, job.light_version)?;
    let write_ms = write_started.elapsed().as_secs_f64() * 1000.0;

    let sidecar_bytes = job.sidecars.iter().map(|(_, bytes)| bytes.len() as u64).sum();
    Ok(SaveReport {
        world_bytes: world.len() as u64,
        sidecar_bytes,
        total_bytes: world.len() as u64 + sidecar_bytes,
        chunks: job.snapshot.chunk_count(),
        encode_ms,
        write_ms,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Writes the additive `world_meta.txt` marker after a successful commit.
///
/// Existing unknown keys and comments survive. A failure is reported to the
/// host; the world snapshot itself is already valid, and the missing marker
/// only causes a safe relight on the next load.
fn write_light_meta(dir: &Path, light_version: u32) -> Result<(), SaveFailure> {
    let mut meta = ob_world::read_world_meta(dir)?.unwrap_or_default();
    meta.light_version = light_version;
    ob_world::write_world_meta(dir, &meta)?;
    Ok(())
}

/// Stages every payload, fsyncs it, then renames sidecars first and the world
/// snapshot last.
///
/// Target paths that are directories are rejected before staging, so a bad
/// path cannot leave the group half written.
pub(crate) fn commit(dir: &Path, sidecars: &[(&str, Vec<u8>)], world: &[u8]) -> io::Result<()> {
    let mut files: Vec<(&str, &[u8])> = Vec::with_capacity(sidecars.len() + 1);
    for (name, bytes) in sidecars {
        files.push((name, bytes));
    }
    files.push(("world.obs", world));

    for (name, _) in &files {
        let target = dir.join(name);
        if target.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("save target {} is a directory", target.display()),
            ));
        }
    }

    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(files.len());
    for (name, bytes) in &files {
        let target = dir.join(name);
        let temporary = target.with_extension("tmp");
        if let Err(error) = write_staged(&temporary, bytes) {
            let _ = std::fs::remove_file(&temporary);
            for (temporary, _) in &staged {
                let _ = std::fs::remove_file(temporary);
            }
            return Err(error);
        }
        staged.push((temporary, target));
    }

    for (index, (temporary, target)) in staged.iter().enumerate() {
        if let Err(error) = std::fs::rename(temporary, target) {
            for (temporary, _) in &staged[index..] {
                let _ = std::fs::remove_file(temporary);
            }
            return Err(error);
        }
    }
    // Best effort: make the renames durable where the platform supports
    // syncing a directory handle. Individual files were already fsynced.
    if let Ok(directory) = std::fs::File::open(dir) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Writes one staged save file and flushes it to stable storage.
fn write_staged(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Upper bound on how long [`crate::Host::save_and_wait`] waits for a worker
/// that is already in flight before giving up. A timeout is reported instead of
/// starting a second writer over the same files.
pub(crate) const SAVE_WAIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Shared reference count is only asserted here to document that snapshots are
/// cheap and Send; the worker moves them by value.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<WorldSnapshot>>();
};

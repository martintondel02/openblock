//! World persistence: deterministic snapshot format v4 with zstd compression,
//! plus read compatibility with the v1, v2 and v3 snapshots written by earlier
//! builds.
//!
//! A v4 snapshot is a single little-endian byte stream:
//!
//! ```text
//! "OBSV" | version u16 = 4 | seed u64 | registry_fingerprint u64
//!        | compression u8 | uncompressed_payload_len u32
//!        | compressed_payload_len u32 | compressed_payload | crc32c u32
//! ```
//!
//! The checksum covers every preceding byte (header and compressed payload).
//! The payload is the chunk stream:
//!
//! ```text
//! chunk_count u32
//! chunk_count x (cx i32, cz i32, 24 x section)
//! ```
//!
//! A section stores its block-storage mode (uniform, 4/8/16-bit palette) and
//! its light. Light uses three encodings: flag 0 for a uniform-zero word, flag
//! 1 for a dense array of 4096 `u16` words, and flag 2 for a uniform non-zero
//! word followed by that `u16`. Uniform light therefore costs one or three
//! bytes instead of 8 KiB. Palette word arrays are still written verbatim;
//! compression removes their long zero runs.
//!
//! Version 3 added the registry fingerprint but did not know about the block
//! types appended by the crafting slice. Because block ids are append-only,
//! v1/v2 (which carry no fingerprint) and v3 saves whose fingerprint predates
//! an append are accepted: ids are interpreted unchanged and a v3 mismatch logs
//! a warning naming both fingerprints. v4 (and later) use **append-only prefix
//! acceptance**: the saved fingerprint is accepted when it equals today's
//! registry fingerprint or the fingerprint of one of its strict prefixes.
//! Appending blocks at the end keeps every saved id valid; removing or
//! reordering any saved block changes the saved prefix and is rejected with
//! [`SaveError::RegistryMismatch`]. The fingerprint covers the ordered block
//! registry only; appended item types do not change it.
//!
//! [`WorldSnapshot`] captures the loaded columns as shared [`Arc`]s so the host
//! can move encoding, compression and the staged write to a background worker
//! without copying the world or holding the simulation lock.
//!
//! Version 1 used the same chunk stream without compression, with light flag 1
//! always carrying a dense array even for a uniform non-zero value:
//!
//! ```text
//! "OBSV" | version u16 = 1 | seed u64 | chunk_count u32
//! chunks
//! crc32c u32
//! ```
//!
//! Chunks are always written sorted by `(cx, cz)`, so the encoding is
//! independent of hash-map iteration order and identical worlds produce
//! identical bytes.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ob_content::{BlockId, BlockType, Registry};
use ob_math::{ChunkPos, N_SECTIONS, SECTION_SIZE};
use rustc_hash::FxHashSet;
use zstd::decompress::zstd_decompress::{ZSTD_CONTENTSIZE_ERROR, ZSTD_CONTENTSIZE_UNKNOWN, ZSTD_getFrameContentSize};
use zstd::prelude::{
    ZSTD_CCtx, ZSTD_CStreamOutSize, ZSTD_compress, ZSTD_compressBound, ZSTD_compressStream, ZSTD_createCCtx,
    ZSTD_decompress, ZSTD_endStream, ZSTD_initCStream_srcSize, ZSTD_isError,
};

use crate::chunk::ChunkColumn;
use crate::light::Light;
use crate::section::{BLOCK_COUNT, Section, SectionStorage};
use crate::world::World;

/// Snapshot magic at the start of every byte stream.
const MAGIC: &[u8; 4] = b"OBSV";
/// Snapshot format version written by this build.
const VERSION: u16 = 4;

/// Current light semantics version written to the additive `world_meta.txt`.
///
/// Version 1 is every 1.0/1.1 save: per-section fake sky light with no vertical
/// occlusion. Version 2 is the 1.2 column-seeded engine; the directory loader
/// relights a world once when it sees a missing or older marker.
pub const LIGHT_VERSION: u32 = 2;
/// Bytes before the first chunk in a v1 stream: magic, version, seed, count.
const V1_HEADER_LEN: usize = 4 + 2 + 8 + 4;
/// Bytes before the compressed payload in a v2 stream.
const V2_HEADER_LEN: usize = 4 + 2 + 8 + 1 + 4 + 4;
/// Bytes before the compressed payload in a v3 stream (adds a registry fingerprint).
const V3_HEADER_LEN: usize = 4 + 2 + 8 + 8 + 1 + 4 + 4;
/// Bytes before the compressed payload in a v4 stream (same layout as v3).
const V4_HEADER_LEN: usize = V3_HEADER_LEN;
/// Bytes in the trailing checksum.
const CRC_LEN: usize = 4;
/// Payload is stored verbatim.
const COMPRESSION_NONE: u8 = 0;
/// Payload is a zstd frame (level 3).
const COMPRESSION_ZSTD: u8 = 1;
/// zstd compression level used for snapshots.
const ZSTD_LEVEL: i32 = 3;
/// Upper bound on a decompressed payload, to keep malformed headers from
/// requesting an unbounded allocation before the payload is parsed.
///
/// 512 MiB covers the v1.1 512-block client ceiling: a 65×65 generated world
/// measured 338 MiB uncompressed, while the previous 256 MiB limit rejected
/// every autosave of a full interest set. The loader still rejects anything
/// above this bound before allocating the body.
const MAX_PAYLOAD_LEN: usize = 512 * 1024 * 1024;
/// Largest world payload this build can read back.
///
/// This is the public half of the private loader cap. Writers must not
/// produce a snapshot whose uncompressed payload exceeds it: the loader
/// rejects such a file before allocating, so a save that crossed this limit
/// would be silently unloadable on the next start.
pub const MAX_SAVE_PAYLOAD_BYTES: usize = MAX_PAYLOAD_LEN;
/// Upper bound on the on-disk size of `world.obs` accepted by
/// [`read_save_file`].
///
/// The compressed body can be slightly larger than the decompressed payload for
/// incompressible data, so this is deliberately twice the decompressed cap.
pub const MAX_SAVE_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// Smallest possible on-disk chunk: position plus 24 uniform sections with
/// three storage bytes and one uniform-zero light byte each.
const MIN_CHUNK_BYTES: usize = 8 + N_SECTIONS * 4;
/// Smallest chunk coordinate whose block coordinates fit in an `i32`.
const MIN_CHUNK_COORD: i32 = i32::MIN / SECTION_SIZE as i32;
/// Largest chunk coordinate whose block coordinates fit in an `i32`.
const MAX_CHUNK_COORD: i32 = i32::MAX / SECTION_SIZE as i32;
/// Packed words in a 4-bit section (4096 indices at 16 per word).
const BITS4_WORDS: usize = 256;
/// Packed words in an 8-bit section (4096 indices at 8 per word).
const BITS8_WORDS: usize = 512;
/// Packed words in a 16-bit section (4096 indices at 4 per word).
const BITS16_WORDS: usize = 1024;

/// Reflected Castagnoli polynomial (CRC32C).
const CRC32C_POLYNOMIAL: u32 = 0x82F6_3B78;

/// Lookup table for the reflected CRC32C implementation.
const CRC32C_TABLE: [u32; 256] = build_crc32c_table();

/// Builds the reflected CRC32C lookup table at compile time.
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

/// Computes the CRC32C (Castagnoli) checksum of `bytes`.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[index];
    }
    !crc
}

/// Errors produced while saving or loading a world.
#[derive(Debug)]
pub enum SaveError {
    /// The byte stream is too short or structurally invalid.
    InvalidFormat(&'static str),
    /// The save format version is newer than this build understands.
    UnsupportedVersion(u16),
    /// The registry referenced by the save is missing a block type.
    UnknownBlock(u16),
    /// The save was written with a different content pack (block order/content).
    RegistryMismatch { saved: u64, current: u64 },
    /// A world payload would exceed the loader's uncompressed-size limit.
    PayloadTooLarge {
        /// Encoded payload length in bytes.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// An encoded snapshot would exceed the on-disk loader limit.
    SaveFileTooLarge {
        /// Encoded file length in bytes.
        len: u64,
        /// Configured maximum.
        max: u64,
    },
    /// A world or sidecar file could not be read or written.
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::InvalidFormat(reason) => write!(f, "invalid world save: {reason}"),
            SaveError::UnsupportedVersion(v) => write!(f, "unsupported save version {v}"),
            SaveError::UnknownBlock(id) => write!(f, "save references unknown block id {id}"),
            SaveError::RegistryMismatch { saved, current } => write!(
                f,
                "world was saved with a different content pack (registry fingerprint {saved:016x}, current {current:016x}); \
                 reordering or changing block types invalidates existing saves"
            ),
            SaveError::PayloadTooLarge { len, max } => write!(
                f,
                "world snapshot payload of {len} bytes exceeds the {max}-byte load limit"
            ),
            SaveError::SaveFileTooLarge { len, max } => {
                write!(f, "world snapshot of {len} bytes exceeds the {max}-byte file limit")
            }
            SaveError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// File name of the additive world metadata sidecar.
///
/// It lives next to `world.obs`, never inside it: the v4 save format stays
/// byte-compatible with 1.0/1.1, and a world without the sidecar is treated as
/// pre-1.2 and relit once.
pub const WORLD_META_FILE: &str = "world_meta.txt";

/// Additive world-level metadata stored in [`WORLD_META_FILE`].
///
/// Unknown keys and comments are preserved verbatim so later waves can append
/// fields without this loader dropping them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorldMeta {
    /// Light semantics version: 0/1 = 1.0/1.1 fake per-section sky light,
    /// 2 = the 1.2 column-seeded engine (see [`LIGHT_VERSION`]).
    pub light_version: u32,
    /// Lines this build does not interpret, preserved when rewriting.
    pub extra_lines: Vec<String>,
}

/// Result of the one-time directory-level light migration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LightMigration {
    /// Whether the loaded world was relit.
    pub relit: bool,
    /// Whether `world.obs` was rewritten with the new light.
    pub save_rewritten: bool,
    /// Whether `world_meta.txt` was written or upgraded.
    pub meta_written: bool,
}

/// Reads the additive world metadata, if present.
///
/// A missing file is `Ok(None)` (pre-1.2 world). A present file without a
/// `light_version` key is version 0. Unknown keys are kept in
/// [`WorldMeta::extra_lines`].
pub fn read_world_meta(dir: &Path) -> Result<Option<WorldMeta>, SaveError> {
    let path = dir.join(WORLD_META_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SaveError::Io { path, source }),
    };
    let mut meta = WorldMeta::default();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("light_version=") {
            meta.light_version = value.trim().parse().unwrap_or(0);
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            meta.extra_lines.push(line.to_owned());
        }
    }
    Ok(Some(meta))
}

/// Writes the additive world metadata atomically.
pub fn write_world_meta(dir: &Path, meta: &WorldMeta) -> Result<(), SaveError> {
    std::fs::create_dir_all(dir).map_err(|source| SaveError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut text = String::from("# OpenBlock world metadata (additive; safe to delete to force a light relight)\n");
    text.push_str(&format!("light_version={}\n", meta.light_version));
    for line in &meta.extra_lines {
        text.push_str(line);
        text.push('\n');
    }
    write_file_atomic(&dir.join(WORLD_META_FILE), text.as_bytes()).map_err(|source| SaveError::Io {
        path: dir.join(WORLD_META_FILE),
        source,
    })
}

/// Writes bytes to `path` through a sibling temporary file and an atomic rename.
fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("world");
    let temp = path.with_file_name(format!(".{file_name}.tmp"));
    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)
}

/// Loads a world directory, applying the one-time 1.2 light migration.
///
/// A missing or legacy `light_version` marker relights every resident column,
/// rewrites `world.obs` with the corrected light (still format v4) and writes
/// `world_meta.txt` with `light_version=2`. A second load sees the marker and
/// does not relight. A raw [`from_bytes`] caller bypasses this policy and sees
/// [`World::light_version`](crate::World::light_version) == 1.
pub fn load_world_dir(registry: Arc<Registry>, dir: &Path) -> Result<World, SaveError> {
    load_world_dir_with_report(registry, dir).map(|(world, _)| world)
}

/// [`load_world_dir`] with a report of what the migration did (tests/tools).
pub fn load_world_dir_with_report(registry: Arc<Registry>, dir: &Path) -> Result<(World, LightMigration), SaveError> {
    let save_path = dir.join("world.obs");
    let bytes = read_save_file(&save_path).map_err(|source| SaveError::Io {
        path: save_path.clone(),
        source,
    })?;
    let mut world = from_bytes(registry, &bytes)?;
    let meta = read_world_meta(dir)?;
    let mut migration = LightMigration::default();
    let needs_relight = meta.as_ref().is_none_or(|meta| meta.light_version < LIGHT_VERSION);
    if needs_relight {
        world.relight_all();
        let relit = to_bytes_checked(&world)?;
        write_file_atomic(&save_path, &relit).map_err(|source| SaveError::Io {
            path: save_path,
            source,
        })?;
        migration.save_rewritten = true;
        let mut meta = meta.unwrap_or_default();
        meta.light_version = LIGHT_VERSION;
        write_world_meta(dir, &meta)?;
        migration.meta_written = true;
        migration.relit = true;
    } else {
        world.set_light_version(LIGHT_VERSION);
    }
    Ok((world, migration))
}

/// Saves a world directory (v4 snapshot plus the additive light marker).
///
/// New 1.2 worlds call this and therefore write `light_version=2` immediately,
/// so they never pay a relight on the next load. Existing sidecar keys survive.
pub fn save_world_dir(world: &World, dir: &Path) -> Result<(), SaveError> {
    let bytes = to_bytes_checked(world)?;
    let save_path = dir.join("world.obs");
    write_file_atomic(&save_path, &bytes).map_err(|source| SaveError::Io {
        path: save_path,
        source,
    })?;
    let mut meta = read_world_meta(dir)?.unwrap_or_default();
    meta.light_version = world.light_version();
    write_world_meta(dir, &meta)
}

/// Bookkeeping type kept for source compatibility with the v0.0.1 world API.
///
/// The snapshot encoder and decoder are free functions; this marker carries no
/// state yet.
#[derive(Debug, Clone, Default)]
pub struct WorldSave;

/// Immutable, cheaply shareable view of the loaded world for a background save.
///
/// Captured by [`World::snapshot`](crate::World::snapshot) under the simulation
/// lock in O(columns) pointer copies: the columns are shared through [`Arc`],
/// not cloned. Encoding the snapshot on a worker thread therefore does not
/// block the tick thread and does not duplicate the world's block data. A later
/// edit copy-on-writes only the column it touches; the snapshot keeps the
/// pre-edit version, which is exactly the consistency a save needs.
///
/// The chunk order is fixed at capture time, so encoding the same snapshot
/// twice produces identical bytes. The v4 stream produced here is the same
/// uncompressed chunk payload as [`to_bytes`]; only the zstd encoder is driven
/// incrementally so the 300+ MiB uncompressed buffer never has to exist. The
/// compressed bytes are a deterministic function of the payload but are not
/// guaranteed to be byte-identical to the one-shot encoder's output.
#[derive(Debug, Clone)]
pub struct WorldSnapshot {
    seed: u64,
    registry_fingerprint: u64,
    chunks: Vec<Arc<ChunkColumn>>,
}

impl WorldSnapshot {
    /// Builds a snapshot from already sorted, shared columns.
    pub(crate) fn new(seed: u64, registry: &Registry, chunks: Vec<Arc<ChunkColumn>>) -> Self {
        Self {
            seed,
            registry_fingerprint: registry_fingerprint(registry),
            chunks,
        }
    }

    /// The persisted world seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The registry fingerprint recorded when the snapshot was captured.
    pub fn registry_fingerprint(&self) -> u64 {
        self.registry_fingerprint
    }

    /// Number of captured columns.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Bytes the chunk payload would occupy before compression.
    ///
    /// This is computed by walking the sections, so the streaming encoder can
    /// declare the payload length in the v4 header without materialising the
    /// payload. It is tested to equal the length of the buffered encoder.
    pub fn payload_len(&self) -> Result<usize, SaveError> {
        let mut len = 4usize; // chunk_count
        for chunk in &self.chunks {
            len = len.checked_add(8).ok_or(SaveError::PayloadTooLarge {
                len: usize::MAX,
                max: MAX_SAVE_PAYLOAD_BYTES,
            })?;
            for (_, section) in chunk.sections() {
                len = len
                    .checked_add(section_encoded_len(section))
                    .ok_or(SaveError::PayloadTooLarge {
                        len: usize::MAX,
                        max: MAX_SAVE_PAYLOAD_BYTES,
                    })?;
            }
        }
        Ok(len)
    }

    /// Serializes the captured world to the v4 snapshot format.
    ///
    /// The uncompressed chunk stream is fed straight into a streaming zstd
    /// encoder, so peak memory is the compressed body plus a small scratch
    /// buffer instead of the whole payload. If the streaming encoder reports an
    /// error the encoder falls back to the buffered path, preserving the
    /// historical output at the cost of the transient payload allocation.
    pub fn to_bytes_checked(&self) -> Result<Vec<u8>, SaveError> {
        let payload_len = self.payload_len()?;
        if payload_len > MAX_SAVE_PAYLOAD_BYTES {
            return Err(SaveError::PayloadTooLarge {
                len: payload_len,
                max: MAX_SAVE_PAYLOAD_BYTES,
            });
        }
        match compress_snapshot_streaming(self, payload_len) {
            Ok(body) => finish_v4(
                self.seed,
                self.registry_fingerprint,
                payload_len,
                COMPRESSION_ZSTD,
                body,
                MAX_SAVE_PAYLOAD_BYTES,
                MAX_SAVE_FILE_BYTES,
            ),
            Err(()) => {
                // Streaming is an optimisation, not a format requirement. Keep
                // the save working if the codec rejects incremental input.
                let payload = encode_snapshot_payload(self);
                if payload.len() != payload_len {
                    return Err(SaveError::InvalidFormat("streaming payload length mismatch"));
                }
                let (compression, body) = compress_payload(&payload);
                finish_v4(
                    self.seed,
                    self.registry_fingerprint,
                    payload_len,
                    compression,
                    body,
                    MAX_SAVE_PAYLOAD_BYTES,
                    MAX_SAVE_FILE_BYTES,
                )
            }
        }
    }
}

/// Serializes a world to the compressed v4 snapshot format.
///
/// Chunks are emitted in ascending `(cx, cz)` order so repeated saves of the
/// same world produce identical bytes. The payload is compressed with zstd
/// level 3. The trailing CRC32C covers the header and compressed payload, and
/// is written last (not included in its own checksum).
///
/// This convenience wrapper is for callers that already know the world fits.
/// Use [`to_bytes_checked`] in a save path: it enforces the same payload and
/// file limits the loader applies, so a successful save is always reloadable by
/// this build. It panics only when those limits are exceeded.
pub fn to_bytes(world: &World) -> Vec<u8> {
    to_bytes_checked(world).unwrap_or_else(|error| panic!("world snapshot cannot be encoded: {error}"))
}

/// Serializes a world and enforces the loader's payload and on-disk limits.
///
/// Returns [`SaveError::PayloadTooLarge`] when the uncompressed chunk payload
/// exceeds [`MAX_SAVE_PAYLOAD_BYTES`] and [`SaveError::SaveFileTooLarge`] when
/// the compressed snapshot exceeds [`MAX_SAVE_FILE_BYTES`]. This is what keeps a
/// long-running server with resident chunks from writing a `world.obs` that the
/// next start (or the next version) cannot open.
pub fn to_bytes_checked(world: &World) -> Result<Vec<u8>, SaveError> {
    encode_v4(world, MAX_SAVE_PAYLOAD_BYTES, MAX_SAVE_FILE_BYTES)
}

/// Encodes a v4 snapshot with explicit limits (unit-testable).
fn encode_v4(world: &World, max_payload: usize, max_file: u64) -> Result<Vec<u8>, SaveError> {
    let payload = encode_payload(world, write_section);
    if payload.len() > max_payload {
        return Err(SaveError::PayloadTooLarge {
            len: payload.len(),
            max: max_payload,
        });
    }
    let (compression, body) = compress_payload(&payload);
    finish_v4(
        world.seed(),
        registry_fingerprint(world.registry()),
        payload.len(),
        compression,
        body,
        max_payload,
        max_file,
    )
}

/// Assembles the v4 header, compressed body and trailing CRC32C.
#[allow(clippy::too_many_arguments)]
fn finish_v4(
    seed: u64,
    registry: u64,
    payload_len: usize,
    compression: u8,
    body: Vec<u8>,
    max_payload: usize,
    max_file: u64,
) -> Result<Vec<u8>, SaveError> {
    let payload_len_u32 = u32::try_from(payload_len).map_err(|_| SaveError::PayloadTooLarge {
        len: payload_len,
        max: max_payload,
    })?;
    let body_len = u32::try_from(body.len()).map_err(|_| SaveError::SaveFileTooLarge {
        len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        max: max_file,
    })?;
    let file_len = (V4_HEADER_LEN + body.len() + CRC_LEN) as u64;
    if file_len > max_file {
        return Err(SaveError::SaveFileTooLarge {
            len: file_len,
            max: max_file,
        });
    }

    let mut out = Vec::with_capacity(V4_HEADER_LEN + body.len() + CRC_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&seed.to_le_bytes());
    out.extend_from_slice(&registry.to_le_bytes());
    out.push(compression);
    out.extend_from_slice(&payload_len_u32.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&body);

    let checksum = crc32c(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

/// Loads a world from a v1, v2, v3 or v4 snapshot.
///
/// v1/v2 carry no registry fingerprint, and a v3 save whose fingerprint
/// differs is still accepted: the append-only block-id invariant makes its ids
/// valid and a warning names both fingerprints. v4 (the format this build
/// writes) keeps the strict fingerprint check.
///
/// The whole stream is validated before a [`World`] is returned: header,
/// version, checksum, payload lengths, compression method, palette bounds
/// against `registry`, section word counts, light encoding, chunk counts, and
/// the absence of trailing bytes. Malformed input yields a [`SaveError`] and
/// never panics.
pub fn from_bytes(registry: Arc<Registry>, bytes: &[u8]) -> Result<World, SaveError> {
    if !bytes.starts_with(MAGIC) {
        return Err(SaveError::InvalidFormat("wrong magic"));
    }
    if bytes.len() < 6 {
        return Err(SaveError::InvalidFormat("truncated header"));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version > VERSION {
        return Err(SaveError::UnsupportedVersion(version));
    }
    match version {
        1 => load_v1(registry, bytes),
        2 => load_v2(registry, bytes),
        3 => load_v3(registry, bytes),
        4 => load_v4(registry, bytes),
        _ => Err(SaveError::InvalidFormat("unknown format version")),
    }
}

/// Reads a `world.obs` file with the on-disk size cap enforced before and
/// during the read.
///
/// The metadata check rejects an oversized file without reading it; the
/// `take` limit still bounds the read if the file grows between the metadata
/// call and the read (or is a special file that reports no length). A file above
/// [`MAX_SAVE_FILE_BYTES`] is reported as
/// [`io::ErrorKind::InvalidData`]; callers can then keep the previous world or
/// refuse to start without allocating attacker-chosen memory.
pub fn read_save_file(path: &Path) -> Result<Vec<u8>, io::Error> {
    // Check the path *before* opening it: opening a FIFO or device can block
    // indefinitely, and a downloaded world directory is untrusted input. A
    // second check on the opened handle narrows the metadata/open race.
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("world save {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_SAVE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("world save file exceeds the {MAX_SAVE_FILE_BYTES}-byte limit"),
        ));
    }
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("world save {} is not a regular file", path.display()),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SAVE_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SAVE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("world save file exceeds the {MAX_SAVE_FILE_BYTES}-byte limit"),
        ));
    }
    Ok(bytes)
}

/// Encodes the chunk stream (chunk count, coordinates and sections).
///
/// `write_section` is a parameter so tests can also produce the legacy v1
/// light encoding from the same world data.
fn encode_payload(world: &World, write_section: fn(&mut Vec<u8>, &Section)) -> Vec<u8> {
    let mut chunks: Vec<&ChunkColumn> = world.chunks().map(|(_, chunk)| chunk).collect();
    chunks.sort_by_key(|chunk| (chunk.pos().x, chunk.pos().z));

    let mut out = Vec::new();
    let chunk_count = u32::try_from(chunks.len()).expect("world snapshot has more than 4 billion chunks");
    out.extend_from_slice(&chunk_count.to_le_bytes());

    for chunk in chunks {
        let pos = chunk.pos();
        out.extend_from_slice(&pos.x.to_le_bytes());
        out.extend_from_slice(&pos.z.to_le_bytes());
        for (_, section) in chunk.sections() {
            write_section(&mut out, section);
        }
    }
    out
}

/// Encodes the chunk stream of a captured snapshot.
///
/// Only used by the streaming encoder's fallback and by unit tests; the normal
/// background-save path never materialises this payload.
fn encode_snapshot_payload(snapshot: &WorldSnapshot) -> Vec<u8> {
    let mut out = Vec::new();
    let chunk_count = u32::try_from(snapshot.chunks.len()).expect("world snapshot has more than 4 billion chunks");
    out.extend_from_slice(&chunk_count.to_le_bytes());
    for chunk in &snapshot.chunks {
        let pos = chunk.pos();
        out.extend_from_slice(&pos.x.to_le_bytes());
        out.extend_from_slice(&pos.z.to_le_bytes());
        for (_, section) in chunk.sections() {
            write_section(&mut out, section);
        }
    }
    out
}

/// Exact number of payload bytes [`write_section`] emits for one section.
fn section_encoded_len(section: &Section) -> usize {
    let storage = match section.storage() {
        SectionStorage::Uniform(_) => 3,
        SectionStorage::Bits4 { palette, .. } => 3 + 2 * palette.len().min(u16::MAX as usize) + 8 * BITS4_WORDS,
        SectionStorage::Bits8 { palette, .. } => 3 + 2 * palette.len().min(u16::MAX as usize) + 8 * BITS8_WORDS,
        SectionStorage::Bits16 { palette, .. } => 3 + 2 * palette.len().min(u16::MAX as usize) + 8 * BITS16_WORDS,
    };
    let light = match section.light().uniform_value() {
        Some(0) => 1,
        Some(_) => 3,
        None => 1 + BLOCK_COUNT * 2,
    };
    storage + light
}

/// Streaming zstd encoder state.
struct StreamingCompressor {
    context: Box<ZSTD_CCtx>,
    output: Vec<u8>,
    buffer: Vec<u8>,
}

impl StreamingCompressor {
    /// Starts a zstd level-3 frame that declares `payload_len` as its content
    /// size, so the loader's frame-size check can reject truncation early.
    fn new(payload_len: usize) -> Option<Self> {
        let mut context = ZSTD_createCCtx()?;
        let result = ZSTD_initCStream_srcSize(&mut context, ZSTD_LEVEL, payload_len as u64);
        if ZSTD_isError(result) {
            return None;
        }
        Some(Self {
            context,
            output: Vec::new(),
            buffer: vec![0u8; ZSTD_CStreamOutSize().max(32)],
        })
    }

    /// Feeds one section-sized input chunk and appends all produced bytes.
    fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let mut input_pos = 0usize;
        while input_pos < bytes.len() {
            let mut output_pos = 0usize;
            let result = ZSTD_compressStream(
                &mut self.context,
                &mut self.buffer,
                &mut output_pos,
                bytes,
                &mut input_pos,
            );
            if ZSTD_isError(result) {
                return Err(());
            }
            self.output.extend_from_slice(&self.buffer[..output_pos]);
            if output_pos == 0 && input_pos < bytes.len() && result == 0 {
                // Defensive: the codec should consume input or produce output.
                // Avoid spinning forever if it reports no progress.
                return Err(());
            }
        }
        Ok(())
    }

    /// Ends the frame and returns the compressed body.
    fn finish(mut self) -> Result<Vec<u8>, ()> {
        loop {
            let mut output_pos = 0usize;
            let result = ZSTD_endStream(&mut self.context, &mut self.buffer, &mut output_pos);
            if ZSTD_isError(result) {
                return Err(());
            }
            self.output.extend_from_slice(&self.buffer[..output_pos]);
            if result == 0 {
                return Ok(self.output);
            }
            if output_pos == 0 {
                // No progress and the frame is not finished: report an encoder
                // failure so the snapshot falls back to the buffered path
                // instead of spinning forever.
                return Err(());
            }
        }
    }
}

/// Compresses a snapshot section by section, never holding the full payload.
fn compress_snapshot_streaming(snapshot: &WorldSnapshot, payload_len: usize) -> Result<Vec<u8>, ()> {
    let mut compressor = StreamingCompressor::new(payload_len).ok_or(())?;
    let chunk_count = u32::try_from(snapshot.chunks.len()).map_err(|_| ())?;
    compressor.write(&chunk_count.to_le_bytes())?;
    let mut scratch = Vec::new();
    for chunk in &snapshot.chunks {
        let pos = chunk.pos();
        compressor.write(&pos.x.to_le_bytes())?;
        compressor.write(&pos.z.to_le_bytes())?;
        for (_, section) in chunk.sections() {
            scratch.clear();
            write_section(&mut scratch, section);
            compressor.write(&scratch)?;
        }
    }
    compressor.finish()
}

/// Compresses a payload with zstd level 3.
///
/// Returns the compression flag and the bytes to store. If the pure-Rust zstd
/// codec reports an error, the payload is stored verbatim (flag 0) instead of
/// making the save fail.
fn compress_payload(payload: &[u8]) -> (u8, Vec<u8>) {
    let bound = ZSTD_compressBound(payload.len());
    if bound == 0 && !payload.is_empty() {
        return (COMPRESSION_NONE, payload.to_vec());
    }
    let mut compressed = vec![0u8; bound];
    let written = ZSTD_compress(&mut compressed, payload, ZSTD_LEVEL);
    if ZSTD_isError(written) || written > compressed.len() {
        return (COMPRESSION_NONE, payload.to_vec());
    }
    compressed.truncate(written);
    (COMPRESSION_ZSTD, compressed)
}

/// Decompresses a v2 payload and validates the declared length.
fn decode_payload(compression: u8, uncompressed_len: usize, body: &[u8]) -> Result<Vec<u8>, SaveError> {
    match compression {
        COMPRESSION_NONE => {
            if body.len() != uncompressed_len {
                return Err(SaveError::InvalidFormat("uncompressed payload length mismatch"));
            }
            Ok(body.to_vec())
        }
        COMPRESSION_ZSTD => {
            if uncompressed_len > MAX_PAYLOAD_LEN {
                return Err(SaveError::InvalidFormat("uncompressed payload too large"));
            }
            // The zstd frame header carries the decompressed size. Reject a
            // header/body disagreement before allocating: a tiny corrupt file
            // that claims a huge payload must not cause a large allocation.
            let frame_size = ZSTD_getFrameContentSize(body);
            if frame_size == ZSTD_CONTENTSIZE_ERROR {
                return Err(SaveError::InvalidFormat("zstd frame header is invalid"));
            }
            if frame_size != ZSTD_CONTENTSIZE_UNKNOWN && frame_size != uncompressed_len as u64 {
                return Err(SaveError::InvalidFormat("zstd frame content size mismatch"));
            }
            let mut payload = vec![0u8; uncompressed_len];
            let written = ZSTD_decompress(&mut payload, body);
            if ZSTD_isError(written) {
                return Err(SaveError::InvalidFormat("zstd decompression failed"));
            }
            if written != uncompressed_len {
                return Err(SaveError::InvalidFormat("decompressed payload length mismatch"));
            }
            Ok(payload)
        }
        _ => Err(SaveError::InvalidFormat("unknown compression method")),
    }
}

/// Loads the old uncompressed v1 snapshot layout.
fn load_v1(registry: Arc<Registry>, bytes: &[u8]) -> Result<World, SaveError> {
    if bytes.len() < V1_HEADER_LEN + CRC_LEN {
        return Err(SaveError::InvalidFormat("truncated header"));
    }
    let payload_len = bytes.len() - CRC_LEN;
    let stored_crc = u32::from_le_bytes([
        bytes[payload_len],
        bytes[payload_len + 1],
        bytes[payload_len + 2],
        bytes[payload_len + 3],
    ]);
    if crc32c(&bytes[..payload_len]) != stored_crc {
        return Err(SaveError::InvalidFormat("checksum mismatch"));
    }

    let mut reader = Reader::new(&bytes[..payload_len]);
    let magic = reader.read_exact::<4>()?;
    if &magic != MAGIC {
        return Err(SaveError::InvalidFormat("wrong magic"));
    }
    if reader.read_u16()? != 1 {
        return Err(SaveError::InvalidFormat("unknown format version"));
    }
    let seed = reader.read_u64()?;
    read_payload(&mut reader, registry, seed, LightEncoding::V1)
}

/// Loads the compressed v2 snapshot layout.
fn load_v2(registry: Arc<Registry>, bytes: &[u8]) -> Result<World, SaveError> {
    if bytes.len() < V2_HEADER_LEN + CRC_LEN {
        return Err(SaveError::InvalidFormat("truncated header"));
    }
    let checksum_offset = bytes.len() - CRC_LEN;
    let stored_crc = u32::from_le_bytes([
        bytes[checksum_offset],
        bytes[checksum_offset + 1],
        bytes[checksum_offset + 2],
        bytes[checksum_offset + 3],
    ]);
    if crc32c(&bytes[..checksum_offset]) != stored_crc {
        return Err(SaveError::InvalidFormat("checksum mismatch"));
    }

    let compression = bytes[14];
    let uncompressed_len = u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]);
    let compressed_len = u32::from_le_bytes([bytes[19], bytes[20], bytes[21], bytes[22]]);
    let compressed_len = usize::try_from(compressed_len).map_err(|_| SaveError::InvalidFormat("payload too large"))?;
    let payload_end = V2_HEADER_LEN
        .checked_add(compressed_len)
        .ok_or(SaveError::InvalidFormat("payload length overflow"))?;
    if payload_end > checksum_offset {
        return Err(SaveError::InvalidFormat("truncated payload"));
    }
    if payload_end < checksum_offset {
        return Err(SaveError::InvalidFormat("trailing bytes after payload"));
    }

    let uncompressed_len =
        usize::try_from(uncompressed_len).map_err(|_| SaveError::InvalidFormat("payload too large"))?;
    let body = &bytes[V2_HEADER_LEN..payload_end];
    let payload = decode_payload(compression, uncompressed_len, body)?;

    let seed = u64::from_le_bytes([
        bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13],
    ]);
    let mut reader = Reader::new(&payload);
    read_payload(&mut reader, registry, seed, LightEncoding::V2)
}

/// Loads the compressed v3 snapshot layout.
///
/// v3 predates the crafting-slice block additions, so a fingerprint mismatch
/// is a warning rather than an error; see the module docs for the append-only
/// invariant.
fn load_v3(registry: Arc<Registry>, bytes: &[u8]) -> Result<World, SaveError> {
    load_compressed(registry, bytes, false)
}

/// Loads the compressed v4 snapshot layout with the strict fingerprint check.
fn load_v4(registry: Arc<Registry>, bytes: &[u8]) -> Result<World, SaveError> {
    load_compressed(registry, bytes, true)
}

/// Shared v3/v4 decoder (the headers are byte-identical).
fn load_compressed(registry: Arc<Registry>, bytes: &[u8], strict_fingerprint: bool) -> Result<World, SaveError> {
    if bytes.len() < V4_HEADER_LEN + CRC_LEN {
        return Err(SaveError::InvalidFormat("truncated header"));
    }
    let checksum_offset = bytes.len() - CRC_LEN;
    let stored_crc = u32::from_le_bytes([
        bytes[checksum_offset],
        bytes[checksum_offset + 1],
        bytes[checksum_offset + 2],
        bytes[checksum_offset + 3],
    ]);
    if crc32c(&bytes[..checksum_offset]) != stored_crc {
        return Err(SaveError::InvalidFormat("checksum mismatch"));
    }
    let seed = u64::from_le_bytes([
        bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13],
    ]);
    let saved_registry = u64::from_le_bytes([
        bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
    ]);
    let current_registry = registry_fingerprint(&registry);
    if saved_registry != current_registry {
        if !registry_is_append_only_prefix(saved_registry, &registry) {
            return Err(SaveError::RegistryMismatch {
                saved: saved_registry,
                current: current_registry,
            });
        }
        // The saved registry is a prefix of the current one: every id it uses
        // keeps its old meaning and the appended block ids were never present
        // in the save. This is the 1.0.x -> 1.1/1.2 compatibility path.
        let message = format!(
            "world snapshot registry fingerprint {saved_registry:016x} predates appended block \
             types (current {current_registry:016x}); loading its ids unchanged"
        );
        if strict_fingerprint {
            log::info!("{message}");
        } else {
            log::warn!("{message}");
        }
    }
    let compression = bytes[22];
    let uncompressed_len = u32::from_le_bytes([bytes[23], bytes[24], bytes[25], bytes[26]]);
    let compressed_len = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], bytes[30]]);
    let compressed_len = usize::try_from(compressed_len).map_err(|_| SaveError::InvalidFormat("payload too large"))?;
    let payload_end = V4_HEADER_LEN
        .checked_add(compressed_len)
        .ok_or(SaveError::InvalidFormat("payload length overflow"))?;
    if payload_end > checksum_offset {
        return Err(SaveError::InvalidFormat("truncated payload"));
    }
    if payload_end < checksum_offset {
        return Err(SaveError::InvalidFormat("trailing bytes after payload"));
    }
    let body = &bytes[V4_HEADER_LEN..payload_end];
    let payload = decode_payload(
        compression,
        usize::try_from(uncompressed_len).map_err(|_| SaveError::InvalidFormat("payload too large"))?,
        body,
    )?;
    let mut reader = Reader::new(&payload);
    read_payload(&mut reader, registry, seed, LightEncoding::V2)
}

/// Stable content-pack fingerprint: FNV-1a over the block count and ids in order.
pub fn registry_fingerprint(registry: &Registry) -> u64 {
    let blocks: Vec<&BlockType> = registry.iter().map(|(_, block)| block).collect();
    fingerprint_blocks(&blocks, blocks.len())
}

/// FNV-1a over `count` followed by the first `count` block ids.
///
/// The registry fingerprint covers the ordered *block* registry; appending
/// items does not change it. A save written before a content append records the
/// fingerprint of a strict prefix of today's registry.
fn fingerprint_blocks(blocks: &[&BlockType], count: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for byte in (count as u32).to_le_bytes() {
        mix(byte);
    }
    for block in blocks.iter().take(count) {
        for byte in block.id.as_bytes() {
            mix(*byte);
        }
        mix(0xff);
    }
    hash
}

/// Whether `saved` is the fingerprint of a strict prefix of `registry`.
///
/// v4 stores only the fingerprint, not the registry length, so append-only
/// acceptance compares the saved value with the fingerprint of every prefix of
/// the current registry. A removal or reorder changes the bytes of the saved
/// prefix and no longer matches; an appended block only adds a new, longer
/// prefix. The comparison is a 64-bit hash test (collisions are negligible) and
/// block ids beyond the current registry are still rejected later by
/// [`validate_block`]. Realistic registries have tens to hundreds of blocks, so
/// the O(n²) byte walk is far below the save's I/O cost.
fn registry_is_append_only_prefix(saved: u64, registry: &Registry) -> bool {
    let blocks: Vec<&BlockType> = registry.iter().map(|(_, block)| block).collect();
    if saved == fingerprint_blocks(&blocks, blocks.len()) {
        return true;
    }
    (0..blocks.len()).any(|count| fingerprint_blocks(&blocks, count) == saved)
}

fn read_payload(
    reader: &mut Reader<'_>,
    registry: Arc<Registry>,
    seed: u64,
    light_encoding: LightEncoding,
) -> Result<World, SaveError> {
    let chunk_count = reader.read_u32()?;
    let minimum_payload = u64::from(chunk_count) * MIN_CHUNK_BYTES as u64;
    if minimum_payload > reader.remaining() as u64 {
        return Err(SaveError::InvalidFormat("chunk count exceeds payload"));
    }

    let mut world = World::new(Arc::clone(&registry), seed);
    let mut seen: FxHashSet<ChunkPos> = FxHashSet::default();
    for _ in 0..chunk_count {
        let x = reader.read_i32()?;
        let z = reader.read_i32()?;
        if !(MIN_CHUNK_COORD..=MAX_CHUNK_COORD).contains(&x) || !(MIN_CHUNK_COORD..=MAX_CHUNK_COORD).contains(&z) {
            return Err(SaveError::InvalidFormat("chunk position out of range"));
        }
        let pos = ChunkPos::new(x, z);
        if !seen.insert(pos) {
            return Err(SaveError::InvalidFormat("duplicate chunk position"));
        }

        let mut sections = Vec::with_capacity(N_SECTIONS);
        for _ in 0..N_SECTIONS {
            sections.push(read_section(reader, &registry, light_encoding)?);
        }
        world.insert_loaded_chunk(ChunkColumn::from_sections(pos, sections));
    }

    if reader.remaining() != 0 {
        return Err(SaveError::InvalidFormat("trailing bytes after chunks"));
    }
    // A raw v4 stream carries no light-version marker. The directory loader
    // consults world_meta.txt and relights when it is absent or below 2; a
    // direct from_bytes caller sees version 1 (legacy) so it does not silently
    // trust 1.0/1.1 fake sky light.
    world.set_light_version(1);
    Ok(world)
}

/// Which light encoding a version's sections use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LightEncoding {
    /// v1: flag 0 = uniform zero, flag 1 = dense array.
    V1,
    /// v2: flag 0 = uniform zero, flag 1 = dense array, flag 2 = uniform word.
    V2,
}

/// Writes one section's storage mode, palette/words, and light.
fn write_section(out: &mut Vec<u8>, section: &Section) {
    write_storage(out, section);

    match section.light().uniform_value() {
        Some(0) => out.push(0),
        Some(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => {
            out.push(1);
            let data = section
                .light()
                .raw()
                .expect("non-uniform light always has a dense array");
            for word in data.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// Writes one section's block-storage body.
fn write_storage(out: &mut Vec<u8>, section: &Section) {
    match section.storage() {
        SectionStorage::Uniform(id) => {
            out.push(0);
            out.extend_from_slice(&id.0.to_le_bytes());
        }
        SectionStorage::Bits4 { .. } => {
            write_paletted(out, 1, section.palette(), section.words(), BITS4_WORDS);
        }
        SectionStorage::Bits8 { .. } => {
            write_paletted(out, 2, section.palette(), section.words(), BITS8_WORDS);
        }
        SectionStorage::Bits16 { .. } => {
            write_paletted(out, 3, section.palette(), section.words(), BITS16_WORDS);
        }
    }
}

/// Writes a section using the v1 light encoding (dense arrays for every
/// non-zero container). Test-only, used to construct v1 compatibility fixtures.
#[cfg(test)]
fn write_legacy_section(out: &mut Vec<u8>, section: &Section) {
    write_storage(out, section);

    match section.light().uniform_value() {
        Some(0) => out.push(0),
        Some(value) => {
            out.push(1);
            for _ in 0..BLOCK_COUNT {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        None => {
            out.push(1);
            let data = section
                .light()
                .raw()
                .expect("non-uniform light always has a dense array");
            for word in data.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// Writes a paletted section body. Missing words are zero-padded so the
/// on-disk word count always matches the storage mode.
fn write_paletted(out: &mut Vec<u8>, mode: u8, palette: &[BlockId], words: &[u64], required_words: usize) {
    let palette_len = u16::try_from(palette.len()).unwrap_or(u16::MAX);
    out.push(mode);
    out.extend_from_slice(&palette_len.to_le_bytes());
    for id in palette.iter().take(usize::from(palette_len)) {
        out.extend_from_slice(&id.0.to_le_bytes());
    }
    for index in 0..required_words {
        let word = words.get(index).copied().unwrap_or(0);
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Reads one section, validating block ids and palette indices.
fn read_section(
    reader: &mut Reader<'_>,
    registry: &Registry,
    light_encoding: LightEncoding,
) -> Result<Section, SaveError> {
    let mode = reader.read_u8()?;
    let storage = match mode {
        0 => {
            let id = BlockId(reader.read_u16()?);
            validate_block(registry, id)?;
            SectionStorage::Uniform(id)
        }
        1 => read_paletted(reader, registry, 4, 16, BITS4_WORDS)?,
        2 => read_paletted(reader, registry, 8, 256, BITS8_WORDS)?,
        3 => read_paletted(reader, registry, 16, u16::MAX as usize, BITS16_WORDS)?,
        _ => return Err(SaveError::InvalidFormat("unknown storage mode")),
    };
    let light = match light_encoding {
        LightEncoding::V1 => read_light_v1(reader)?,
        LightEncoding::V2 => read_light_v2(reader)?,
    };
    Ok(Section::from_parts(storage, light))
}

/// Reads a paletted storage body and checks every palette index.
fn read_paletted(
    reader: &mut Reader<'_>,
    registry: &Registry,
    bits: u8,
    palette_capacity: usize,
    word_count: usize,
) -> Result<SectionStorage, SaveError> {
    let palette_len = usize::from(reader.read_u16()?);
    if palette_len == 0 {
        return Err(SaveError::InvalidFormat("empty palette"));
    }
    if palette_len > palette_capacity {
        return Err(SaveError::InvalidFormat("palette too large"));
    }
    let needed = palette_len * 2 + word_count * 8;
    if reader.remaining() < needed {
        return Err(SaveError::InvalidFormat("truncated section"));
    }

    let mut palette = Vec::with_capacity(palette_len);
    for _ in 0..palette_len {
        let id = BlockId(reader.read_u16()?);
        validate_block(registry, id)?;
        palette.push(id);
    }

    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        words.push(reader.read_u64()?);
    }
    validate_indices(bits, &words, palette_len)?;

    let storage = match bits {
        4 => SectionStorage::Bits4 { words, palette },
        8 => SectionStorage::Bits8 { words, palette },
        16 => SectionStorage::Bits16 { words, palette },
        _ => return Err(SaveError::InvalidFormat("unknown storage mode")),
    };
    Ok(storage)
}

/// Checks that every packed palette index addresses a palette entry.
fn validate_indices(bits: u8, words: &[u64], palette_len: usize) -> Result<(), SaveError> {
    let per_word = 64 / usize::from(bits);
    let mask = (1u64 << bits) - 1;
    for &word in words {
        for slot in 0..per_word {
            let index = ((word >> (slot * usize::from(bits))) & mask) as usize;
            if index >= palette_len {
                return Err(SaveError::InvalidFormat("palette index out of range"));
            }
        }
    }
    Ok(())
}

/// Reads a v1 section's light payload.
///
/// v1 always stored a dense array for non-zero light, including sections that
/// were uniform (sky 15). Those arrays are collapsed on load so an upgraded
/// v0.0.1 world also becomes compact in memory; light values are unchanged.
fn read_light_v1(reader: &mut Reader<'_>) -> Result<Light, SaveError> {
    match reader.read_u8()? {
        0 => Ok(Light::Uniform(0)),
        1 => Ok(collapse_if_uniform(read_dense_light(reader)?)),
        _ => Err(SaveError::InvalidFormat("invalid light presence byte")),
    }
}

/// Collapses a dense light array whose words are all equal to a uniform value.
fn collapse_if_uniform(light: Light) -> Light {
    match &light {
        Light::Uniform(_) => light,
        Light::Dense(data) => {
            let first = data[0];
            if data.iter().all(|&word| word == first) {
                Light::Uniform(first)
            } else {
                light
            }
        }
    }
}

/// Reads a v2 section's light payload.
fn read_light_v2(reader: &mut Reader<'_>) -> Result<Light, SaveError> {
    match reader.read_u8()? {
        0 => Ok(Light::Uniform(0)),
        1 => read_dense_light(reader),
        2 => {
            let value = reader.read_u16()?;
            if value == 0 {
                return Err(SaveError::InvalidFormat("uniform light value must be non-zero"));
            }
            Ok(Light::from_uniform(value))
        }
        _ => Err(SaveError::InvalidFormat("invalid light encoding flag")),
    }
}

/// Reads a dense array of packed light words.
fn read_dense_light(reader: &mut Reader<'_>) -> Result<Light, SaveError> {
    if reader.remaining() < BLOCK_COUNT * 2 {
        return Err(SaveError::InvalidFormat("truncated light data"));
    }
    let mut data = Box::new([0u16; BLOCK_COUNT]);
    for slot in data.iter_mut() {
        *slot = reader.read_u16()?;
    }
    Ok(Light::from_raw(Some(data)))
}

/// Rejects block ids that the session registry cannot resolve.
fn validate_block(registry: &Registry, id: BlockId) -> Result<(), SaveError> {
    if usize::from(id.0) < registry.len() {
        Ok(())
    } else {
        Err(SaveError::UnknownBlock(id.0))
    }
}

/// Bounds-checked cursor over the payload bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a cursor at the start of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Reads a fixed-size array, or reports a truncated stream.
    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], SaveError> {
        let end = self
            .pos
            .checked_add(N)
            .ok_or(SaveError::InvalidFormat("truncated stream"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(SaveError::InvalidFormat("truncated stream"))?;
        let mut value = [0u8; N];
        value.copy_from_slice(slice);
        self.pos = end;
        Ok(value)
    }

    /// Reads one byte.
    fn read_u8(&mut self) -> Result<u8, SaveError> {
        Ok(self.read_exact::<1>()?[0])
    }

    /// Reads a little-endian `u16`.
    fn read_u16(&mut self) -> Result<u16, SaveError> {
        Ok(u16::from_le_bytes(self.read_exact()?))
    }

    /// Reads a little-endian `u32`.
    fn read_u32(&mut self) -> Result<u32, SaveError> {
        Ok(u32::from_le_bytes(self.read_exact()?))
    }

    /// Reads a little-endian `u64`.
    fn read_u64(&mut self) -> Result<u64, SaveError> {
        Ok(u64::from_le_bytes(self.read_exact()?))
    }

    /// Reads a little-endian `i32`.
    fn read_i32(&mut self) -> Result<i32, SaveError> {
        Ok(i32::from_le_bytes(self.read_exact()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section::StorageMode;
    use ob_math::{BlockPos, LocalPos, SectionPos, WORLD_MIN_Y};

    fn base_registry() -> Arc<Registry> {
        Arc::new(Registry::base())
    }

    fn block(registry: &Registry, name: &str) -> BlockId {
        registry.id_or_air(name)
    }

    fn sample_world() -> (Arc<Registry>, World) {
        sample_world_with(base_registry())
    }

    fn sample_world_with(registry: Arc<Registry>) -> (Arc<Registry>, World) {
        let air = BlockId::AIR;
        let stone = block(&registry, "openblock:stone");
        let dirt = block(&registry, "openblock:dirt");
        let grass = block(&registry, "openblock:grass");
        let sand = block(&registry, "openblock:sand");
        let water = block(&registry, "openblock:water");
        let log = block(&registry, "openblock:log");
        let leaves = block(&registry, "openblock:leaves");
        let bedrock = block(&registry, "openblock:bedrock");

        let mut world = World::new(registry.clone(), 0x0BAD_F00D_DEAD_BEEF);
        let positions = [ChunkPos::new(0, 0), ChunkPos::new(-2, 3), ChunkPos::new(4, -1)];
        for (chunk_index, pos) in positions.into_iter().enumerate() {
            let mut column = ChunkColumn::new(pos);
            let origin_x = pos.x * 16;
            let origin_z = pos.z * 16;
            let rise = chunk_index as i32 * 3;

            for x in 0..16 {
                for z in 0..16 {
                    let world_x = origin_x + x;
                    let world_z = origin_z + z;
                    column.set_block(BlockPos::new(world_x, WORLD_MIN_Y, world_z), bedrock);
                    for y in (WORLD_MIN_Y + 1)..=(WORLD_MIN_Y + 4 + rise) {
                        column.set_block(BlockPos::new(world_x, y, world_z), stone);
                    }
                    column.set_block(BlockPos::new(world_x, WORLD_MIN_Y + 5 + rise, world_z), dirt);
                    column.set_block(BlockPos::new(world_x, WORLD_MIN_Y + 6 + rise, world_z), grass);
                    if (x + z) % 5 == 0 {
                        column.set_block(BlockPos::new(world_x, WORLD_MIN_Y + 7 + rise, world_z), water);
                    }
                }
            }

            // A non-uniform palette section with edited sky and block light.
            let surface = 8 + chunk_index as i32;
            column.set_block(BlockPos::new(origin_x + 1, surface, origin_z + 2), log);
            column.set_block(BlockPos::new(origin_x + 2, surface, origin_z + 2), leaves);
            column.set_block(BlockPos::new(origin_x + 3, surface, origin_z + 3), sand);
            let section_y = surface.div_euclid(16);
            let local = LocalPos {
                x: 1,
                y: (surface - section_y * 16) as u8,
                z: 2,
            };
            let section = column
                .section_mut(SectionPos::new(pos.x, section_y, pos.z))
                .expect("section belongs to the column");
            section.light_mut().set_sky(local, 11);
            section.light_mut().set_block(local, 4, 5, 6);

            // A uniform section and one using the uniform-zero light fast path.
            column
                .section_mut(SectionPos::new(pos.x, 1, pos.z))
                .expect("section belongs to the column")
                .fill(stone);
            let dark = column
                .section_mut(SectionPos::new(pos.x, -2, pos.z))
                .expect("section belongs to the column");
            *dark.light_mut() = Light::uniform_sky(0);

            // Guarantee a fully empty section as well.
            column
                .section_mut(SectionPos::new(pos.x, 2, pos.z))
                .expect("section belongs to the column")
                .fill(air);

            column.recompute_heightmap();
            world.insert_chunk(column);
        }
        (registry, world)
    }

    fn sorted_chunks(world: &World) -> Vec<(ChunkPos, &ChunkColumn)> {
        let mut chunks: Vec<(ChunkPos, &ChunkColumn)> = world.chunks().map(|(pos, chunk)| (*pos, chunk)).collect();
        chunks.sort_by_key(|(pos, _)| (pos.x, pos.z));
        chunks
    }

    fn assert_same_world(expected: &World, actual: &World) {
        assert_eq!(expected.seed(), actual.seed());
        let expected_chunks = sorted_chunks(expected);
        let actual_chunks = sorted_chunks(actual);
        assert_eq!(expected_chunks.len(), actual_chunks.len());
        for ((expected_pos, expected_chunk), (actual_pos, actual_chunk)) in
            expected_chunks.iter().zip(actual_chunks.iter())
        {
            assert_eq!(expected_pos, actual_pos);
            assert_eq!(expected_chunk.pos(), actual_chunk.pos());
            let expected_sections: Vec<_> = expected_chunk.sections().collect();
            let actual_sections: Vec<_> = actual_chunk.sections().collect();
            assert_eq!(expected_sections.len(), N_SECTIONS);
            assert_eq!(actual_sections.len(), N_SECTIONS);
            for ((expected_section_pos, expected_section), (actual_section_pos, actual_section)) in
                expected_sections.iter().zip(actual_sections.iter())
            {
                assert_eq!(expected_section_pos, actual_section_pos);
                assert_eq!(expected_section.mode(), actual_section.mode());
                assert_eq!(expected_section.palette(), actual_section.palette());
                assert_eq!(expected_section.words(), actual_section.words());
                assert_eq!(expected_section.non_air_count(), actual_section.non_air_count());
                for index in 0..BLOCK_COUNT {
                    let local = LocalPos::from_index(index);
                    assert_eq!(
                        expected_section.get(local),
                        actual_section.get(local),
                        "block mismatch in {expected_section_pos:?} at {local:?}"
                    );
                    assert_eq!(
                        expected_section.light().sky(local),
                        actual_section.light().sky(local),
                        "sky light mismatch in {expected_section_pos:?} at {local:?}"
                    );
                    assert_eq!(
                        expected_section.light().block_rgb(local),
                        actual_section.light().block_rgb(local),
                        "block light mismatch in {expected_section_pos:?} at {local:?}"
                    );
                }
            }
            for x in 0..16 {
                for z in 0..16 {
                    assert_eq!(
                        expected_chunk.height_at(x, z),
                        actual_chunk.height_at(x, z),
                        "heightmap mismatch at {x},{z} of {expected_pos:?}"
                    );
                }
            }
        }
    }

    fn paletted_section(bits: u8, palette_len: usize, registry: &Registry, light: Light) -> Section {
        assert!(palette_len > 0);
        let ids = [
            BlockId::AIR,
            block(registry, "openblock:stone"),
            block(registry, "openblock:dirt"),
            block(registry, "openblock:grass"),
            block(registry, "openblock:sand"),
            block(registry, "openblock:water"),
            block(registry, "openblock:log"),
            block(registry, "openblock:leaves"),
            block(registry, "openblock:bedrock"),
        ];
        let palette: Vec<BlockId> = (0..palette_len).map(|i| ids[i % ids.len()]).collect();
        let (word_count, per_word) = match bits {
            4 => (BITS4_WORDS, 16),
            8 => (BITS8_WORDS, 8),
            16 => (BITS16_WORDS, 4),
            _ => unreachable!("test only uses 4, 8 and 16 bit storage"),
        };
        let mut words = vec![0u64; word_count];
        for index in 0..BLOCK_COUNT {
            let palette_index = index % palette_len;
            let word = index / per_word;
            let shift = (index % per_word) * usize::from(bits);
            words[word] |= (palette_index as u64) << shift;
        }
        let storage = match bits {
            4 => SectionStorage::Bits4 { words, palette },
            8 => SectionStorage::Bits8 { words, palette },
            16 => SectionStorage::Bits16 { words, palette },
            _ => unreachable!("test only uses 4, 8 and 16 bit storage"),
        };
        Section::from_parts(storage, light)
    }

    fn compact_world(registry: &Arc<Registry>) -> World {
        let mut world = World::new(Arc::clone(registry), 9);
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        // All sections stay uniform air, with compact uniform-zero lights. This
        // fixture is about the save encoding, not about lighting; insert it as
        // authoritative rather than letting the world reseed it.
        for index in 0..N_SECTIONS {
            let y = WORLD_MIN_Y / 16 + index as i32;
            let section = column
                .section_mut(SectionPos::new(0, y, 0))
                .expect("section belongs to the column");
            *section.light_mut() = Light::uniform_sky(0);
        }
        world.insert_loaded_chunk(column);
        world
    }

    fn resign(bytes: &mut [u8]) {
        let payload_len = bytes.len() - CRC_LEN;
        let checksum = crc32c(&bytes[..payload_len]);
        bytes[payload_len..].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Builds a v1 snapshot (uncompressed, old dense light encoding).
    fn to_v1_bytes(world: &World) -> Vec<u8> {
        let payload = encode_payload(world, write_legacy_section);
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&world.seed().to_le_bytes());
        out.extend_from_slice(&payload);
        let checksum = crc32c(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Builds a v3 snapshot whose payload is stored without compression.
    fn to_v3_bytes(world: &World) -> Vec<u8> {
        let payload = encode_payload(world, write_section);
        let payload_len = u32::try_from(payload.len()).expect("test payload fits");
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&world.seed().to_le_bytes());
        out.extend_from_slice(&registry_fingerprint(world.registry()).to_le_bytes());
        out.push(COMPRESSION_NONE);
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&payload);
        let checksum = crc32c(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Builds a v2 snapshot whose payload is stored without compression.
    fn to_v2_uncompressed_bytes(world: &World) -> Vec<u8> {
        let payload = encode_payload(world, write_section);
        let payload_len = u32::try_from(payload.len()).expect("test payload fits");
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&world.seed().to_le_bytes());
        out.push(COMPRESSION_NONE);
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&payload);
        let checksum = crc32c(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    #[test]
    fn crc32c_matches_known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn round_trips_blocks_light_and_heightmaps() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);
        let loaded = from_bytes(registry, &bytes).expect("snapshot is valid");
        assert_same_world(&world, &loaded);
        assert_eq!(to_bytes(&loaded), bytes);
    }

    #[test]
    fn checked_encoder_enforces_the_loader_limits() {
        let (_registry, world) = sample_world();
        let bytes = to_bytes_checked(&world).expect("a normal sample world fits");
        assert_eq!(bytes, to_bytes(&world));

        let error = encode_v4(&world, 1, u64::MAX).expect_err("a tiny payload cap must be enforced");
        assert!(matches!(error, SaveError::PayloadTooLarge { max: 1, .. }), "{error}");
        assert!(
            error.to_string().contains("load limit"),
            "the error should name the limit: {error}"
        );

        let error = encode_v4(&world, MAX_SAVE_PAYLOAD_BYTES, 1).expect_err("a tiny file cap must be enforced");
        assert!(matches!(error, SaveError::SaveFileTooLarge { max: 1, .. }), "{error}");
    }

    #[test]
    fn save_is_deterministic_and_stable_after_load() {
        let (registry, world) = sample_world();
        let first = to_bytes(&world);
        assert_eq!(first, to_bytes(&world));
        assert_eq!(first[22], COMPRESSION_ZSTD, "v4 snapshots are compressed by default");

        let loaded = from_bytes(Arc::clone(&registry), &first).expect("snapshot is valid");
        assert_eq!(to_bytes(&loaded), first);

        // Insertion order must not leak into the byte stream.
        let mut reversed = World::new(Arc::clone(&registry), world.seed());
        let mut chunks: Vec<ChunkColumn> = world.chunks().map(|(_, chunk)| chunk.clone()).collect();
        chunks.reverse();
        for chunk in chunks {
            reversed.insert_chunk(chunk);
        }
        assert_eq!(to_bytes(&reversed), first);
    }

    #[test]
    fn empty_world_round_trips() {
        let registry = base_registry();
        let world = World::new(Arc::clone(&registry), 42);
        let bytes = to_bytes(&world);
        assert!(bytes.starts_with(MAGIC));
        assert_eq!(
            u16::from_le_bytes([bytes[4], bytes[5]]),
            VERSION,
            "empty snapshot uses the current version"
        );
        assert_eq!(bytes[22], COMPRESSION_ZSTD);
        let compressed_len = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], bytes[30]]) as usize;
        assert_eq!(bytes.len(), V4_HEADER_LEN + compressed_len + CRC_LEN);

        let loaded = from_bytes(Arc::clone(&registry), &bytes).expect("empty snapshot is valid");
        assert_eq!(loaded.seed(), 42);
        assert_eq!(loaded.chunk_count(), 0);
        assert_eq!(to_bytes(&loaded), bytes);
    }

    #[test]
    fn v1_snapshots_still_load_and_upgrade_to_v4() {
        let (registry, world) = sample_world();
        let v1 = to_v1_bytes(&world);
        assert_eq!(u16::from_le_bytes([v1[4], v1[5]]), 1);
        assert_eq!(&v1[..4], MAGIC);

        let loaded = from_bytes(Arc::clone(&registry), &v1).expect("v1 snapshot is valid");
        assert_same_world(&world, &loaded);

        // Saving the upgraded world writes a self-consistent v2 snapshot.
        let upgraded = to_bytes(&loaded);
        assert_eq!(u16::from_le_bytes([upgraded[4], upgraded[5]]), VERSION);
        let reloaded = from_bytes(Arc::clone(&registry), &upgraded).expect("upgraded snapshot is valid");
        assert_same_world(&loaded, &reloaded);
        assert_eq!(to_bytes(&reloaded), upgraded);
    }

    #[test]
    fn v1_dense_uniform_light_is_collapsed_on_load() {
        let registry = base_registry();
        let mut world = World::new(Arc::clone(&registry), 11);
        // A default column uses compact uniform sky-15 light; v1 writes it as a
        // dense array, but loading must recover the compact representation.
        world.insert_chunk(ChunkColumn::new(ChunkPos::new(0, 0)));
        let v1 = to_v1_bytes(&world);

        let loaded = from_bytes(Arc::clone(&registry), &v1).expect("v1 snapshot is valid");
        let chunk = loaded.chunk(ChunkPos::new(0, 0)).expect("chunk was loaded");
        for (_, section) in chunk.sections() {
            assert_eq!(section.light().uniform_value(), Some(15 << 12));
            assert_eq!(section.light().heap_bytes(), 0);
        }
    }

    #[test]
    fn v2_dense_uniform_light_keeps_its_encoding() {
        let registry = base_registry();
        let pos = ChunkPos::new(0, 0);
        let mut sections: Vec<Section> = (0..N_SECTIONS).map(|_| Section::empty()).collect();
        sections[0] = Section::from_parts(
            SectionStorage::Uniform(BlockId::AIR),
            Light::from_raw(Some(Box::new([0x1234u16; BLOCK_COUNT]))),
        );
        let mut world = World::new(Arc::clone(&registry), 77);
        world.insert_chunk(ChunkColumn::from_sections(pos, sections));

        let bytes = to_bytes(&world);
        let loaded = from_bytes(Arc::clone(&registry), &bytes).expect("snapshot is valid");
        let loaded_chunk = loaded.chunk(pos).expect("chunk was loaded");
        let loaded_light = loaded_chunk
            .section(SectionPos::new(0, WORLD_MIN_Y / 16, 0))
            .expect("section belongs to the chunk")
            .light();
        assert_eq!(loaded_light.uniform_value(), None, "flag 1 stays dense");
        assert_eq!(loaded_light.raw().expect("dense array")[0], 0x1234);
        assert_eq!(
            to_bytes(&loaded),
            bytes,
            "a dense-uniform light array is canonical for v2 and must round-trip unchanged"
        );
    }

    #[test]
    fn v1_checksum_is_validated() {
        let (registry, world) = sample_world();
        let mut bytes = to_v1_bytes(&world);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        match from_bytes(registry, &bytes) {
            Err(SaveError::InvalidFormat("checksum mismatch")) => {}
            other => panic!("expected checksum mismatch, got {other:?}"),
        }
    }

    #[test]
    fn v2_uncompressed_payload_loads() {
        let (registry, world) = sample_world();
        let bytes = to_v2_uncompressed_bytes(&world);
        assert_eq!(bytes[14], COMPRESSION_NONE);
        assert_eq!(
            u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]),
            u32::from_le_bytes([bytes[19], bytes[20], bytes[21], bytes[22]])
        );

        let loaded = from_bytes(Arc::clone(&registry), &bytes).expect("uncompressed v2 snapshot is valid");
        assert_same_world(&world, &loaded);
        let recompressed = to_bytes(&loaded);
        let reloaded = from_bytes(registry, &recompressed).expect("recompressed snapshot is valid");
        assert_same_world(&loaded, &reloaded);
        assert_eq!(to_bytes(&reloaded), recompressed);
    }

    #[test]
    fn v2_light_encoding_uses_compact_uniform_forms() {
        let registry = base_registry();
        let mut world = World::new(Arc::clone(&registry), 5);
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));

        // Every section starts as uniform air with uniform-zero light.
        for index in 0..N_SECTIONS {
            let y = WORLD_MIN_Y / 16 + index as i32;
            let section = column
                .section_mut(SectionPos::new(0, y, 0))
                .expect("section belongs to the column");
            *section.light_mut() = Light::uniform_sky(0);
        }
        // Section 0: uniform non-zero; section 1: dense with one edit.
        let first_y = WORLD_MIN_Y / 16;
        *column
            .section_mut(SectionPos::new(0, first_y, 0))
            .expect("section belongs to the column")
            .light_mut() = Light::uniform_sky(15);
        let dense = column
            .section_mut(SectionPos::new(0, first_y + 1, 0))
            .expect("section belongs to the column");
        *dense.light_mut() = Light::from_uniform(0);
        dense.light_mut().set_sky(LocalPos { x: 0, y: 0, z: 0 }, 7);
        world.insert_loaded_chunk(column);

        let payload = encode_payload(&world, write_section);
        assert_eq!(u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]), 1);

        // Skip chunk count and chunk coordinates; all sections use uniform
        // storage (mode byte + id), so the light flags can be read directly.
        let mut offset = 12;
        for index in 0..N_SECTIONS {
            assert_eq!(payload[offset], 0, "uniform storage mode in section {index}");
            offset += 1;
            let id = u16::from_le_bytes([payload[offset], payload[offset + 1]]);
            assert_eq!(id, BlockId::AIR.0);
            offset += 2;

            let flag = payload[offset];
            offset += 1;
            match index {
                0 => {
                    assert_eq!(flag, 2, "uniform non-zero light uses flag 2");
                    let value = u16::from_le_bytes([payload[offset], payload[offset + 1]]);
                    assert_eq!(value, 15 << 12);
                    offset += 2;
                }
                1 => {
                    assert_eq!(flag, 1, "edited light uses a dense array");
                    offset += BLOCK_COUNT * 2;
                }
                _ => assert_eq!(flag, 0, "uniform-zero light uses flag 0"),
            }
        }
        assert_eq!(offset, payload.len(), "payload has no trailing bytes");
    }

    #[test]
    fn v4_header_lengths_and_compression_are_validated() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);

        let mut unknown_method = bytes.clone();
        unknown_method[22] = 9;
        resign(&mut unknown_method);
        assert!(matches!(
            from_bytes(Arc::clone(&registry), &unknown_method),
            Err(SaveError::InvalidFormat("unknown compression method"))
        ));

        let mut huge_payload = bytes.clone();
        huge_payload[23..27].copy_from_slice(&u32::MAX.to_le_bytes());
        resign(&mut huge_payload);
        assert!(matches!(
            from_bytes(Arc::clone(&registry), &huge_payload),
            Err(SaveError::InvalidFormat("uncompressed payload too large"))
        ));

        let compressed_len = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], bytes[30]]);
        let mut trailing = bytes.clone();
        trailing[27..31].copy_from_slice(&(compressed_len + 1).to_le_bytes());
        resign(&mut trailing);
        assert!(matches!(
            from_bytes(Arc::clone(&registry), &trailing),
            Err(SaveError::InvalidFormat("truncated payload"))
        ));

        let mut truncated = bytes.clone();
        truncated[27..31].copy_from_slice(&compressed_len.saturating_sub(1).to_le_bytes());
        resign(&mut truncated);
        assert!(matches!(
            from_bytes(Arc::clone(&registry), &truncated),
            Err(SaveError::InvalidFormat("trailing bytes after payload"))
        ));
    }

    #[test]
    fn crc_corruption_is_invalid_format() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);
        let offsets = [
            10,
            bytes.len() / 2,
            bytes.len() - 5,
            bytes.len() - 1, // the checksum itself
        ];
        for offset in offsets {
            let mut corrupt = bytes.clone();
            corrupt[offset] ^= 0xFF;
            let error = from_bytes(Arc::clone(&registry), &corrupt).expect_err("corrupt snapshot must not load");
            assert!(
                matches!(error, SaveError::InvalidFormat(_)),
                "offset {offset}: unexpected error {error}"
            );
        }
    }

    #[test]
    fn truncated_stream_is_rejected_without_panicking() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);
        let mut lengths = vec![0, 1, 2, 3, 4, 5, 6, 17, 18, 19, 20, 21];
        lengths.extend([bytes.len() / 4, bytes.len() / 2, bytes.len() * 3 / 4, bytes.len() - 1]);
        for len in lengths {
            if len >= bytes.len() {
                continue;
            }
            let result = from_bytes(Arc::clone(&registry), &bytes[..len]);
            assert!(result.is_err(), "truncated stream of {len} bytes was accepted");
        }

        // A structurally truncated but checksummed stream must also fail.
        let compact = compact_world(&registry);
        let compact_bytes = to_bytes(&compact);
        for len in (V4_HEADER_LEN + CRC_LEN)..compact_bytes.len() {
            let mut truncated = compact_bytes[..len].to_vec();
            resign(&mut truncated);
            assert!(
                from_bytes(Arc::clone(&registry), &truncated).is_err(),
                "re-checksummed truncated stream of {len} bytes was accepted"
            );
        }
    }

    /// Wraps a raw chunk payload in a v4 header with no compression and a valid
    /// checksum, so tests can mutate the payload and still reach the decoder.
    fn v4_uncompressed(payload: &[u8]) -> Vec<u8> {
        let len = u32::try_from(payload.len()).expect("test payload fits");
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&7u64.to_le_bytes());
        out.extend_from_slice(&registry_fingerprint(&base_registry()).to_le_bytes());
        out.push(COMPRESSION_NONE);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(payload);
        let checksum = crc32c(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    #[test]
    fn compressed_header_size_must_match_the_zstd_frame() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);
        assert_eq!(bytes[22], COMPRESSION_ZSTD);

        // A tiny body that claims a huge decompressed payload must be rejected
        // from the zstd frame header, before the payload is allocated.
        let mut lying = bytes.clone();
        lying[23..27].copy_from_slice(&(64 * 1024 * 1024u32).to_le_bytes());
        resign(&mut lying);
        match from_bytes(Arc::clone(&registry), &lying) {
            Err(SaveError::InvalidFormat("zstd frame content size mismatch")) => {}
            other => panic!("expected a frame-size mismatch, got {other:?}"),
        }

        // Garbage behind a valid checksum is a typed error, not a panic.
        let mut garbage = bytes;
        let body_end = garbage.len() - CRC_LEN;
        for byte in &mut garbage[V4_HEADER_LEN..body_end] {
            *byte = 0xFF;
        }
        resign(&mut garbage);
        assert!(matches!(
            from_bytes(registry, &garbage),
            Err(SaveError::InvalidFormat("zstd frame header is invalid"))
        ));
    }

    #[test]
    fn uncompressed_payload_structure_is_validated() {
        let registry = base_registry();
        let compact = compact_world(&registry);
        let payload = encode_payload(&compact, write_section);
        assert_eq!(encode_payload(&compact, write_section), payload);
        assert_eq!(payload.len(), 4 + 8 + N_SECTIONS * 4, "one uniform chunk");

        let chunk = &payload[4..];
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("chunk count exceeds payload", vec![1, 0, 0, 0]),
            ("trailing bytes after chunks", {
                let mut trailing = payload.clone();
                trailing.push(0);
                trailing
            }),
            ("unknown storage mode", {
                let mut bad = payload.clone();
                bad[12] = 9;
                bad
            }),
            ("invalid light encoding flag", {
                let mut bad = payload.clone();
                bad[15] = 9;
                bad
            }),
            ("uniform light value must be non-zero", {
                let mut bad = payload[..15].to_vec();
                bad.extend_from_slice(&[2, 0, 0]);
                bad.extend_from_slice(&payload[16..]);
                bad
            }),
            ("duplicate chunk position", {
                let mut duplicate = Vec::new();
                duplicate.extend_from_slice(&2u32.to_le_bytes());
                duplicate.extend_from_slice(chunk);
                duplicate.extend_from_slice(chunk);
                duplicate
            }),
        ];
        for (expected, payload) in cases {
            let bytes = v4_uncompressed(&payload);
            match from_bytes(Arc::clone(&registry), &bytes) {
                Err(SaveError::InvalidFormat(reason)) => {
                    assert_eq!(reason, expected, "unexpected rejection reason");
                }
                other => panic!("payload case {expected:?} was accepted: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn oversized_world_file_is_rejected_from_metadata() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!("ob-save-oversized-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("world.obs");
        let file = std::fs::File::create(&path).expect("create sparse save");
        file.set_len(MAX_SAVE_FILE_BYTES + 1)
            .expect("make the sparse save oversized");
        drop(file);
        let error = read_save_file(&path).expect_err("oversized save must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("limit"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_regular_save_paths_are_rejected_before_opening() {
        let dir = std::env::temp_dir().join(format!(
            "openblock-save-non-regular-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join("world.obs")).expect("create directory in place of the save");
        let error = read_save_file(&dir.join("world.obs")).expect_err("a directory is not a save file");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn newer_version_is_unsupported() {
        let (registry, world) = sample_world();
        let mut bytes = to_bytes(&world);
        bytes[4..6].copy_from_slice(&99u16.to_le_bytes());
        match from_bytes(registry, &bytes) {
            Err(SaveError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {other:?}"),
        }
    }

    #[test]
    fn unknown_block_ids_are_rejected() {
        let registry = base_registry();
        let pos = ChunkPos::new(1, 1);

        // Section::set widening to 8-bit storage with ids the registry lacks.
        let mut widened = Section::empty();
        for index in 0..17u16 {
            widened.set(LocalPos::from_index(usize::from(index)), BlockId(1000 + index));
        }
        assert_eq!(widened.mode(), StorageMode::Bits8);
        let mut sections: Vec<Section> = (0..N_SECTIONS).map(|_| Section::empty()).collect();
        sections[4] = widened;
        let mut world = World::new(Arc::clone(&registry), 1);
        world.insert_chunk(ChunkColumn::from_sections(pos, sections));
        let bytes = to_bytes(&world);
        match from_bytes(Arc::clone(&registry), &bytes) {
            Err(SaveError::UnknownBlock(id)) => assert_eq!(id, 1000),
            other => panic!("expected UnknownBlock, got {other:?}"),
        }

        // Uniform sections are validated against the registry as well.
        let sections: Vec<Section> = (0..N_SECTIONS).map(|_| Section::uniform(BlockId(999))).collect();
        let mut world = World::new(Arc::clone(&registry), 2);
        world.insert_chunk(ChunkColumn::from_sections(pos, sections));
        let bytes = to_bytes(&world);
        match from_bytes(registry, &bytes) {
            Err(SaveError::UnknownBlock(999)) => {}
            other => panic!("expected UnknownBlock(999), got {other:?}"),
        }
    }

    #[test]
    fn bits8_and_bits16_sections_round_trip() {
        let registry = base_registry();
        let pos = ChunkPos::new(2, -3);
        let mut sections: Vec<Section> = (0..N_SECTIONS).map(|_| Section::empty()).collect();
        let mut bits8 = paletted_section(8, 17, &registry, Light::uniform_sky(12));
        bits8.light_mut().set_sky(LocalPos { x: 0, y: 0, z: 0 }, 3);
        bits8.light_mut().set_block(LocalPos { x: 1, y: 2, z: 3 }, 4, 5, 6);
        sections[4] = bits8;
        sections[5] = paletted_section(16, 300, &registry, Light::uniform_sky(0));
        let mut world = World::new(Arc::clone(&registry), 7);
        world.insert_chunk(ChunkColumn::from_sections(pos, sections));

        let bytes = to_bytes(&world);
        let snapshot = world.snapshot();
        assert_eq!(
            snapshot.payload_len().expect("payload length"),
            encode_payload(&world, write_section).len(),
            "streaming length must cover 8- and 16-bit palettes and dense light"
        );
        let async_bytes = snapshot.to_bytes_checked().expect("streaming snapshot encodes");
        let async_loaded = from_bytes(Arc::clone(&registry), &async_bytes).expect("streaming snapshot is valid");
        assert_same_world(&world, &async_loaded);

        let loaded = from_bytes(Arc::clone(&registry), &bytes).expect("snapshot is valid");
        let loaded_chunk = loaded.chunk(pos).expect("chunk was loaded");
        let modes: Vec<StorageMode> = loaded_chunk.sections().map(|(_, s)| s.mode()).collect();
        assert_eq!(modes[0], StorageMode::Uniform);
        assert_eq!(modes[4], StorageMode::Bits8);
        assert_eq!(modes[5], StorageMode::Bits16);
        assert_same_world(&world, &loaded);
        assert_eq!(to_bytes(&loaded), bytes);
    }

    #[test]
    fn arbitrary_and_mutated_bytes_never_panic() {
        let registry = base_registry();
        for len in 0..=64 {
            for fill in [0u8, 1, 2, b'O', 0xFF] {
                let data = vec![fill; len];
                let _ = from_bytes(Arc::clone(&registry), &data);
            }
        }

        let compact = compact_world(&registry);
        let original = to_bytes(&compact);
        for offset in 0..original.len() - CRC_LEN {
            for value in [0u8, 1, 2, 3, 4, 15, 254, 255] {
                let mut data = original.clone();
                data[offset] = value;
                resign(&mut data);
                let _ = from_bytes(Arc::clone(&registry), &data);
            }
        }
    }

    #[test]
    fn v2_snapshots_still_load_and_upgrade_to_v4() {
        let (registry, world) = sample_world();
        let v2 = to_v2_uncompressed_bytes(&world);
        assert_eq!(u16::from_le_bytes([v2[4], v2[5]]), 2);
        let loaded = from_bytes(Arc::clone(&registry), &v2).expect("v2 snapshot still loads");
        assert_same_world(&world, &loaded);
        let upgraded = to_bytes(&loaded);
        assert_eq!(u16::from_le_bytes([upgraded[4], upgraded[5]]), VERSION);
        assert_eq!(load_checked(Arc::clone(&registry), &upgraded).seed(), world.seed());
    }

    fn load_checked(registry: Arc<Registry>, bytes: &[u8]) -> World {
        from_bytes(registry, bytes).expect("snapshot is valid")
    }

    #[test]
    fn v4_rejects_a_different_registry_fingerprint() {
        let (registry, world) = sample_world();
        let bytes = to_bytes(&world);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 4);

        // Same block ordering but changed content (a renamed block id) must be detected.
        let mut blocks: Vec<_> = registry.iter().map(|(_, block)| block.clone()).collect();
        blocks[1].id = "openblock:stone_variant";
        let changed = Arc::new(Registry::from_blocks(blocks).expect("variant registry is valid"));
        match from_bytes(changed, &bytes) {
            Err(SaveError::RegistryMismatch { saved, current }) => assert_ne!(saved, current),
            other => panic!("expected RegistryMismatch, got {other:?}"),
        }
    }

    /// Decompresses the v4 body so tests can compare the snapshot encoder's
    /// payload with the buffered reference encoder.
    fn v4_payload(bytes: &[u8]) -> Vec<u8> {
        assert_eq!(bytes[22], COMPRESSION_ZSTD);
        let uncompressed = u32::from_le_bytes([bytes[23], bytes[24], bytes[25], bytes[26]]) as usize;
        let compressed = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], bytes[30]]) as usize;
        decode_payload(
            COMPRESSION_ZSTD,
            uncompressed,
            &bytes[V4_HEADER_LEN..V4_HEADER_LEN + compressed],
        )
        .expect("snapshot body decompresses")
    }

    #[test]
    fn snapshot_encoding_matches_the_buffered_payload_and_is_deterministic() {
        let (registry, world) = sample_world();
        let snapshot = world.snapshot();
        assert_eq!(snapshot.seed(), world.seed());
        assert_eq!(snapshot.chunk_count(), world.chunk_count());
        assert_eq!(
            snapshot.registry_fingerprint(),
            registry_fingerprint(&registry),
            "snapshot freezes the registry fingerprint"
        );

        let payload = encode_payload(&world, write_section);
        assert_eq!(
            snapshot.payload_len().expect("payload length is computable"),
            payload.len(),
            "the streaming length walk must exactly match write_section"
        );

        let first = snapshot.to_bytes_checked().expect("snapshot encodes");
        let second = snapshot.to_bytes_checked().expect("snapshot encodes deterministically");
        assert_eq!(first, second, "the same snapshot must always encode identically");
        assert_eq!(
            v4_payload(&first),
            payload,
            "streaming and buffered encoders must agree on the v4 payload"
        );

        let loaded = from_bytes(Arc::clone(&registry), &first).expect("snapshot loads");
        assert_same_world(&world, &loaded);
        assert_eq!(
            v4_payload(&to_bytes(&loaded)),
            payload,
            "a loaded snapshot re-encodes the same chunk payload"
        );
    }

    #[test]
    fn snapshot_is_isolated_from_later_world_edits() {
        let (registry, world) = sample_world();
        let snapshot = world.snapshot();
        let before = snapshot.to_bytes_checked().expect("snapshot encodes");
        let original_payload = v4_payload(&before);

        // An air block high above the sample terrain is a real edit that the
        // snapshot must not observe (Arc copy-on-write per touched column).
        let mut edited = world;
        let edit_pos = BlockPos::new(1, 200, 1);
        let stone = block(&registry, "openblock:stone");
        assert!(edited.set_block(edit_pos, stone), "edit applies to the live world");

        assert_eq!(
            v4_payload(&snapshot.to_bytes_checked().expect("snapshot still encodes")),
            original_payload,
            "the snapshot must not observe an edit made after capture"
        );
        assert_ne!(
            v4_payload(&to_bytes(&edited)),
            original_payload,
            "the live world changed, so its own encoding must differ"
        );
        // The pre-edit snapshot must still load the original blocks.
        let loaded = from_bytes(Arc::clone(&registry), &before).expect("old snapshot still loads");
        assert_eq!(loaded.get_block(edit_pos), BlockId::AIR);
        assert_eq!(edited.get_block(edit_pos), stone);
    }

    #[test]
    fn empty_snapshot_round_trips() {
        let registry = base_registry();
        let world = World::new(Arc::clone(&registry), 42);
        let snapshot = world.snapshot();
        assert_eq!(snapshot.chunk_count(), 0);
        let bytes = snapshot.to_bytes_checked().expect("empty snapshot encodes");
        let loaded = from_bytes(registry, &bytes).expect("empty snapshot loads");
        assert_eq!(loaded.chunk_count(), 0);
        assert_eq!(loaded.seed(), 42);
        assert_eq!(v4_payload(&to_bytes(&loaded)), v4_payload(&bytes));
    }

    #[test]
    fn v4_accepts_an_append_only_registry_prefix() {
        let current = base_registry();
        // The 1.0.2-era block registry: air through the crafting table, before
        // the five wave-1 blocks (three ores, furnace, chest) were appended at
        // the end. This is the realistic pre-1.2 content pack; the bytes are
        // generated here rather than committed as a fixture.
        let mut old_blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        old_blocks.truncate(15);
        let old_registry = Arc::new(Registry::from_blocks(old_blocks).expect("old registry is valid"));
        let old_fingerprint = registry_fingerprint(&old_registry);
        assert_ne!(old_fingerprint, registry_fingerprint(&current));

        let (_, world) = sample_world_with(Arc::clone(&old_registry));
        let bytes = to_bytes(&world);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 4, "1.0.2 wrote v4");
        assert_eq!(
            u64::from_le_bytes([
                bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21]
            ]),
            old_fingerprint
        );

        let loaded = from_bytes(Arc::clone(&current), &bytes).expect("a prefix registry save must load");
        assert_same_world(&world, &loaded);

        // Re-saving records today's full fingerprint and stays deterministic.
        let upgraded = to_bytes(&loaded);
        assert_eq!(
            u64::from_le_bytes([
                upgraded[14],
                upgraded[15],
                upgraded[16],
                upgraded[17],
                upgraded[18],
                upgraded[19],
                upgraded[20],
                upgraded[21]
            ]),
            registry_fingerprint(&current)
        );
        let reloaded = from_bytes(Arc::clone(&current), &upgraded).expect("upgraded save loads");
        assert_same_world(&loaded, &reloaded);
        assert_eq!(to_bytes(&reloaded), upgraded);
    }

    #[test]
    fn v4_accepts_every_append_only_registry_prefix() {
        let current = base_registry();
        let blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        let (_, world) = sample_world_with(Arc::clone(&current));
        for count in 1..=blocks.len() {
            let prefix = Arc::new(Registry::from_blocks(blocks[..count].to_vec()).expect("a registry prefix is valid"));
            let (_, prefix_world) = sample_world_with(Arc::clone(&prefix));
            let bytes = to_bytes(&prefix_world);
            let loaded = from_bytes(Arc::clone(&current), &bytes)
                .unwrap_or_else(|error| panic!("prefix of {count} blocks was rejected: {error}"));
            assert_same_world(&prefix_world, &loaded);
            assert_eq!(prefix_world.seed(), world.seed());
        }
    }

    #[test]
    fn v4_rejects_reordered_and_removed_registries() {
        let current = base_registry();
        let (_, world) = sample_world();
        let bytes = to_bytes(&world);

        // Reorder two existing block ids: the saved fingerprint is no longer a
        // prefix of today's ordered registry.
        let mut reordered_blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        reordered_blocks.swap(1, 2);
        let reordered = Arc::new(Registry::from_blocks(reordered_blocks).expect("a reordered registry is valid"));
        match from_bytes(reordered, &bytes) {
            Err(SaveError::RegistryMismatch { .. }) => {}
            other => panic!("a reordered registry must be rejected, got {other:?}"),
        }

        // Remove a block from the middle: later ids shift, so this is not a
        // prefix even though the registry is shorter.
        let mut removed_blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        removed_blocks.remove(1);
        let removed = Arc::new(Registry::from_blocks(removed_blocks).expect("a shortened registry is valid"));
        match from_bytes(removed, &bytes) {
            Err(SaveError::RegistryMismatch { .. }) => {}
            other => panic!("a registry with a removed block must be rejected, got {other:?}"),
        }

        // Appending to the *saved* registry while reordering the saved prefix
        // must also be rejected: a longer registry is not a prefix of a shorter
        // save, and the modified prefix changes the hash.
        let mut tampered_blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        tampered_blocks.truncate(12);
        tampered_blocks.swap(1, 2);
        let tampered = Arc::new(Registry::from_blocks(tampered_blocks).expect("tampered registry is valid"));
        let (_, tampered_world) = sample_world_with(Arc::clone(&tampered));
        let tampered_bytes = to_bytes(&tampered_world);
        match from_bytes(Arc::clone(&current), &tampered_bytes) {
            Err(SaveError::RegistryMismatch { .. }) => {}
            other => panic!("a mutated saved prefix must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn v3_reordered_fingerprint_is_rejected() {
        let (registry, world) = sample_world();
        let mut bytes = to_v3_bytes(&world);
        let mut blocks: Vec<_> = registry.iter().map(|(_, block)| block.clone()).collect();
        blocks.swap(1, 2);
        let reordered = Registry::from_blocks(blocks).expect("reordered registry is valid");
        let fingerprint = registry_fingerprint(&reordered);
        bytes[14..22].copy_from_slice(&fingerprint.to_le_bytes());
        resign(&mut bytes);
        match from_bytes(registry, &bytes) {
            Err(SaveError::RegistryMismatch { .. }) => {}
            other => panic!("a reordered v3 registry must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn v3_old_fingerprint_loads_append_only_and_upgrades_to_v4() {
        let new_registry = base_registry();

        // Build the v0.0.7 registry: the twelve blocks before planks,
        // cobblestone and the crafting table were appended.
        let mut old_blocks: Vec<_> = new_registry.iter().map(|(_, block)| block.clone()).collect();
        old_blocks.truncate(12);
        let old_registry = Arc::new(Registry::from_blocks(old_blocks).expect("old registry is valid"));
        assert_eq!(old_registry.len(), 12);
        assert_ne!(
            registry_fingerprint(&old_registry),
            registry_fingerprint(&new_registry),
            "the appended blocks must change the fingerprint"
        );

        let (_, world) = sample_world_with(Arc::clone(&old_registry));
        let v3 = to_v3_bytes(&world);
        assert_eq!(u16::from_le_bytes([v3[4], v3[5]]), 3);
        assert_eq!(
            u64::from_le_bytes([v3[14], v3[15], v3[16], v3[17], v3[18], v3[19], v3[20], v3[21]]),
            registry_fingerprint(&old_registry)
        );

        // This build accepts the old fingerprint, interprets ids unchanged and
        // then writes the strict v4 format on the next save.
        let loaded =
            from_bytes(Arc::clone(&new_registry), &v3).expect("v3 save with a pre-append-only fingerprint must load");
        assert_same_world(&world, &loaded);
        let upgraded = to_bytes(&loaded);
        assert_eq!(u16::from_le_bytes([upgraded[4], upgraded[5]]), VERSION);
        let reloaded = from_bytes(new_registry, &upgraded).expect("upgraded v4 snapshot is valid");
        assert_same_world(&loaded, &reloaded);
    }

    #[test]
    fn base_pack_and_code_registry_share_a_fingerprint() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/base/blocks.json");
        let packed = ob_content::registry_from_pack(&path).expect("base pack loads");
        assert_eq!(
            registry_fingerprint(&Registry::base()),
            registry_fingerprint(&packed),
            "packs/base/blocks.json must stay in sync with Registry::base()"
        );
    }

    fn unique_world_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ob-light-migration-{}-{label}-{id}", std::process::id()))
    }

    /// A hand-built "1.0/1.1" world: a sealed cave whose light is the old fake
    /// per-section sky 15. A correct loader must turn every cave cell sky 0.
    fn fake_light_cave_world(registry: &Arc<Registry>, pos: ChunkPos) -> (World, BlockPos) {
        let stone = block(registry, "openblock:stone");
        let mut column = ChunkColumn::new(pos);
        let (ox, oz) = pos.origin();
        for y in 50..70 {
            for z in 0..16 {
                for x in 0..16 {
                    column.set_block(BlockPos::new(ox + x, y, oz + z), stone);
                }
            }
        }
        for y in 64..=66 {
            for z in 4..=10 {
                for x in 4..=10 {
                    column.set_block(BlockPos::new(ox + x, y, oz + z), BlockId::AIR);
                }
            }
        }
        for index in 0..N_SECTIONS {
            let y = WORLD_MIN_Y / 16 + index as i32;
            let section = column
                .section_mut(SectionPos::new(pos.x, y, pos.z))
                .expect("section belongs to the column");
            *section.light_mut() = Light::uniform_sky(15);
        }
        column.recompute_heightmap();
        let mut world = World::new(Arc::clone(registry), 42);
        world.insert_loaded_chunk(column);
        (world, BlockPos::new(ox + 7, 65, oz + 7))
    }

    #[test]
    fn world_meta_round_trips_and_preserves_unknown_lines() {
        let dir = unique_world_dir("meta-round-trip");
        std::fs::create_dir_all(&dir).expect("create world dir");
        let meta = WorldMeta {
            light_version: 2,
            extra_lines: vec!["future_key=kept".to_owned()],
        };
        write_world_meta(&dir, &meta).expect("write meta");
        let read = read_world_meta(&dir).expect("read meta").expect("meta exists");
        assert_eq!(read, meta);
        let text = std::fs::read_to_string(dir.join(WORLD_META_FILE)).expect("meta text");
        assert!(text.contains("light_version=2"));
        assert!(text.contains("future_key=kept"));

        // A file without the key is version 0, never an error.
        std::fs::write(dir.join(WORLD_META_FILE), "# old sidecar\nunknown=1\n").expect("write legacy meta");
        let legacy = read_world_meta(&dir).expect("legacy meta").expect("exists");
        assert_eq!(legacy.light_version, 0);
        assert_eq!(legacy.extra_lines, vec!["unknown=1"]);
        std::fs::remove_dir_all(&dir).expect("remove temp world");
    }

    #[test]
    fn legacy_fake_light_world_is_relit_once_and_reloaded_without_relight() {
        let current = base_registry();
        // The 1.0.2-era registry: air through the crafting table. A v4 save
        // written with it is accepted by prefix and then migrated.
        let mut old_blocks: Vec<_> = current.iter().map(|(_, block)| block.clone()).collect();
        old_blocks.truncate(15);
        let old_registry = Arc::new(Registry::from_blocks(old_blocks).expect("old registry is valid"));
        let (fake_world, cave) = fake_light_cave_world(&old_registry, ChunkPos::new(0, 0));
        assert_eq!(fake_world.sky_light(cave), 15, "the 1.0 fixture starts fake-lit");

        let dir = unique_world_dir("legacy-relight");
        std::fs::create_dir_all(&dir).expect("create world dir");
        std::fs::write(dir.join("world.obs"), to_bytes(&fake_world)).expect("write fake save");
        assert!(!dir.join(WORLD_META_FILE).exists());

        let (world, migration) =
            load_world_dir_with_report(Arc::clone(&current), &dir).expect("legacy world loads and migrates");
        assert!(migration.relit && migration.save_rewritten && migration.meta_written);
        assert_eq!(world.light_version(), LIGHT_VERSION);
        assert_eq!(world.sky_light(cave), 0, "the sealed cave is dark after migration");
        assert_eq!(world.block_light(cave), 0);

        let meta = read_world_meta(&dir).expect("read meta").expect("marker written");
        assert_eq!(meta.light_version, LIGHT_VERSION);
        let rewritten = std::fs::read(dir.join("world.obs")).expect("rewritten save");
        assert_eq!(u16::from_le_bytes([rewritten[4], rewritten[5]]), 4, "v4 is preserved");
        assert_ne!(rewritten, to_bytes(&fake_world), "migration persisted the relit light");

        // The reload trusts the marker and does not relight. The cave still
        // reads dark because the first migration saved the corrected words.
        let (reloaded, second) = load_world_dir_with_report(Arc::clone(&current), &dir).expect("relit world reloads");
        assert!(!second.relit && !second.save_rewritten && !second.meta_written);
        assert_eq!(reloaded.sky_light(cave), 0);
        assert_eq!(reloaded.light_hash(), world.light_hash());
        std::fs::remove_dir_all(&dir).expect("remove temp world");
    }

    #[test]
    fn a_v2_marker_prevents_a_second_relight_even_for_a_legacy_light_payload() {
        let registry = base_registry();
        let (fake_world, cave) = fake_light_cave_world(&registry, ChunkPos::new(2, -1));
        let dir = unique_world_dir("marker-trust");
        std::fs::create_dir_all(&dir).expect("create world dir");
        std::fs::write(dir.join("world.obs"), to_bytes(&fake_world)).expect("write fake save");
        write_world_meta(
            &dir,
            &WorldMeta {
                light_version: LIGHT_VERSION,
                extra_lines: Vec::new(),
            },
        )
        .expect("write marker");

        let (world, migration) = load_world_dir_with_report(Arc::clone(&registry), &dir).expect("marked world loads");
        assert!(!migration.relit, "a v2 marker must suppress migration");
        assert_eq!(world.sky_light(cave), 15, "the loader trusted the stored light");
        assert_eq!(world.light_version(), LIGHT_VERSION);
        std::fs::remove_dir_all(&dir).expect("remove temp world");
    }

    #[test]
    fn new_worlds_write_the_v2_marker_and_save_format_4() {
        let registry = base_registry();
        let (world, cave) = {
            // A correctly seeded world, as a 1.2 host creates it.
            let mut world = World::new(Arc::clone(&registry), 42);
            let (fake, _) = fake_light_cave_world(&registry, ChunkPos::new(0, 0));
            for (_, chunk) in fake.chunks() {
                world.insert_loaded_chunk(chunk.clone());
            }
            world.relight_all();
            (world, BlockPos::new(7, 65, 7))
        };
        assert_eq!(world.light_version(), LIGHT_VERSION);
        let dir = unique_world_dir("new-world");
        std::fs::create_dir_all(&dir).expect("create world dir");
        save_world_dir(&world, &dir).expect("save new world");
        let meta = read_world_meta(&dir).expect("read meta").expect("marker exists");
        assert_eq!(meta.light_version, LIGHT_VERSION, "new worlds write v2 immediately");
        let bytes = std::fs::read(dir.join("world.obs")).expect("read save");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 4);

        let (loaded, migration) = load_world_dir_with_report(Arc::clone(&registry), &dir).expect("new world reloads");
        assert!(!migration.relit, "a new 1.2 world must not relight");
        assert_eq!(loaded.sky_light(cave), 0);
        std::fs::remove_dir_all(&dir).expect("remove temp world");
    }
}

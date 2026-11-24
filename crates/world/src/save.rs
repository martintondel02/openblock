//! World persistence (region format v1 arrives at MS-13; this is the
//! deterministic v0 snapshot format for the v0.0.1 tech preview).
//!
//! The snapshot is a single little-endian byte stream:
//!
//! ```text
//! "OBSV" | version u16 | seed u64 | chunk_count u32
//! chunk_count x chunk
//! crc32c u32
//! ```
//!
//! A chunk stores its XZ position followed by all 24 sections in ascending Y
//! order. Each section records its storage mode, packed block indices, and
//! packed light (or the uniform-zero light fast path). The trailing CRC32C
//! covers every preceding byte. Chunks are written sorted by `(cx, cz)` so the
//! encoding is independent of hash-map iteration order.

use std::sync::Arc;

use ob_content::{BlockId, Registry};
use ob_math::{ChunkPos, N_SECTIONS, SECTION_SIZE};
use rustc_hash::FxHashSet;

use crate::chunk::ChunkColumn;
use crate::light::Light;
use crate::section::{BLOCK_COUNT, Section, SectionStorage};
use crate::world::World;

/// Snapshot magic at the start of every byte stream.
const MAGIC: &[u8; 4] = b"OBSV";
/// Snapshot format version understood by this build.
const VERSION: u16 = 1;
/// Bytes before the first chunk: magic, version, seed, chunk count.
const HEADER_LEN: usize = 4 + 2 + 8 + 4;
/// Bytes in the trailing checksum.
const CRC_LEN: usize = 4;
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
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::InvalidFormat(reason) => write!(f, "invalid world save: {reason}"),
            SaveError::UnsupportedVersion(v) => write!(f, "unsupported save version {v}"),
            SaveError::UnknownBlock(id) => write!(f, "save references unknown block id {id}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Bookkeeping type kept for source compatibility with the v0.0.1 world API.
///
/// The snapshot encoder and decoder are free functions; this marker carries no
/// state yet.
#[derive(Debug, Clone, Default)]
pub struct WorldSave;

/// Serializes a world to the v0 snapshot format.
///
/// Chunks are emitted in ascending `(cx, cz)` order so repeated saves of the
/// same world produce identical bytes. The trailing CRC32C covers the header,
/// every chunk, and is written last (not included in its own checksum).
pub fn to_bytes(world: &World) -> Vec<u8> {
    let mut chunks: Vec<&ChunkColumn> = world.chunks().map(|(_, chunk)| chunk).collect();
    chunks.sort_by_key(|chunk| (chunk.pos().x, chunk.pos().z));

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&world.seed().to_le_bytes());
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());

    for chunk in chunks {
        let pos = chunk.pos();
        out.extend_from_slice(&pos.x.to_le_bytes());
        out.extend_from_slice(&pos.z.to_le_bytes());
        for (_, section) in chunk.sections() {
            write_section(&mut out, section);
        }
    }

    let checksum = crc32c(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Loads a world from the v0 snapshot format.
///
/// The whole stream is validated before a [`World`] is returned: header,
/// version, checksum, palette bounds against `registry`, section word counts,
/// and the absence of trailing bytes. Malformed input yields a [`SaveError`]
/// and never panics.
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
    if version != VERSION {
        return Err(SaveError::InvalidFormat("unknown format version"));
    }
    if bytes.len() < HEADER_LEN + CRC_LEN {
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
    let _magic = reader.read_exact::<4>()?;
    let _version = reader.read_u16()?;
    let seed = reader.read_u64()?;
    let chunk_count = reader.read_u32()?;

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
            sections.push(read_section(&mut reader, &registry)?);
        }
        world.insert_chunk(ChunkColumn::from_sections(pos, sections));
    }

    if reader.remaining() != 0 {
        return Err(SaveError::InvalidFormat("trailing bytes after chunks"));
    }
    Ok(world)
}

/// Writes one section's storage mode, palette/words, and light.
fn write_section(out: &mut Vec<u8>, section: &Section) {
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

    match section.light().raw() {
        None => out.push(0),
        Some(data) => {
            out.push(1);
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
fn read_section(reader: &mut Reader<'_>, registry: &Registry) -> Result<Section, SaveError> {
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
    let light = read_light(reader)?;
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

/// Reads a section's light payload.
fn read_light(reader: &mut Reader<'_>) -> Result<Light, SaveError> {
    match reader.read_u8()? {
        0 => Ok(Light::default()),
        1 => {
            if reader.remaining() < BLOCK_COUNT * 2 {
                return Err(SaveError::InvalidFormat("truncated light data"));
            }
            let mut data = Box::new([0u16; BLOCK_COUNT]);
            for slot in data.iter_mut() {
                *slot = reader.read_u16()?;
            }
            Ok(Light::from_raw(Some(data)))
        }
        _ => Err(SaveError::InvalidFormat("invalid light presence byte")),
    }
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
        let registry = base_registry();
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
                assert_eq!(expected_section.light().raw(), actual_section.light().raw());
                assert_eq!(expected_section.non_air_count(), actual_section.non_air_count());
                for index in 0..BLOCK_COUNT {
                    let local = LocalPos::from_index(index);
                    assert_eq!(
                        expected_section.get(local),
                        actual_section.get(local),
                        "block mismatch in {expected_section_pos:?} at {local:?}"
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
        // All sections stay uniform air, but zero the light arrays so the
        // fixture is small enough to mutate exhaustively in tests.
        for index in 0..N_SECTIONS {
            let y = WORLD_MIN_Y / 16 + index as i32;
            let section = column
                .section_mut(SectionPos::new(0, y, 0))
                .expect("section belongs to the column");
            *section.light_mut() = Light::uniform_sky(0);
        }
        world.insert_chunk(column);
        world
    }

    fn resign(bytes: &mut [u8]) {
        let payload_len = bytes.len() - CRC_LEN;
        let checksum = crc32c(&bytes[..payload_len]);
        bytes[payload_len..].copy_from_slice(&checksum.to_le_bytes());
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
    fn save_is_deterministic_and_stable_after_load() {
        let (registry, world) = sample_world();
        let first = to_bytes(&world);
        assert_eq!(first, to_bytes(&world));

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
        assert_eq!(bytes.len(), HEADER_LEN + CRC_LEN);
        assert!(bytes.starts_with(MAGIC));
        assert_eq!(
            u16::from_le_bytes([bytes[4], bytes[5]]),
            VERSION,
            "empty snapshot uses the current version"
        );

        let loaded = from_bytes(Arc::clone(&registry), &bytes).expect("empty snapshot is valid");
        assert_eq!(loaded.seed(), 42);
        assert_eq!(loaded.chunk_count(), 0);
        assert_eq!(to_bytes(&loaded), bytes);
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
        for len in (HEADER_LEN + CRC_LEN)..compact_bytes.len() {
            let mut truncated = compact_bytes[..len].to_vec();
            resign(&mut truncated);
            assert!(
                from_bytes(Arc::clone(&registry), &truncated).is_err(),
                "re-checksummed truncated stream of {len} bytes was accepted"
            );
        }
    }

    #[test]
    fn newer_version_is_unsupported() {
        let (registry, world) = sample_world();
        let mut bytes = to_bytes(&world);
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        match from_bytes(registry, &bytes) {
            Err(SaveError::UnsupportedVersion(2)) => {}
            other => panic!("expected UnsupportedVersion(2), got {other:?}"),
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
}

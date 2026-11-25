//! Deterministic chunk-column snapshot codec.
//!
//! v1 uses a raw (optionally zstd-compressed) paletted layout. Every section is
//! written in ascending `y` order:
//!
//! ```text
//! storage: u8 mode (0 uniform, 1 bits4, 2 bits8, 3 bits16)
//!          uniform: u16 block id
//!          paletted: u16 palette_len, palette_len x u16 block id,
//!                    fixed number of u64 index words (256/512/1024)
//! light:   u8 flag (0 uniform zero, 1 dense 4096 x u16, 2 uniform non-zero + u16)
//! ```
//!
//! The whole body is prefixed by `"OBC1"`, the chunk coordinates, a section
//! count and the compression/raw lengths. zstd is used at level 1 when it makes
//! the payload smaller; otherwise the raw body is stored. The format is
//! deterministic: encoding the same column twice produces identical bytes.
//!
//! A decoder validates the magic, coordinate match, section count, every block
//! id against the supplied registry, every palette index, both length bounds
//! and the absence of trailing bytes. It never panics on malformed input.

use ob_content::{BlockId, Registry};
use ob_math::{ChunkPos, N_SECTIONS, SECTION_SIZE};
use ob_world::section::SectionStorage;
use ob_world::{ChunkColumn, Light, Section};
use zstd::prelude::{ZSTD_compress, ZSTD_compressBound, ZSTD_decompress, ZSTD_isError};

use crate::ProtocolError;

/// Magic bytes at the start of an encoded chunk (`OBC` plus version `1`).
const CHUNK_MAGIC: &[u8; 4] = b"OBC1";
/// Maximum encoded chunk payload accepted by the decoder (1 MiB).
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Maximum uncompressed chunk body accepted by the decoder (1 MiB).
pub const MAX_CHUNK_BODY_BYTES: usize = 1024 * 1024;
/// Compression flag: body is stored verbatim.
const COMPRESSION_NONE: u8 = 0;
/// Compression flag: body is a zstd frame.
const COMPRESSION_ZSTD: u8 = 1;
/// zstd level used for chunk snapshots.
const ZSTD_LEVEL: i32 = 1;
/// Packed words in a 4-bit section (4,096 indices at 16 per word).
const BITS4_WORDS: usize = 256;
/// Packed words in an 8-bit section (4,096 indices at 8 per word).
const BITS8_WORDS: usize = 512;
/// Packed words in a 16-bit section (4,096 indices at 4 per word).
const BITS16_WORDS: usize = 1024;

/// Encodes a whole chunk column deterministically.
///
/// The result may be longer than [`MAX_CHUNK_BYTES`] for pathological custom
/// packs; callers that put it on the wire should use
/// [`encode_chunk_checked`] or reject oversized results themselves.
pub fn encode_chunk(registry: &Registry, column: &ChunkColumn) -> Vec<u8> {
    let raw = encode_body(registry, column);
    let mut compressed = vec![0u8; ZSTD_compressBound(raw.len())];
    let compressed_len = ZSTD_compress(&mut compressed, &raw, ZSTD_LEVEL);
    let (flag, body): (u8, &[u8]) = if !ZSTD_isError(compressed_len) && compressed_len < raw.len() {
        compressed.truncate(compressed_len);
        (COMPRESSION_ZSTD, &compressed)
    } else {
        (COMPRESSION_NONE, &raw)
    };

    let pos = column.pos();
    let mut out = Vec::with_capacity(22 + body.len());
    out.extend_from_slice(CHUNK_MAGIC);
    out.extend_from_slice(&pos.x.to_le_bytes());
    out.extend_from_slice(&pos.z.to_le_bytes());
    out.push(N_SECTIONS as u8);
    out.push(flag);
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Encodes a chunk and rejects payloads above [`MAX_CHUNK_BYTES`].
pub fn encode_chunk_checked(registry: &Registry, column: &ChunkColumn) -> Result<Vec<u8>, ProtocolError> {
    let bytes = encode_chunk(registry, column);
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(ProtocolError::ChunkTooLarge {
            len: bytes.len(),
            max: MAX_CHUNK_BYTES,
        });
    }
    Ok(bytes)
}

/// Decodes a chunk snapshot and verifies that it belongs to `(cx, cz)`.
///
/// Block ids are validated against `registry`; a snapshot produced with a
/// different content pack is rejected rather than silently reinterpreted.
pub fn decode_chunk(registry: &Registry, cx: i32, cz: i32, bytes: &[u8]) -> Result<ChunkColumn, ProtocolError> {
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(ProtocolError::ChunkTooLarge {
            len: bytes.len(),
            max: MAX_CHUNK_BYTES,
        });
    }
    let mut reader = ChunkReader::new(bytes);
    let magic = reader.take(4)?;
    if magic != CHUNK_MAGIC {
        return Err(ProtocolError::InvalidChunk("bad magic"));
    }
    let encoded_cx = reader.read_i32()?;
    let encoded_cz = reader.read_i32()?;
    if encoded_cx != cx || encoded_cz != cz {
        return Err(ProtocolError::InvalidChunk("chunk coordinate mismatch"));
    }
    let section_count = reader.read_u8()?;
    if section_count as usize != N_SECTIONS {
        return Err(ProtocolError::InvalidChunk("unexpected section count"));
    }
    let compression = reader.read_u8()?;
    let raw_len = reader.read_u32()? as usize;
    let body_len = reader.read_u32()? as usize;
    if raw_len > MAX_CHUNK_BODY_BYTES {
        return Err(ProtocolError::ChunkTooLarge {
            len: raw_len,
            max: MAX_CHUNK_BODY_BYTES,
        });
    }
    if body_len > MAX_CHUNK_BYTES || body_len > reader.remaining() {
        return Err(ProtocolError::InvalidChunk("truncated chunk body"));
    }
    let body = reader.take(body_len)?;
    if reader.remaining() != 0 {
        return Err(ProtocolError::InvalidChunk("trailing bytes after chunk body"));
    }
    let raw = match compression {
        COMPRESSION_NONE => {
            if body_len != raw_len {
                return Err(ProtocolError::InvalidChunk("raw length mismatch"));
            }
            body.to_vec()
        }
        COMPRESSION_ZSTD => {
            let mut decompressed = vec![0u8; raw_len];
            let written = ZSTD_decompress(&mut decompressed, body);
            if ZSTD_isError(written) || written != raw_len {
                return Err(ProtocolError::InvalidChunk("zstd decompression failed"));
            }
            decompressed
        }
        _ => return Err(ProtocolError::InvalidChunk("unknown compression flag")),
    };

    let mut body_reader = ChunkReader::new(&raw);
    let mut sections = Vec::with_capacity(N_SECTIONS);
    for _ in 0..N_SECTIONS {
        sections.push(read_section(&mut body_reader, registry)?);
    }
    if body_reader.remaining() != 0 {
        return Err(ProtocolError::InvalidChunk("trailing bytes in chunk body"));
    }
    Ok(ChunkColumn::from_sections(ChunkPos::new(cx, cz), sections))
}

/// Writes the raw block/light body for every section, ascending by `y`.
fn encode_body(registry: &Registry, column: &ChunkColumn) -> Vec<u8> {
    let mut out = Vec::with_capacity(N_SECTIONS * 16);
    for (_, section) in column.sections() {
        write_storage(&mut out, section);
        write_light(&mut out, section.light());
    }
    // Keep the registry in the signature: it documents that block ids are
    // registry indices even at encode time, and catches accidental misuse in
    // debug builds.
    debug_assert!(column.sections().count() == N_SECTIONS);
    debug_assert!(registry.len() <= ob_content::MAX_BLOCKS);
    out
}

/// Writes one section's paletted block storage.
fn write_storage(out: &mut Vec<u8>, section: &Section) {
    match section.storage() {
        SectionStorage::Uniform(id) => {
            out.push(0);
            out.extend_from_slice(&id.0.to_le_bytes());
        }
        SectionStorage::Bits4 { .. } => write_paletted(out, 1, section, BITS4_WORDS),
        SectionStorage::Bits8 { .. } => write_paletted(out, 2, section, BITS8_WORDS),
        SectionStorage::Bits16 { .. } => write_paletted(out, 3, section, BITS16_WORDS),
    }
}

/// Writes a paletted section body with exactly `word_count` packed words.
fn write_paletted(out: &mut Vec<u8>, mode: u8, section: &Section, word_count: usize) {
    let palette = section.palette();
    out.push(mode);
    let palette_len = u16::try_from(palette.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&palette_len.to_le_bytes());
    for id in palette.iter().take(usize::from(palette_len)) {
        out.extend_from_slice(&id.0.to_le_bytes());
    }
    let words = section.words();
    for index in 0..word_count {
        let word = words.get(index).copied().unwrap_or(0);
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Writes one section's light in the compact uniform/dense encoding.
fn write_light(out: &mut Vec<u8>, light: &Light) {
    match light.uniform_value() {
        Some(0) => out.push(0),
        Some(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => {
            out.push(1);
            let words = light.raw().expect("non-uniform light has dense storage");
            for word in words.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// Reads one section and validates every id, palette index and light flag.
fn read_section(reader: &mut ChunkReader<'_>, registry: &Registry) -> Result<Section, ProtocolError> {
    let mode = reader.read_u8()?;
    let storage = match mode {
        0 => {
            let id = BlockId(reader.read_u16()?);
            validate_block(registry, id)?;
            SectionStorage::Uniform(id)
        }
        1 => read_paletted(reader, registry, 4, 16, BITS4_WORDS)?,
        2 => read_paletted(reader, registry, 8, 256, BITS8_WORDS)?,
        3 => read_paletted(reader, registry, 16, usize::from(u16::MAX), BITS16_WORDS)?,
        _ => return Err(ProtocolError::InvalidChunk("unknown storage mode")),
    };
    let light = read_light(reader)?;
    Ok(Section::from_parts(storage, light))
}

/// Reads a paletted storage body, checking the palette length and indices.
fn read_paletted(
    reader: &mut ChunkReader<'_>,
    registry: &Registry,
    bits: u8,
    palette_capacity: usize,
    word_count: usize,
) -> Result<SectionStorage, ProtocolError> {
    let palette_len = usize::from(reader.read_u16()?);
    if palette_len == 0 {
        return Err(ProtocolError::InvalidChunk("empty palette"));
    }
    if palette_len > palette_capacity {
        return Err(ProtocolError::InvalidChunk("palette too large"));
    }
    if reader.remaining() < palette_len * 2 + word_count * 8 {
        return Err(ProtocolError::InvalidChunk("truncated section"));
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
    Ok(match bits {
        4 => SectionStorage::Bits4 { words, palette },
        8 => SectionStorage::Bits8 { words, palette },
        16 => SectionStorage::Bits16 { words, palette },
        _ => return Err(ProtocolError::InvalidChunk("unknown storage width")),
    })
}

/// Checks that every packed index addresses an existing palette entry.
fn validate_indices(bits: u8, words: &[u64], palette_len: usize) -> Result<(), ProtocolError> {
    let per_word = 64 / usize::from(bits);
    let mask = (1u64 << bits) - 1;
    for word in words {
        for slot in 0..per_word {
            let index = ((word >> (slot * usize::from(bits))) & mask) as usize;
            if index >= palette_len {
                return Err(ProtocolError::InvalidChunk("palette index out of range"));
            }
        }
    }
    Ok(())
}

/// Reads one section's light payload.
fn read_light(reader: &mut ChunkReader<'_>) -> Result<Light, ProtocolError> {
    match reader.read_u8()? {
        0 => Ok(Light::from_uniform(0)),
        1 => {
            if reader.remaining() < SECTION_SIZE * SECTION_SIZE * SECTION_SIZE * 2 {
                return Err(ProtocolError::InvalidChunk("truncated light data"));
            }
            let mut data = Box::new([0u16; SECTION_SIZE * SECTION_SIZE * SECTION_SIZE]);
            for slot in data.iter_mut() {
                *slot = reader.read_u16()?;
            }
            Ok(Light::from_raw(Some(data)))
        }
        2 => {
            let value = reader.read_u16()?;
            if value == 0 {
                return Err(ProtocolError::InvalidChunk("uniform light value must be non-zero"));
            }
            Ok(Light::from_uniform(value))
        }
        _ => Err(ProtocolError::InvalidChunk("invalid light encoding flag")),
    }
}

/// Rejects block ids the session registry cannot resolve.
fn validate_block(registry: &Registry, id: BlockId) -> Result<(), ProtocolError> {
    if usize::from(id.0) < registry.len() {
        Ok(())
    } else {
        Err(ProtocolError::InvalidChunk("block id is not in the registry"))
    }
}

/// Bounds-checked cursor over an encoded chunk payload.
struct ChunkReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ChunkReader<'a> {
    /// Creates a cursor at the start of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Reads a fixed-size slice or reports truncation.
    fn take(&mut self, length: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .pos
            .checked_add(length)
            .ok_or(ProtocolError::InvalidChunk("length overflow"))?;
        if end > self.bytes.len() {
            return Err(ProtocolError::InvalidChunk("truncated chunk payload"));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    fn read_u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u16`.
    fn read_u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a little-endian `i32`.
    fn read_i32(&mut self) -> Result<i32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a little-endian `u32`.
    fn read_u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a little-endian `u64`.
    fn read_u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ob_math::{BlockPos, LocalPos, SEA_LEVEL};

    fn registry() -> Registry {
        Registry::base()
    }

    fn compare_columns(left: &ChunkColumn, right: &ChunkColumn) {
        assert_eq!(left.pos(), right.pos());
        for y in ob_math::WORLD_MIN_Y..ob_math::WORLD_MAX_Y {
            for z in 0..SECTION_SIZE {
                for x in 0..SECTION_SIZE {
                    let pos = BlockPos::new(
                        left.pos().x * SECTION_SIZE as i32 + x as i32,
                        y,
                        left.pos().z * SECTION_SIZE as i32 + z as i32,
                    );
                    assert_eq!(left.get_block(pos), right.get_block(pos), "block at {pos:?}");
                }
            }
        }
        for z in 0..SECTION_SIZE {
            for x in 0..SECTION_SIZE {
                assert_eq!(left.height_at(x, z), right.height_at(x, z), "height {x},{z}");
            }
        }
    }

    fn synthetic_column(pos: ChunkPos, registry: &Registry) -> ChunkColumn {
        let mut column = ChunkColumn::new(pos);
        let stone = registry.id_or_air("openblock:stone");
        let water = registry.id_or_air("openblock:water");
        let dirt = registry.id_or_air("openblock:dirt");
        let grass = registry.id_or_air("openblock:grass");
        let (ox, oz) = pos.origin();
        for z in 0..SECTION_SIZE {
            for x in 0..SECTION_SIZE {
                for y in ob_math::WORLD_MIN_Y..=SEA_LEVEL {
                    let block = if y < SEA_LEVEL - 4 {
                        // A deterministic "ore" pattern using a distinct block
                        // id, plus stone.
                        if (i64::from(x as i32) * 7 + i64::from(z as i32) * 13 + i64::from(y)).rem_euclid(37) == 0 {
                            dirt
                        } else {
                            stone
                        }
                    } else if y < SEA_LEVEL {
                        water
                    } else {
                        grass
                    };
                    column.set_block(BlockPos::new(ox + x as i32, y, oz + z as i32), block);
                }
            }
        }
        column
    }

    #[test]
    fn empty_column_round_trips() {
        let registry = registry();
        let column = ChunkColumn::new(ChunkPos::new(-2, 5));
        let bytes = encode_chunk(&registry, &column);
        let decoded = decode_chunk(&registry, -2, 5, &bytes).expect("decode");
        compare_columns(&column, &decoded);
    }

    #[test]
    fn generated_columns_round_trip() {
        let registry = registry();
        for (seed, pos) in [(42u64, ChunkPos::new(0, 0)), (7, ChunkPos::new(-3, 4))] {
            let generator = ob_worldgen::Generator::new(seed);
            let column = generator.generate_chunk(pos, &registry);
            let bytes = encode_chunk(&registry, &column);
            let decoded = decode_chunk(&registry, pos.x, pos.z, &bytes).expect("decode");
            compare_columns(&column, &decoded);
        }
    }

    #[test]
    fn water_and_synthetic_ore_columns_round_trip() {
        let registry = registry();
        let column = synthetic_column(ChunkPos::new(3, -4), &registry);
        let bytes = encode_chunk(&registry, &column);
        let decoded = decode_chunk(&registry, 3, -4, &bytes).expect("decode");
        compare_columns(&column, &decoded);
    }

    #[test]
    fn encoding_is_deterministic() {
        let registry = registry();
        let generator = ob_worldgen::Generator::new(99);
        let column = generator.generate_chunk(ChunkPos::new(2, -1), &registry);
        let first = encode_chunk(&registry, &column);
        let second = encode_chunk(&registry, &column);
        assert_eq!(first, second);
        assert!(first.len() <= MAX_CHUNK_BYTES);
    }

    #[test]
    fn light_values_round_trip() {
        let registry = registry();
        let mut column = ChunkColumn::new(ChunkPos::new(0, 0));
        let section = column.section_mut(ob_math::SectionPos::new(0, -1, 0)).expect("section");
        section.light_mut().set_sky(LocalPos { x: 1, y: 2, z: 3 }, 9);
        section.light_mut().set_block(LocalPos { x: 4, y: 5, z: 6 }, 4, 5, 6);
        let bytes = encode_chunk(&registry, &column);
        let decoded = decode_chunk(&registry, 0, 0, &bytes).expect("decode");
        let restored = decoded.section(ob_math::SectionPos::new(0, -1, 0)).expect("section");
        let pos = LocalPos { x: 1, y: 2, z: 3 };
        assert_eq!(restored.light().sky(pos), 9);
        assert_eq!(restored.light().block_rgb(LocalPos { x: 4, y: 5, z: 6 }), (4, 5, 6));
    }

    #[test]
    fn malformed_payloads_are_rejected() {
        let registry = registry();
        let column = synthetic_column(ChunkPos::new(1, 1), &registry);
        let good = encode_chunk(&registry, &column);
        assert!(decode_chunk(&registry, 1, 1, &good).is_ok());

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(decode_chunk(&registry, 1, 1, &bad_magic).is_err());

        let mut bad_coords = good.clone();
        bad_coords[4] ^= 0xff;
        assert!(decode_chunk(&registry, 1, 1, &bad_coords).is_err());

        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_chunk(&registry, 1, 1, &trailing).is_err());

        let truncated = &good[..good.len() / 2];
        assert!(decode_chunk(&registry, 1, 1, truncated).is_err());

        assert!(decode_chunk(&registry, 1, 1, &[]).is_err());
        assert!(matches!(
            decode_chunk(&registry, 1, 1, &vec![0; MAX_CHUNK_BYTES + 1]),
            Err(ProtocolError::ChunkTooLarge { .. })
        ));
    }

    #[test]
    fn unknown_block_ids_are_rejected() {
        let registry = registry();
        let column = ChunkColumn::new(ChunkPos::new(0, 0));
        let mut bytes = encode_chunk(&registry, &column);
        // The first section is uniform air: header(22) + mode(1) + block(2).
        bytes[23] = 0xff;
        bytes[24] = 0xff;
        assert!(decode_chunk(&registry, 0, 0, &bytes).is_err());
    }

    #[test]
    fn deterministic_random_mutations_never_panic() {
        let registry = registry();
        let column = synthetic_column(ChunkPos::new(0, 0), &registry);
        let base = encode_chunk(&registry, &column);
        let mut state = 0x1234_5678_9abc_def0u64;
        for iteration in 0..250 {
            let mut mutated = base.clone();
            for _ in 0..(iteration % 4 + 1) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
                let index = (state as usize) % mutated.len();
                mutated[index] = (state >> 32) as u8;
            }
            let result = std::panic::catch_unwind(|| decode_chunk(&registry, 0, 0, &mutated));
            assert!(result.is_ok(), "chunk decoder panicked on mutation {iteration}");
            if let Ok(Ok(decoded)) = result {
                // A mutation may still form a valid payload; decoding it must
                // have produced a structurally valid column.
                assert_eq!(decoded.pos(), ChunkPos::new(0, 0));
            }
        }
    }
}

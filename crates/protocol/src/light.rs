//! Compact per-section light patches for [`crate::ServerMessage::LightUpdate`].
//!
//! A light patch carries the canonical **dense** 4,096 packed words of one
//! 16³ section, in `LocalPos::from_index` order, exactly as the chunk codec
//! stores them. Two encodings are defined:
//!
//! * raw little-endian `u16` words (8,192 bytes), and
//! * a zstd frame of those bytes (used only when it is strictly smaller).
//!
//! The encoding is a self-describing flag on [`crate::LightSectionDto`], so a client
//! rejects an unknown flag instead of misreading the bytes. Patches are
//! intentionally dense: a section can change anywhere inside the 31³ relight
//! box, and a sparse delta would need per-cell indices that cost more than the
//! compressed words for the small number of sections involved.
//!
//! The protocol remains **v3**: this is an additive server message, and the
//! per-section `format` field lets a later revision change the payload without
//! guessing.

use ob_math::SECTION_SIZE;
use zstd::prelude::{ZSTD_compress, ZSTD_compressBound, ZSTD_decompress, ZSTD_isError};

use crate::ProtocolError;

/// Number of packed light words in one section (4,096 cells).
pub const LIGHT_CELLS: usize = SECTION_SIZE * SECTION_SIZE * SECTION_SIZE;
/// Bytes of one raw dense light section (`LIGHT_CELLS * 2`).
pub const LIGHT_WORDS_BYTES: usize = LIGHT_CELLS * 2;
/// Payload format implemented by this build.
pub const LIGHT_SECTION_FORMAT: u8 = 1;
/// Payload bytes are raw little-endian `u16` words.
pub const LIGHT_ENCODING_RAW: u8 = 0;
/// Payload bytes are a zstd frame of the raw little-endian words.
pub const LIGHT_ENCODING_ZSTD: u8 = 1;
/// Largest accepted encoded payload for one section.
pub const MAX_LIGHT_SECTION_BYTES: usize = LIGHT_WORDS_BYTES;
/// zstd level used for light patches (matches the chunk codec).
const ZSTD_LEVEL: i32 = 1;

/// Encodes one section's dense light words.
///
/// `words` must contain exactly [`LIGHT_CELLS`] entries. The raw encoding is
/// used when compression does not make the payload strictly smaller, so the
/// result is deterministic for a given input. Returns `(encoding, bytes)`.
pub fn encode_light_words(words: &[u16]) -> Result<(u8, Vec<u8>), ProtocolError> {
    if words.len() != LIGHT_CELLS {
        return Err(ProtocolError::InvalidLight("light section must contain 4096 words"));
    }
    let mut raw = Vec::with_capacity(LIGHT_WORDS_BYTES);
    for word in words {
        raw.extend_from_slice(&word.to_le_bytes());
    }
    let mut compressed = vec![0u8; ZSTD_compressBound(raw.len())];
    let compressed_len = ZSTD_compress(&mut compressed, &raw, ZSTD_LEVEL);
    if !ZSTD_isError(compressed_len) && compressed_len < raw.len() {
        compressed.truncate(compressed_len);
        Ok((LIGHT_ENCODING_ZSTD, compressed))
    } else {
        Ok((LIGHT_ENCODING_RAW, raw))
    }
}

/// Decodes one section patch into exactly [`LIGHT_CELLS`] packed words.
///
/// Unknown encodings, wrong sizes and corrupt zstd frames are rejected without
/// panicking; the result is safe to use against a hostile peer.
pub fn decode_light_words(encoding: u8, data: &[u8]) -> Result<Vec<u16>, ProtocolError> {
    if data.len() > MAX_LIGHT_SECTION_BYTES {
        return Err(ProtocolError::InvalidLight("light section payload is too large"));
    }
    let raw = match encoding {
        LIGHT_ENCODING_RAW => {
            if data.len() != LIGHT_WORDS_BYTES {
                return Err(ProtocolError::InvalidLight("raw light section has the wrong length"));
            }
            data.to_vec()
        }
        LIGHT_ENCODING_ZSTD => {
            let mut decompressed = vec![0u8; LIGHT_WORDS_BYTES];
            let written = ZSTD_decompress(&mut decompressed, data);
            if ZSTD_isError(written) || written != LIGHT_WORDS_BYTES {
                return Err(ProtocolError::InvalidLight("zstd light section decompression failed"));
            }
            decompressed
        }
        _ => return Err(ProtocolError::InvalidLight("unknown light section encoding")),
    };
    let mut words = Vec::with_capacity(LIGHT_CELLS);
    let (pairs, remainder) = raw.as_chunks::<2>();
    debug_assert!(remainder.is_empty(), "a raw light section is an even byte length");
    for pair in pairs {
        words.push(u16::from_le_bytes(*pair));
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dense_words() -> Vec<u16> {
        (0..LIGHT_CELLS)
            .map(|index| {
                let sky = (index % 16) as u16;
                let red = ((index / 16) % 16) as u16;
                let green = ((index / 256) % 16) as u16;
                let blue = ((index / 4096) % 16) as u16;
                (sky << 12) | (red << 8) | (green << 4) | blue
            })
            .collect()
    }

    #[test]
    fn dense_words_round_trip_in_both_encodings() {
        let words = dense_words();
        let (encoding, bytes) = encode_light_words(&words).expect("encode dense light");
        assert_eq!(
            encoding, LIGHT_ENCODING_ZSTD,
            "this fixture compresses below the raw size"
        );
        assert_eq!(decode_light_words(encoding, &bytes).expect("decode"), words);

        let uniform = vec![0x0F5Au16; LIGHT_CELLS];
        let (encoding, bytes) = encode_light_words(&uniform).expect("encode uniform light");
        assert!(encoding == LIGHT_ENCODING_RAW || encoding == LIGHT_ENCODING_ZSTD);
        assert_eq!(decode_light_words(encoding, &bytes).expect("decode"), uniform);

        let zero = vec![0u16; LIGHT_CELLS];
        let (encoding, bytes) = encode_light_words(&zero).expect("encode zero light");
        assert_eq!(decode_light_words(encoding, &bytes).expect("decode"), zero);
    }

    #[test]
    fn malformed_patches_are_rejected_without_panicking() {
        assert!(encode_light_words(&[0; LIGHT_CELLS - 1]).is_err());
        assert!(decode_light_words(LIGHT_ENCODING_RAW, &[0; 3]).is_err());
        assert!(decode_light_words(LIGHT_ENCODING_ZSTD, b"not a zstd frame").is_err());
        assert!(decode_light_words(7, &vec![0; LIGHT_WORDS_BYTES]).is_err());
        assert!(decode_light_words(LIGHT_ENCODING_RAW, &vec![0; MAX_LIGHT_SECTION_BYTES + 1]).is_err());
    }

    #[test]
    fn encoding_is_deterministic() {
        let words = dense_words();
        let first = encode_light_words(&words).expect("first");
        let second = encode_light_words(&words).expect("second");
        assert_eq!(first, second);
    }
    #[test]
    fn a_full_light_batch_fits_the_frame_budget() {
        // Base64 of a worst-case raw batch (incompressible sections) must stay
        // under the protocol frame limit even before JSON overhead is counted.
        let base64_per_section = LIGHT_WORDS_BYTES.div_ceil(3) * 4;
        assert!(
            crate::MAX_LIGHT_SECTIONS_PER_UPDATE * base64_per_section < crate::MAX_FRAME_LEN,
            "MAX_LIGHT_SECTIONS_PER_UPDATE must fit MAX_FRAME_LEN"
        );
    }
}

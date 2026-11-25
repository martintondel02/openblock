//! Minimal strict base64 used to carry chunk bytes in the JSON wire format.
//!
//! The interim protocol is JSON, so a `Vec<u8>` serialised naively becomes a
//! JSON array of numbers and roughly quadruples in size. Chunk payloads are
//! therefore encoded as a standard padded base64 string (RFC 4648 alphabet),
//! which keeps frames small without adding a dependency.

/// Standard base64 alphabet (`A-Z`, `a-z`, `0-9`, `+`, `/`).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes bytes as padded standard base64.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = u32::from(chunk[0]);
        let b = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let c = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let value = (a << 16) | (b << 8) | c;
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(value & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decodes padded standard base64, rejecting malformed input.
pub(crate) fn decode(text: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err("base64 length is not a multiple of four");
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let chunk = &bytes[index..index + 4];
        index += 4;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 {
            return Err("base64 padding is invalid");
        }
        if padding > 0 && index != bytes.len() {
            return Err("base64 padding appears before the end");
        }
        let mut value = 0u32;
        for (offset, byte) in chunk.iter().enumerate() {
            let digit = if *byte == b'=' {
                0
            } else {
                decode_digit(*byte).ok_or("base64 contains an invalid character")?
            };
            if *byte == b'=' && offset < 4 - padding {
                return Err("base64 padding is not trailing");
            }
            value = (value << 6) | u32::from(digit);
        }
        out.push((value >> 16) as u8);
        if padding < 2 {
            out.push((value >> 8) as u8);
        }
        if padding < 1 {
            out.push(value as u8);
        }
    }
    Ok(out)
}

/// Maps one base64 digit to its six-bit value.
fn decode_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Serde adapter for `Vec<u8>` fields that travel as base64 strings.
pub(crate) mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialises a byte vector as a base64 string.
    pub(crate) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::encode(bytes))
    }

    /// Deserialises a base64 string into a byte vector.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::decode(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_remainder() {
        for length in 0..=32usize {
            let source: Vec<u8> = (0..length).map(|value| (value * 7 + 3) as u8).collect();
            let encoded = encode(&source);
            assert_eq!(decode(&encoded).expect("valid base64"), source);
        }
    }

    #[test]
    fn matches_known_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_malformed_input() {
        for text in ["A", "AA", "AAA", "!!!!", "Zg=a", "Z===", "Zg==Zg=="] {
            assert!(decode(text).is_err(), "{text:?} must be rejected");
        }
    }
}

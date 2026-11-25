//! Versioned JSON content-pack loading for blocks.
//!
//! This is the first slice of the data-driven content pipeline described in
//! `docs/spec/15-content-system.md`: it parses one `blocks.json` document,
//! validates every entry and produces the same [`BlockType`] vector that
//! [`Registry::from_blocks`] freezes into a [`Registry`]. The base game is not
//! wired to it yet; [`Registry::base`] is still the host default.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde_json::Value;

use crate::{BlockShape, BlockTextures, BlockType, Registry, TextureId, ToolKind, textures};

/// The only supported `format` version of the JSON schema.
pub const FORMAT_VERSION: u32 = 1;

/// Maximum accepted pack file size (8 MiB).
pub const MAX_PACK_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum accepted number of blocks in one pack.
pub const MAX_BLOCKS: usize = 4096;

/// Resource id every registry must declare at index 0.
pub(crate) const AIR_ID: &str = "openblock:air";

const DEBUG_TEXTURE_NAME: &str = textures::NAMES[0];

/// Errors produced while loading a content pack.
///
/// Loading errors always name the pack path; entry errors carry the block
/// index and resource id as well. [`Registry::from_blocks`] produces the same
/// entry variants without a path because it has no file context.
#[derive(Debug)]
pub enum PackError {
    /// The pack file could not be opened or read.
    Io {
        /// File that failed to load.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The pack file exceeds [`MAX_PACK_BYTES`].
    FileTooLarge {
        /// File that failed to load.
        path: PathBuf,
        /// Observed file size in bytes.
        size: u64,
        /// Configured limit in bytes.
        limit: u64,
    },
    /// The file is not valid JSON or does not match the pack schema.
    InvalidJson {
        /// File that failed to load.
        path: PathBuf,
        /// Parser or schema message, including line/column where available.
        message: String,
    },
    /// The `format` field is missing or is not [`FORMAT_VERSION`].
    UnsupportedFormat {
        /// File that failed to load.
        path: PathBuf,
        /// Human-readable description of the declared format.
        found: String,
        /// Schema version this build understands.
        expected: u32,
    },
    /// The block list is empty, so `openblock:air` is absent.
    MissingAir {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
    },
    /// More than [`MAX_BLOCKS`] blocks were declared.
    TooManyBlocks {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Number of declared blocks.
        count: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The first block is not `openblock:air`.
    InvalidOrder {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Resource id found at index 0.
        id: String,
    },
    /// Two blocks share one resource id.
    DuplicateId {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Duplicated resource id.
        id: String,
        /// Index of the first declaration.
        first: usize,
        /// Index of the second declaration.
        second: usize,
    },
    /// A block id does not follow the namespaced-id rule.
    InvalidId {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Offending resource id.
        id: String,
        /// Why the id is invalid.
        reason: &'static str,
    },
    /// A block color is not three finite channels in `0..=1`.
    InvalidColor {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Resource id of the offending block.
        id: String,
        /// Why the color is invalid.
        reason: String,
    },
    /// The optional hardness value is out of range.
    InvalidHardness {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Resource id of the offending block.
        id: String,
        /// Why the hardness is invalid.
        reason: String,
    },
    /// A texture name is not present in [`crate::textures::NAMES`].
    UnknownTexture {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Resource id of the offending block.
        id: String,
        /// Face the texture was declared for (`top`, `side` or `bottom`).
        face: &'static str,
        /// Unknown texture name.
        texture: String,
    },
    /// A block tool rule is inconsistent.
    InvalidToolRule {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Resource id of the offending block.
        id: String,
        /// Why the rule is invalid.
        reason: &'static str,
    },
    /// A light emission, attenuation or shape value is invalid.
    InvalidLight {
        /// File being loaded, when the error came from a pack.
        path: Option<PathBuf>,
        /// Index of the offending block.
        index: usize,
        /// Resource id of the offending block.
        id: String,
        /// Why the light data is invalid.
        reason: String,
    },
}

impl PackError {
    /// Attaches file context to an entry error produced by
    /// [`Registry::from_blocks`].
    fn with_path(self, path: &Path) -> Self {
        let path = Some(path.to_path_buf());
        match self {
            Self::MissingAir { .. } => Self::MissingAir { path },
            Self::TooManyBlocks { count, limit, .. } => Self::TooManyBlocks { path, count, limit },
            Self::InvalidOrder { id, .. } => Self::InvalidOrder { path, id },
            Self::DuplicateId { id, first, second, .. } => Self::DuplicateId {
                path,
                id,
                first,
                second,
            },
            Self::InvalidId { index, id, reason, .. } => Self::InvalidId {
                path,
                index,
                id,
                reason,
            },
            Self::InvalidColor { index, id, reason, .. } => Self::InvalidColor {
                path,
                index,
                id,
                reason,
            },
            Self::InvalidHardness { index, id, reason, .. } => Self::InvalidHardness {
                path,
                index,
                id,
                reason,
            },
            Self::UnknownTexture {
                index,
                id,
                face,
                texture,
                ..
            } => Self::UnknownTexture {
                path,
                index,
                id,
                face,
                texture,
            },
            Self::InvalidToolRule { index, id, reason, .. } => Self::InvalidToolRule {
                path,
                index,
                id,
                reason,
            },
            Self::InvalidLight { index, id, reason, .. } => Self::InvalidLight {
                path,
                index,
                id,
                reason,
            },
            other => other,
        }
    }
}

fn write_path(f: &mut fmt::Formatter<'_>, path: Option<&Path>) -> fmt::Result {
    if let Some(path) = path {
        write!(f, "{}: ", path.display())?;
    }
    Ok(())
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "{}: cannot read content pack: {source}", path.display())
            }
            Self::FileTooLarge { path, size, limit } => write!(
                f,
                "{}: content pack is {size} bytes, exceeding the {limit}-byte limit",
                path.display()
            ),
            Self::InvalidJson { path, message } => {
                write!(f, "{}: invalid content-pack JSON: {message}", path.display())
            }
            Self::UnsupportedFormat { path, found, expected } => write!(
                f,
                "{}: unsupported content format {found}; expected {expected}",
                path.display()
            ),
            Self::MissingAir { path } => {
                write_path(f, path.as_deref())?;
                f.write_str("block registry is empty; openblock:air must be the first block")
            }
            Self::TooManyBlocks { path, count, limit } => {
                write_path(f, path.as_deref())?;
                write!(
                    f,
                    "block registry has {count} blocks, exceeding the {limit}-block limit"
                )
            }
            Self::InvalidOrder { path, id } => {
                write_path(f, path.as_deref())?;
                write!(f, "block 0 must be \"openblock:air\", found {id:?}")
            }
            Self::DuplicateId {
                path,
                id,
                first,
                second,
            } => {
                write_path(f, path.as_deref())?;
                write!(
                    f,
                    "duplicate block id {id:?} at block {second} (first declared at block {first})"
                )
            }
            Self::InvalidId {
                path,
                index,
                id,
                reason,
            } => {
                write_path(f, path.as_deref())?;
                write!(f, "invalid block id {id:?} at block {index}: {reason}")
            }
            Self::InvalidColor {
                path,
                index,
                id,
                reason,
            } => {
                write_path(f, path.as_deref())?;
                write!(f, "invalid color for block {id:?} at block {index}: {reason}")
            }
            Self::InvalidHardness {
                path,
                index,
                id,
                reason,
            } => {
                write_path(f, path.as_deref())?;
                write!(f, "invalid hardness for block {id:?} at block {index}: {reason}")
            }
            Self::UnknownTexture {
                path,
                index,
                id,
                face,
                texture,
            } => {
                write_path(f, path.as_deref())?;
                write!(
                    f,
                    "unknown {face} texture {texture:?} for block {id:?} at block {index}; \
                     valid textures: {}",
                    textures::NAMES.join(", ")
                )
            }
            Self::InvalidToolRule {
                path,
                index,
                id,
                reason,
            } => {
                write_path(f, path.as_deref())?;
                write!(f, "invalid tool rule for block {id:?} at block {index}: {reason}")
            }
            Self::InvalidLight {
                path,
                index,
                id,
                reason,
            } => {
                write_path(f, path.as_deref())?;
                write!(f, "invalid light data for block {id:?} at block {index}: {reason}")
            }
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Loads `blocks.json` and returns its validated block definitions.
///
/// The first block must be `openblock:air`, ids must be unique and match the
/// namespaced-id rule, colors must be three finite channels in `0..=1`, and
/// texture names must exist in [`crate::textures::NAMES`]. Omitted flags
/// default to `false`, color defaults to `[1.0, 1.0, 1.0]` and textures default
/// to the debug layer.
///
/// [`BlockType::id`] is `&'static str`, so ids are interned for the process
/// lifetime. The per-load block count is capped at [`MAX_BLOCKS`].
pub fn load_blocks_json(path: &Path) -> Result<Vec<BlockType>, PackError> {
    let bytes = read_pack_file(path)?;
    let root: Value = serde_json::from_slice(&bytes).map_err(|error| PackError::InvalidJson {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let Some(object) = root.as_object() else {
        return Err(PackError::InvalidJson {
            path: path.to_path_buf(),
            message: "top-level value must be an object".to_owned(),
        });
    };

    let format = object.get("format");
    if !matches!(
        format,
        Some(Value::Number(number)) if number.as_u64() == Some(u64::from(FORMAT_VERSION))
    ) {
        return Err(PackError::UnsupportedFormat {
            path: path.to_path_buf(),
            found: describe_format(format),
            expected: FORMAT_VERSION,
        });
    }

    if let Some(field) = object.keys().find(|key| !matches!(key.as_str(), "format" | "blocks")) {
        return Err(PackError::InvalidJson {
            path: path.to_path_buf(),
            message: format!("unknown field {field:?} at the pack root"),
        });
    }

    let Some(blocks_value) = object.get("blocks") else {
        return Err(PackError::InvalidJson {
            path: path.to_path_buf(),
            message: "missing required field \"blocks\"".to_owned(),
        });
    };
    let Some(block_values) = blocks_value.as_array() else {
        return Err(PackError::InvalidJson {
            path: path.to_path_buf(),
            message: "\"blocks\" must be an array".to_owned(),
        });
    };
    if block_values.is_empty() {
        return Err(PackError::MissingAir {
            path: Some(path.to_path_buf()),
        });
    }
    if block_values.len() > MAX_BLOCKS {
        return Err(PackError::TooManyBlocks {
            path: Some(path.to_path_buf()),
            count: block_values.len(),
            limit: MAX_BLOCKS,
        });
    }

    let mut raw_blocks = Vec::with_capacity(block_values.len());
    for (index, value) in block_values.iter().enumerate() {
        let raw: RawBlock = serde_json::from_value(value.clone()).map_err(|error| PackError::InvalidJson {
            path: path.to_path_buf(),
            message: format!("block {index}: {error}"),
        })?;
        raw_blocks.push(raw);
    }

    if raw_blocks[0].id != AIR_ID {
        return Err(PackError::InvalidOrder {
            path: Some(path.to_path_buf()),
            id: raw_blocks[0].id.clone(),
        });
    }

    build_blocks(path, &raw_blocks)
}

/// Loads `blocks.json` and freezes it into a [`Registry`].
///
/// This is the intended entry point for hosts integrating content packs.
pub fn registry_from_pack(path: &Path) -> Result<Registry, PackError> {
    let blocks = load_blocks_json(path)?;
    Registry::from_blocks(blocks).map_err(|error| error.with_path(path))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlock {
    id: String,
    #[serde(default)]
    solid: bool,
    #[serde(default)]
    opaque: bool,
    #[serde(default)]
    fluid: bool,
    color: Option<Vec<f64>>,
    textures: Option<RawTextures>,
    /// Hand-breaking seconds; -1 means unbreakable, omitted means 1.0.
    hardness: Option<f64>,
    /// Preferred tool kind; omitted means no preferred tool.
    #[serde(default)]
    preferred_tool: Option<ToolKind>,
    /// Minimum tool tier for drops and preferred-tool speed; omitted means 0.
    #[serde(default)]
    required_tier: u8,
    /// Emitted RGB block light, each 0..=15; omitted means no emission.
    #[serde(default)]
    emission: Option<[u8; 3]>,
    /// RGB block-light attenuation, each 0..=15; omitted means no attenuation.
    #[serde(default)]
    attenuation: Option<[u8; 3]>,
    /// Sky-light attenuation 0..=15; omitted derives 15 for opaque blocks and
    /// 0 otherwise.
    #[serde(default)]
    sky_attenuation: Option<u8>,
    /// Rendered shape; omitted means a full cube.
    #[serde(default)]
    shape: Option<BlockShape>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTextures {
    top: Option<String>,
    side: Option<String>,
    bottom: Option<String>,
    /// Optional interim front face (+Z); omitted means the side texture.
    front: Option<String>,
}

fn read_pack_file(path: &Path) -> Result<Vec<u8>, PackError> {
    let file = File::open(path).map_err(|source| PackError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| PackError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_PACK_BYTES {
        return Err(PackError::FileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: MAX_PACK_BYTES,
        });
    }

    let mut reader = file.take(MAX_PACK_BYTES + 1);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|source| PackError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() as u64 > MAX_PACK_BYTES {
        return Err(PackError::FileTooLarge {
            path: path.to_path_buf(),
            size: bytes.len() as u64,
            limit: MAX_PACK_BYTES,
        });
    }
    Ok(bytes)
}

fn describe_format(value: Option<&Value>) -> String {
    match value {
        None => "missing".to_owned(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(_)) => "a string".to_owned(),
        Some(Value::Bool(_)) => "a boolean".to_owned(),
        Some(Value::Array(_)) => "an array".to_owned(),
        Some(Value::Object(_)) => "an object".to_owned(),
        Some(Value::Null) => "null".to_owned(),
    }
}

fn build_blocks(path: &Path, raw_blocks: &[RawBlock]) -> Result<Vec<BlockType>, PackError> {
    let mut blocks = Vec::with_capacity(raw_blocks.len());
    let mut seen: FxHashMap<&str, usize> = FxHashMap::default();
    for (index, raw) in raw_blocks.iter().enumerate() {
        validate_id(&raw.id).map_err(|reason| PackError::InvalidId {
            path: Some(path.to_path_buf()),
            index,
            id: raw.id.clone(),
            reason,
        })?;
        if let Some(&first) = seen.get(raw.id.as_str()) {
            return Err(PackError::DuplicateId {
                path: Some(path.to_path_buf()),
                id: raw.id.clone(),
                first,
                second: index,
            });
        }
        seen.insert(raw.id.as_str(), index);

        let color = validate_color(path, index, &raw.id, raw.color.as_deref())?;
        let textures = resolve_textures(path, index, &raw.id, raw.textures.as_ref())?;
        let hardness = validate_hardness(path, index, &raw.id, raw.hardness)?;
        validate_tool_rule(path, index, &raw.id, raw.preferred_tool, raw.required_tier)?;
        let emission = raw.emission.unwrap_or([0; 3]);
        // Material-class defaults mirror the base registry: opaque blocks stop
        // both light kinds, fluids attenuate by one, air/plants pass sky
        // unchanged. Packs override these for leaves, beds, stained glass, etc.
        let attenuation = raw.attenuation.unwrap_or(if raw.opaque {
            [15; 3]
        } else if raw.fluid {
            [1; 3]
        } else {
            [0; 3]
        });
        let sky_attenuation = raw.sky_attenuation.unwrap_or(if raw.opaque {
            15
        } else if raw.fluid {
            1
        } else {
            0
        });
        validate_light(path, index, &raw.id, emission, attenuation, sky_attenuation)?;
        blocks.push(BlockType {
            id: intern(&raw.id),
            color,
            solid: raw.solid,
            opaque: raw.opaque,
            fluid: raw.fluid,
            textures,
            hardness,
            preferred_tool: raw.preferred_tool,
            required_tier: raw.required_tier,
            emission,
            attenuation,
            sky_attenuation,
            shape: raw.shape.unwrap_or_default(),
        });
    }
    Ok(blocks)
}

/// Validates the optional hardness field: seconds, or -1 for unbreakable.
fn validate_hardness(path: &Path, index: usize, id: &str, hardness: Option<f64>) -> Result<f32, PackError> {
    let Some(value) = hardness else {
        return Ok(1.0);
    };
    if !value.is_finite() {
        return Err(PackError::InvalidHardness {
            path: Some(path.to_path_buf()),
            index,
            id: id.to_owned(),
            reason: "must be a finite number or -1".to_owned(),
        });
    }
    if value < 0.0 {
        return Ok(f32::INFINITY);
    }
    if value > 3600.0 {
        return Err(PackError::InvalidHardness {
            path: Some(path.to_path_buf()),
            index,
            id: id.to_owned(),
            reason: "must be at most 3600 seconds".to_owned(),
        });
    }
    Ok(value as f32)
}

/// Validates that a tiered block also declares which tool kind it needs.
fn validate_tool_rule(
    path: &Path,
    index: usize,
    id: &str,
    preferred_tool: Option<ToolKind>,
    required_tier: u8,
) -> Result<(), PackError> {
    if required_tier > 0 && preferred_tool.is_none() {
        return Err(PackError::InvalidToolRule {
            path: Some(path.to_path_buf()),
            index,
            id: id.to_owned(),
            reason: "required_tier > 0 needs a preferred_tool",
        });
    }
    Ok(())
}

/// Validates emission and attenuation channels and sky attenuation.
fn validate_light(
    path: &Path,
    index: usize,
    id: &str,
    emission: [u8; 3],
    attenuation: [u8; 3],
    sky_attenuation: u8,
) -> Result<(), PackError> {
    let invalid = |reason: String| PackError::InvalidLight {
        path: Some(path.to_path_buf()),
        index,
        id: id.to_owned(),
        reason,
    };
    if let Some((channel, value)) = emission.iter().enumerate().find(|(_, value)| **value > 15) {
        return Err(invalid(format!(
            "emission {} channel is {value}, expected 0..=15",
            ["red", "green", "blue"][channel]
        )));
    }
    if let Some((channel, value)) = attenuation.iter().enumerate().find(|(_, value)| **value > 15) {
        return Err(invalid(format!(
            "attenuation {} channel is {value}, expected 0..=15",
            ["red", "green", "blue"][channel]
        )));
    }
    if sky_attenuation > 15 {
        return Err(invalid(format!(
            "sky_attenuation is {sky_attenuation}, expected 0..=15"
        )));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), &'static str> {
    if !id.is_ascii() {
        return Err("must contain only ASCII characters");
    }
    let Some((namespace, path)) = id.split_once(':') else {
        return Err("must be namespaced as \"namespace:path\"");
    };
    if path.contains(':') {
        return Err("must contain exactly one ':'");
    }
    if namespace.is_empty() {
        return Err("namespace must not be empty");
    }
    if path.is_empty() {
        return Err("path must not be empty");
    }
    if namespace.len() > 64 {
        return Err("namespace must be at most 64 bytes");
    }
    if path.len() > 128 {
        return Err("path must be at most 128 bytes");
    }
    if !namespace
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
    {
        return Err("namespace may contain only 'a-z', '0-9', '_', '-', '.'");
    }
    if !path
        .bytes()
        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/'))
    {
        return Err("path may contain only 'a-z', '0-9', '_', '-', '.', '/'");
    }
    Ok(())
}

fn validate_color(path: &Path, index: usize, id: &str, color: Option<&[f64]>) -> Result<[f32; 3], PackError> {
    let Some(channels) = color else {
        return Ok([1.0, 1.0, 1.0]);
    };
    if channels.len() != 3 {
        return Err(PackError::InvalidColor {
            path: Some(path.to_path_buf()),
            index,
            id: id.to_owned(),
            reason: format!("expected exactly 3 channels, found {}", channels.len()),
        });
    }
    for (channel, value) in channels.iter().enumerate() {
        if !value.is_finite() {
            return Err(PackError::InvalidColor {
                path: Some(path.to_path_buf()),
                index,
                id: id.to_owned(),
                reason: format!("channel {channel} is not finite ({value})"),
            });
        }
        if !(0.0..=1.0).contains(value) {
            return Err(PackError::InvalidColor {
                path: Some(path.to_path_buf()),
                index,
                id: id.to_owned(),
                reason: format!("channel {channel} is {value}, outside 0..=1"),
            });
        }
    }
    Ok([channels[0] as f32, channels[1] as f32, channels[2] as f32])
}

fn resolve_textures(
    path: &Path,
    index: usize,
    id: &str,
    textures: Option<&RawTextures>,
) -> Result<BlockTextures, PackError> {
    let Some(textures) = textures else {
        return Ok(BlockTextures::all(textures::DEBUG));
    };
    Ok(BlockTextures {
        top: resolve_texture(path, index, id, "top", textures.top.as_deref())?,
        side: resolve_texture(path, index, id, "side", textures.side.as_deref())?,
        bottom: resolve_texture(path, index, id, "bottom", textures.bottom.as_deref())?,
        front: match textures.front.as_deref() {
            Some(front) => Some(resolve_texture(path, index, id, "front", Some(front))?),
            None => None,
        },
    })
}

fn resolve_texture(
    path: &Path,
    index: usize,
    id: &str,
    face: &'static str,
    name: Option<&str>,
) -> Result<TextureId, PackError> {
    let name = name.unwrap_or(DEBUG_TEXTURE_NAME);
    let Some(layer) = textures::NAMES.iter().position(|candidate| *candidate == name) else {
        return Err(PackError::UnknownTexture {
            path: Some(path.to_path_buf()),
            index,
            id: id.to_owned(),
            face,
            texture: name.to_owned(),
        });
    };
    Ok(TextureId(layer as u8))
}

/// Interns a loaded id for the process lifetime.
///
/// [`BlockType`] stores `&'static str`, so the loader must leak. Interning
/// means repeated loads of the same pack reuse one allocation instead of
/// leaking a fresh string per block per reload.
fn intern(id: &str) -> &'static str {
    static INTERNED_IDS: OnceLock<Mutex<FxHashSet<&'static str>>> = OnceLock::new();
    let mut ids = INTERNED_IDS
        .get_or_init(|| Mutex::new(FxHashSet::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&existing) = ids.get(id) {
        return existing;
    }
    let leaked: &'static str = Box::leak(id.to_owned().into_boxed_str());
    ids.insert(leaked);
    leaked
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::BlockId;

    fn base_pack_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packs")
            .join("base")
            .join("blocks.json")
    }

    struct TempPack {
        dir: PathBuf,
    }

    impl TempPack {
        fn new(label: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("ob_content_pack_{}_{}_{}", std::process::id(), label, id));
            std::fs::create_dir_all(&dir).expect("create temporary pack directory");
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.path(name);
            std::fs::write(&path, contents).expect("write temporary pack file");
            path
        }
    }

    impl Drop for TempPack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn assert_block_eq(expected: &BlockType, actual: &BlockType) {
        assert_eq!(expected.id, actual.id);
        assert_eq!(expected.color, actual.color);
        assert_eq!(expected.solid, actual.solid);
        assert_eq!(expected.opaque, actual.opaque);
        assert_eq!(expected.fluid, actual.fluid);
        assert_eq!(expected.textures, actual.textures);
        assert_eq!(expected.textures.front, actual.textures.front);
        assert_eq!(expected.hardness, actual.hardness);
        assert_eq!(expected.preferred_tool, actual.preferred_tool);
        assert_eq!(expected.required_tier, actual.required_tier);
        assert_eq!(expected.emission, actual.emission);
        assert_eq!(expected.attenuation, actual.attenuation);
        assert_eq!(expected.sky_attenuation, actual.sky_attenuation);
        assert_eq!(expected.shape, actual.shape);
    }

    #[test]
    fn base_pack_matches_base_registry() {
        let path = base_pack_path();
        let base = Registry::base();
        let loaded = registry_from_pack(&path).expect("packs/base/blocks.json must load");

        assert_eq!(base.len(), loaded.len());
        for ((base_id, base_block), (loaded_id, loaded_block)) in base.iter().zip(loaded.iter()) {
            assert_eq!(base_id, loaded_id, "block index for {}", base_block.id);
            assert_block_eq(base_block, loaded_block);
        }
        assert_eq!(loaded.id("openblock:air"), Some(BlockId(0)));
        assert_eq!(loaded.len(), 29);
        let loaded_torch = loaded.block(loaded.id("openblock:torch").unwrap());
        assert_eq!(loaded_torch.emission, [14, 14, 14]);
        assert_eq!(loaded_torch.shape, BlockShape::Cross);
        assert_eq!(
            loaded.block(loaded.id("openblock:cobblestone").unwrap()).preferred_tool,
            Some(ToolKind::Pickaxe)
        );
        assert_eq!(loaded.block(loaded.id("openblock:stone").unwrap()).required_tier, 1);
        let furnace = loaded.block(loaded.id("openblock:furnace").unwrap());
        assert_eq!(furnace.textures.front, Some(textures::FURNACE_FRONT));
        assert_eq!(furnace.textures.for_face(4), textures::FURNACE_FRONT);
        let coal_ore = loaded.block(loaded.id("openblock:coal_ore").unwrap());
        assert_eq!(coal_ore.required_tier, 1);
        assert!(coal_ore.is_ore());
    }

    #[test]
    fn minimal_custom_pack_parses() {
        let temp = TempPack::new("custom");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    {
                        "id": "openblock:test_block",
                        "solid": true,
                        "opaque": true,
                        "fluid": false,
                        "color": [0.25, 0.5, 0.75],
                        "textures": { "top": "stone", "side": "dirt", "bottom": "grass_side" }
                    }
                ]
            }"#,
        );

        let registry = registry_from_pack(&path).expect("minimal pack must load");
        assert_eq!(registry.len(), 2);

        let air = registry.block(BlockId(0));
        assert_eq!(air.id, "openblock:air");
        assert_eq!(air.color, [1.0, 1.0, 1.0]);
        assert!(!air.solid);
        assert!(!air.opaque);
        assert!(!air.fluid);
        assert_eq!(air.textures, BlockTextures::all(textures::DEBUG));

        let test_id = registry.id("openblock:test_block").expect("test block is registered");
        assert_eq!(test_id, BlockId(1));
        let block = registry.block(test_id);
        assert_eq!(block.color, [0.25, 0.5, 0.75]);
        assert!(block.solid);
        assert!(block.opaque);
        assert!(!block.fluid);
        assert_eq!(
            block.textures,
            BlockTextures::column(textures::STONE, textures::DIRT, textures::GRASS_SIDE)
        );
    }

    #[test]
    fn front_texture_is_optional_and_validated() {
        let temp = TempPack::new("front_texture");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    {
                        "id": "openblock:test_front",
                        "textures": { "top": "stone", "side": "dirt", "bottom": "stone", "front": "sand" }
                    }
                ]
            }"#,
        );
        let registry = registry_from_pack(&path).expect("front texture pack loads");
        let block = registry.block(registry.id("openblock:test_front").unwrap());
        assert_eq!(block.textures.top, textures::STONE);
        assert_eq!(block.textures.side, textures::DIRT);
        assert_eq!(block.textures.bottom, textures::STONE);
        assert_eq!(block.textures.front, Some(textures::SAND));
        assert_eq!(block.textures.for_face(4), textures::SAND);

        let unknown = temp.write(
            "unknown_front.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    { "id": "openblock:test_front", "textures": { "front": "nope" } }
                ]
            }"#,
        );
        let error = load_blocks_json(&unknown).expect_err("unknown front texture must fail");
        assert!(
            matches!(&error, PackError::UnknownTexture { face: "front", .. }),
            "{error}"
        );
    }

    #[test]
    fn invalid_json_is_an_error_naming_the_path() {
        let temp = TempPack::new("invalid_json");
        let path = temp.write("blocks.json", "{ \"format\": 1, \"blocks\": [");

        let error = load_blocks_json(&path).expect_err("truncated JSON must fail");
        assert!(matches!(error, PackError::InvalidJson { .. }));
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn unsupported_format_is_rejected() {
        let temp = TempPack::new("format");
        let path = temp.write(
            "blocks.json",
            r#"{ "format": 2, "blocks": [ { "id": "openblock:air" } ] }"#,
        );

        let error = load_blocks_json(&path).expect_err("format 2 must be rejected");
        assert!(matches!(error, PackError::UnsupportedFormat { .. }));
        let message = error.to_string();
        assert!(message.contains('2'), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn empty_block_list_is_missing_air() {
        let temp = TempPack::new("missing_air");
        let path = temp.write("blocks.json", r#"{ "format": 1, "blocks": [] }"#);

        let error = load_blocks_json(&path).expect_err("an empty pack must be rejected");
        assert!(matches!(error, PackError::MissingAir { .. }));
    }

    #[test]
    fn first_block_must_be_air() {
        let temp = TempPack::new("order");
        let path = temp.write(
            "blocks.json",
            r#"{ "format": 1, "blocks": [ { "id": "openblock:stone" } ] }"#,
        );

        let error = load_blocks_json(&path).expect_err("stone first must be rejected");
        assert!(matches!(error, PackError::InvalidOrder { .. }));
        let message = error.to_string();
        assert!(message.contains("openblock:air"), "{message}");
        assert!(message.contains("openblock:stone"), "{message}");
    }

    #[test]
    fn duplicate_block_id_is_rejected() {
        let temp = TempPack::new("duplicate");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    { "id": "openblock:stone" },
                    { "id": "openblock:stone" }
                ]
            }"#,
        );

        let error = load_blocks_json(&path).expect_err("duplicate ids must be rejected");
        assert!(
            matches!(
                error,
                PackError::DuplicateId {
                    first: 1,
                    second: 2,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn malformed_ids_are_rejected() {
        let temp = TempPack::new("bad_ids");
        let bad_ids = [
            "OpenBlock:stone",
            "stone",
            "openblock:",
            ":stone",
            "openblock:Stone",
            "open block:stone",
            "openblock:stone:extra",
        ];

        for id in bad_ids {
            let json = format!(r#"{{ "format": 1, "blocks": [ {{ "id": "openblock:air" }}, {{ "id": "{id}" }} ] }}"#);
            let path = temp.write("blocks.json", json);
            let error = load_blocks_json(&path).expect_err("invalid id must be rejected");
            assert!(
                matches!(&error, PackError::InvalidId { .. }),
                "id {id:?} produced {error}"
            );
            assert!(error.to_string().contains(id), "{error}");
        }
    }

    #[test]
    fn unknown_texture_lists_valid_names() {
        let temp = TempPack::new("texture");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    {
                        "id": "openblock:test_block",
                        "textures": { "top": "does_not_exist" }
                    }
                ]
            }"#,
        );

        let error = load_blocks_json(&path).expect_err("unknown texture must be rejected");
        assert!(matches!(&error, PackError::UnknownTexture { .. }), "{error}");
        let message = error.to_string();
        for name in textures::NAMES {
            assert!(message.contains(name), "missing {name} in {message}");
        }
    }

    #[test]
    fn color_out_of_range_or_wrong_length_is_rejected() {
        let temp = TempPack::new("color");
        let bad_colors = [
            "[1.5, 0.0, 0.0]",
            "[-0.1, 0.0, 0.0]",
            "[1.0, 0.5]",
            "[0.0, 0.0, 0.0, 1.0]",
        ];

        for color in bad_colors {
            let json = format!(
                r#"{{ "format": 1, "blocks": [ {{ "id": "openblock:air" }},
                    {{ "id": "openblock:test_block", "color": {color} }} ] }}"#
            );
            let path = temp.write("blocks.json", json);
            let error = load_blocks_json(&path).expect_err("invalid color must be rejected");
            assert!(
                matches!(&error, PackError::InvalidColor { .. }),
                "color {color} produced {error}"
            );
        }
    }

    #[test]
    fn omitted_optional_fields_use_defaults() {
        let temp = TempPack::new("defaults");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    { "id": "openblock:test_block" }
                ]
            }"#,
        );

        let registry = registry_from_pack(&path).expect("defaults must be accepted");
        let block = registry.block(BlockId(1));
        assert_eq!(block.id, "openblock:test_block");
        assert_eq!(block.color, [1.0, 1.0, 1.0]);
        assert!(!block.solid);
        assert!(!block.opaque);
        assert!(!block.fluid);
        assert_eq!(block.textures, BlockTextures::all(textures::DEBUG));
        assert_eq!(block.preferred_tool, None);
        assert_eq!(block.required_tier, 0);
        assert_eq!(block.emission, [0; 3]);
        assert_eq!(block.attenuation, [0; 3]);
        assert_eq!(block.sky_attenuation, 0);
        assert_eq!(block.shape, BlockShape::Cube);
    }

    #[test]
    fn optional_tool_rules_parse_and_validate() {
        let temp = TempPack::new("tool_rules");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    {
                        "id": "openblock:test_stone",
                        "hardness": 1.5,
                        "preferred_tool": "pickaxe",
                        "required_tier": 1
                    }
                ]
            }"#,
        );
        let registry = registry_from_pack(&path).expect("tool rule pack loads");
        let block = registry.block(registry.id("openblock:test_stone").unwrap());
        assert_eq!(block.preferred_tool, Some(ToolKind::Pickaxe));
        assert_eq!(block.required_tier, 1);

        let bad = temp.write(
            "bad.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    { "id": "openblock:test_stone", "required_tier": 1 }
                ]
            }"#,
        );
        let error = load_blocks_json(&bad).expect_err("tier without a tool kind must fail");
        assert!(matches!(error, PackError::InvalidToolRule { .. }), "{error}");
        assert!(error.to_string().contains("preferred_tool"), "{error}");

        let unknown = temp.write(
            "unknown_tool.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    { "id": "openblock:test_stone", "preferred_tool": "hammer" }
                ]
            }"#,
        );
        assert!(load_blocks_json(&unknown).is_err(), "unknown tool kind must fail");
    }

    #[test]
    fn oversized_block_list_is_rejected() {
        let temp = TempPack::new("too_many_blocks");
        let mut json = String::from(r#"{ "format": 1, "blocks": [ { "id": "openblock:air" }"#);
        for index in 0..MAX_BLOCKS {
            json.push_str(&format!(r#", {{ "id": "openblock:extra{index}" }}"#));
        }
        json.push_str(" ] }");
        let path = temp.write("blocks.json", json);

        let error = load_blocks_json(&path).expect_err("too many blocks must be rejected");
        assert!(
            matches!(
                error,
                PackError::TooManyBlocks {
                    count: 4097,
                    limit: MAX_BLOCKS,
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn oversized_file_is_rejected() {
        let temp = TempPack::new("too_large");
        let path = temp.path("blocks.json");
        let file = std::fs::File::create(&path).expect("create oversized pack file");
        file.set_len(MAX_PACK_BYTES + 1).expect("extend oversized pack file");
        drop(file);

        let error = load_blocks_json(&path).expect_err("oversized file must be rejected");
        assert!(
            matches!(
                error,
                PackError::FileTooLarge {
                    size,
                    limit: MAX_PACK_BYTES,
                    ..
                } if size == MAX_PACK_BYTES + 1
            ),
            "{error}"
        );
    }

    #[test]
    fn hostile_json_shapes_are_bounded_and_typed() {
        let temp = TempPack::new("hostile-json");
        let valid_root = r#"{"format":1,"blocks":[{"id":"openblock:air"}]}"#;
        let cases = [
            // Unknown fields are rejected at every level of the schema.
            r#"{"format":1,"blocks":[],"extra":true}"#.to_string(),
            r#"{"format":1,"blocks":[{"id":"openblock:air","extra":1}]}"#.to_string(),
            r#"{"format":1,"blocks":[{"id":"openblock:air","textures":{"top":"stone","extra":1}}]}"#.to_string(),
            // Trailing garbage and non-object roots are parse errors.
            format!("{valid_root}garbage"),
            r#"[1,2,3]"#.to_string(),
        ];
        for (index, json) in cases.iter().enumerate() {
            let path = temp.write(&format!("hostile_{index}.json"), json);
            let error = load_blocks_json(&path).expect_err("hostile JSON must be rejected");
            assert!(
                matches!(error, PackError::InvalidJson { .. }),
                "case {index} produced {error}"
            );
        }

        // Deep nesting hits serde_json's recursion limit instead of recursing
        // until the stack overflows.
        let mut nested = String::from(r#"{"format":1,"blocks":[{"id":"openblock:air","extra":"#);
        nested.push_str(&"[".repeat(512));
        nested.push('0');
        nested.push_str(&"]".repeat(512));
        nested.push_str("}]}");
        let path = temp.write("deep.json", nested);
        assert!(matches!(load_blocks_json(&path), Err(PackError::InvalidJson { .. })));

        // A huge attacker-controlled string is still parsed within the 8 MiB
        // cap and then rejected by the id rule, not by a panic.
        let huge_id = format!(
            r#"{{"format":1,"blocks":[{{"id":"openblock:air"}},{{"id":"openblock:{}"}}]}}"#,
            "a".repeat(1_000_000)
        );
        let path = temp.write("huge-id.json", huge_id);
        assert!(matches!(load_blocks_json(&path), Err(PackError::InvalidId { .. })));

        // Extreme hardness values are validated as numbers.
        let huge_hardness = temp.write(
            "hardness.json",
            r#"{"format":1,"blocks":[{"id":"openblock:air"},{"id":"openblock:test","hardness":1e300}]}"#,
        );
        assert!(matches!(
            load_blocks_json(&huge_hardness),
            Err(PackError::InvalidHardness { .. })
        ));
        let unbreakable = temp.write(
            "unbreakable.json",
            r#"{"format":1,"blocks":[{"id":"openblock:air"},{"id":"openblock:test","hardness":-1}]}"#,
        );
        let blocks = load_blocks_json(&unbreakable).expect("negative hardness means unbreakable");
        assert!(blocks[1].hardness.is_infinite());

        // Non-UTF-8 JSON is a typed parse error.
        let path = temp.path("non-utf8.json");
        std::fs::write(&path, [0xffu8, 0xfe, b'{']).expect("write non-utf8 pack");
        assert!(matches!(load_blocks_json(&path), Err(PackError::InvalidJson { .. })));
    }

    #[test]
    fn loading_is_deterministic() {
        let path = base_pack_path();
        let first = load_blocks_json(&path).expect("first load succeeds");
        let second = load_blocks_json(&path).expect("second load succeeds");

        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(second.iter()) {
            assert_block_eq(left, right);
        }
    }

    #[test]
    fn from_blocks_rejects_invalid_registries() {
        let base = Registry::base();
        let blocks: Vec<BlockType> = base.iter().map(|(_, block)| block.clone()).collect();

        assert!(matches!(
            Registry::from_blocks(Vec::new()),
            Err(PackError::MissingAir { .. })
        ));

        let mut duplicate = blocks.clone();
        duplicate.push(duplicate[1].clone());
        assert!(matches!(
            Registry::from_blocks(duplicate),
            Err(PackError::DuplicateId { .. })
        ));

        let mut reordered = blocks;
        reordered.swap(0, 1);
        assert!(matches!(
            Registry::from_blocks(reordered),
            Err(PackError::InvalidOrder { .. })
        ));
    }

    #[test]
    fn light_fields_parse_and_invalid_values_are_rejected() {
        let temp = TempPack::new("light_fields");
        let path = temp.write(
            "blocks.json",
            r#"{
                "format": 1,
                "blocks": [
                    { "id": "openblock:air" },
                    {
                        "id": "openblock:test_lamp",
                        "solid": true,
                        "opaque": false,
                        "emission": [14, 10, 6],
                        "attenuation": [1, 2, 3],
                        "sky_attenuation": 4,
                        "shape": "cross"
                    },
                    {
                        "id": "openblock:test_opaque",
                        "solid": true,
                        "opaque": true
                    }
                ]
            }"#,
        );
        let registry = registry_from_pack(&path).expect("light fields load");
        let lamp = registry.block(registry.id("openblock:test_lamp").unwrap());
        assert_eq!(lamp.emission, [14, 10, 6]);
        assert_eq!(lamp.attenuation, [1, 2, 3]);
        assert_eq!(lamp.sky_attenuation, 4);
        assert_eq!(lamp.shape, BlockShape::Cross);
        // Omitted light fields derive from the material class.
        let opaque = registry.block(registry.id("openblock:test_opaque").unwrap());
        assert_eq!(opaque.attenuation, [15; 3]);
        assert_eq!(opaque.sky_attenuation, 15);
        assert_eq!(opaque.shape, BlockShape::Cube);

        for (field, value) in [
            ("emission", "[16, 0, 0]"),
            ("emission", "[0, 0, 16]"),
            ("attenuation", "[0, 16, 0]"),
        ] {
            let bad = temp.write(
                "bad_light.json",
                format!(
                    r#"{{"format":1,"blocks":[{{"id":"openblock:air"}},{{"id":"openblock:test_bad","{field}":{value}}}]}}"#
                ),
            );
            let error = load_blocks_json(&bad).expect_err("out-of-range light values must fail");
            assert!(matches!(error, PackError::InvalidLight { .. }), "{field}: {error}");
        }
        let bad_sky = temp.write(
            "bad_sky.json",
            r#"{"format":1,"blocks":[{"id":"openblock:air"},{"id":"openblock:test_bad","sky_attenuation":16}]}"#,
        );
        let error = load_blocks_json(&bad_sky).expect_err("sky attenuation above 15 must fail");
        assert!(matches!(error, PackError::InvalidLight { .. }), "{error}");
    }
}

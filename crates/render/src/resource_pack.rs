//! Runtime loading of block textures from a Minecraft-format resource pack.
//!
//! A user can point OpenBlock at a resource-pack directory (the pack root, its
//! `assets/minecraft` subdirectory, or a `textures/block` directory). The loader
//! maps the fixed OpenBlock catalogue in [`ob_content::textures::NAMES`] onto the
//! usual Minecraft block texture file names, decodes them with the `png` crate,
//! scales every layer to one common square resolution, and builds the same full
//! mip chain the procedural packs use.
//!
//! Missing layers are not an error: each one falls back to the procedural
//! [`TexturePackStyle::Classic`] layer (or another requested style) and its index
//! is recorded in [`ResourcePackLayers::fallback_layers`]. That keeps every pack
//! usable, including old or partial packs that do not ship, for example, the
//! animated water texture.
//!
//! Everything here is CPU-only and deterministic. The only files read are the
//! user's local pack; no pack assets are bundled with OpenBlock.
//!
//! # Alpha rules
//!
//! Leaves are a cut-out layer: every decoded alpha value below 128 becomes 0 and
//! every value at or above 128 becomes 255. All other layers are forced fully
//! opaque (alpha 255) so translucent pack textures such as glass or water stay
//! compatible with OpenBlock's opaque world pipeline. Mip levels box-filter all
//! four channels exactly like the procedural style path and then re-apply the
//! same 128 cut-out threshold to the leaves layer; the world shader additionally
//! discards samples below 0.5.
//!
//! # Scaling
//!
//! The array's base size is the largest decoded square frame clamped to
//! `16..=256`. A non-power-of-two source size is used as-is (the mip chain
//! follows `floor(edge / 2)`). Smaller textures are magnified with
//! nearest-neighbour sampling; larger ones are reduced with a deterministic box
//! average. The same rule is used for the procedural fallback layers.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ob_content::textures::{COUNT, GRASS_SIDE, GRASS_TOP, LEAVES, NAMES, TORCH, WHEAT_0, WHEAT_3};

use crate::textures::{TexturePack, TexturePackStyle};

/// Maximum size of one source PNG file.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum accepted source width or height in texels.
const MAX_TEXTURE_EDGE: u32 = 1024;

/// Smallest base-level edge length uploaded to the GPU.
const MIN_ARRAY_SIZE: u32 = 16;

/// Largest base-level edge length uploaded to the GPU.
const MAX_ARRAY_SIZE: u32 = 256;

/// Alpha values below this become transparent, others become fully opaque.
const ALPHA_CUTOFF: u8 = 128;

/// Where the block texture array's pixels come from.
///
/// [`TextureSource::Style`] is the deterministic procedural generator that ships
/// with OpenBlock. [`TextureSource::ResourcePack`] is a user-local Minecraft
/// resource pack; it is never copied into the repository or a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureSource {
    /// One of the built-in procedural texture-pack styles.
    Style(TexturePackStyle),
    /// A Minecraft-format resource-pack directory on the user's machine.
    ///
    /// The path may be the pack root, its `assets/minecraft` directory, or a
    /// `textures/block` directory; see [`load_resource_pack`].
    ResourcePack(PathBuf),
}

impl Default for TextureSource {
    fn default() -> Self {
        Self::Style(TexturePackStyle::default())
    }
}

impl TextureSource {
    /// Short human-readable label: the style name or the pack folder name.
    ///
    /// For an `assets/minecraft` path the label is the resource-pack folder
    /// above it, not `minecraft`.
    pub fn label(&self) -> String {
        match self {
            Self::Style(style) => style.name().to_owned(),
            Self::ResourcePack(path) => resource_pack_label(path),
        }
    }
}

/// The folder name used as a label for a resource-pack path.
fn resource_pack_label(path: &Path) -> String {
    // Accept `.../<pack>/assets/minecraft` as well as `.../<pack>` and return
    // the pack folder in both cases.
    if path.file_name().is_some_and(|name| name == "minecraft")
        && let Some(assets) = path.parent()
        && assets.file_name().is_some_and(|name| name == "assets")
        && let Some(pack) = assets.parent()
        && let Some(name) = pack.file_name()
    {
        return name.to_string_lossy().into_owned();
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Errors from resolving or decoding a resource pack.
///
/// Loading user input never panics; every failure is reported through this enum.
#[derive(Debug)]
pub enum TextureLoadError {
    /// A filesystem operation failed.
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// No supported block-texture directory exists under the given path.
    PackNotFound {
        /// Candidate directories that were probed, in order.
        looked: Vec<PathBuf>,
    },
    /// The resolved block-texture directory contains no entries at all.
    EmptyPack {
        /// Resolved block-texture directory.
        path: PathBuf,
    },
    /// A candidate PNG could not be decoded.
    InvalidImage {
        /// File that failed to decode.
        path: PathBuf,
        /// Decoder message.
        message: String,
    },
    /// A PNG's dimensions are zero or exceed the supported limit.
    UnsupportedSize {
        /// File with the unsupported dimensions.
        path: PathBuf,
        /// Decoded width in texels.
        width: u32,
        /// Decoded height in texels.
        height: u32,
    },
    /// A PNG file exceeds the byte limit.
    TooLarge {
        /// File that is too large.
        path: PathBuf,
        /// Size in bytes.
        size: u64,
        /// Maximum accepted size in bytes.
        limit: u64,
    },
    /// The block directory has files, but none of the expected block textures.
    NoTextures {
        /// Resolved block-texture directory.
        path: PathBuf,
    },
}

impl fmt::Display for TextureLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::PackNotFound { looked } => {
                let candidates: Vec<String> = looked.iter().map(|path| path.display().to_string()).collect();
                write!(
                    f,
                    "no block texture directory found; looked in: {}",
                    candidates.join(", ")
                )
            }
            Self::EmptyPack { path } => {
                write!(f, "resource pack block directory is empty: {}", path.display())
            }
            Self::InvalidImage { path, message } => {
                write!(f, "invalid block texture {}: {message}", path.display())
            }
            Self::UnsupportedSize { path, width, height } => write!(
                f,
                "unsupported block texture size {width}x{height} in {} \
                 (expected non-zero dimensions up to {MAX_TEXTURE_EDGE}x{MAX_TEXTURE_EDGE})",
                path.display()
            ),
            Self::TooLarge { path, size, limit } => write!(
                f,
                "block texture {} is {size} bytes, larger than the {limit}-byte limit",
                path.display()
            ),
            Self::NoTextures { path } => write!(f, "no expected Minecraft block textures found in {}", path.display()),
        }
    }
}

impl std::error::Error for TextureLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A CPU-side resource-pack catalogue ready for GPU upload.
///
/// Layers are always in [`NAMES`] order and every layer has the same square
/// resolution. The mip chain is stored level-major exactly as uploaded to the
/// texture array: mip 0 is `COUNT * size * size * 4` bytes of tightly packed RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePackLayers {
    /// Base-level edge length in texels.
    pub size: u32,
    /// The path passed to [`load_resource_pack`].
    pub source: PathBuf,
    /// The resolved `textures/block` directory that was read.
    pub block_dir: PathBuf,
    /// Indices into [`NAMES`] whose files were missing and used procedural fallback.
    ///
    /// Always sorted ascending. Layer 0 (`debug`) is always present here because
    /// it intentionally has no Minecraft file.
    pub fallback_layers: Vec<usize>,
    /// Full mip chain, level-major; every level holds [`COUNT`] square layers.
    pub mips: Vec<Vec<u8>>,
}

/// One decoded source texture, cropped to a square frame.
struct DecodedTexture {
    /// Edge length of the square frame in texels.
    side: u32,
    /// Tightly packed RGBA8 square frame with the pack's original alpha.
    rgba: Vec<u8>,
}

/// Loads the block textures of a Minecraft-format resource pack.
///
/// `path` may be:
///
/// * the resource-pack root (the directory containing `assets/`),
/// * its `assets/minecraft` directory,
/// * its `textures/block` directory, or
/// * any directory that directly contains the expected block PNGs.
///
/// The candidate directories are tried in that order. Missing catalogue entries
/// fall back to the procedural [`TexturePackStyle::Classic`] layer; the returned
/// [`ResourcePackLayers`] records them in `fallback_layers`.
pub fn load_resource_pack(path: &Path) -> Result<ResourcePackLayers, TextureLoadError> {
    load_resource_pack_with_fallback(path, TexturePackStyle::Classic)
}

/// Loads a resource pack, using `fallback` for catalogue entries the pack lacks.
pub(crate) fn load_resource_pack_with_fallback(
    path: &Path,
    fallback: TexturePackStyle,
) -> Result<ResourcePackLayers, TextureLoadError> {
    let block_dir = resolve_block_dir(path)?;
    if directory_is_empty(&block_dir)? {
        return Err(TextureLoadError::EmptyPack { path: block_dir });
    }

    let mut decoded: Vec<Option<DecodedTexture>> = Vec::with_capacity(COUNT);
    let mut fallback_layers = Vec::new();
    let mut largest = 0u32;
    let mut found = 0usize;
    for (index, name) in NAMES.iter().enumerate() {
        let file = candidate_files(name)
            .iter()
            .map(|candidate| block_dir.join(candidate))
            .find(|candidate| candidate.is_file());
        match file {
            Some(file) => {
                let texture = decode_texture(&file)?;
                largest = largest.max(texture.side);
                decoded.push(Some(texture));
                found += 1;
            }
            None => {
                decoded.push(None);
                fallback_layers.push(index);
            }
        }
    }
    if found == 0 {
        return Err(TextureLoadError::NoTextures { path: block_dir });
    }
    apply_biome_tints(&block_dir, &mut decoded)?;

    let size = largest.clamp(MIN_ARRAY_SIZE, MAX_ARRAY_SIZE);
    let fallback_pack = TexturePack::generate(fallback);
    let layer_bytes = (size * size * 4) as usize;
    let mut base = Vec::with_capacity(COUNT * layer_bytes);
    for (index, texture) in decoded.iter().enumerate() {
        let cut_out = is_cut_out_layer(index);
        let mut layer = match texture {
            Some(texture) => scale_square(&texture.rgba, texture.side, size),
            None => {
                let source = fallback_pack
                    .layer(0, index)
                    .ok_or_else(|| TextureLoadError::InvalidImage {
                        path: block_dir.clone(),
                        message: format!("procedural fallback layer {index} is missing"),
                    })?;
                scale_square(source, fallback_pack.resolution(), size)
            }
        };
        apply_alpha(&mut layer, cut_out);
        debug_assert_eq!(layer.len(), layer_bytes);
        base.extend_from_slice(&layer);
    }

    let mips = generate_mips(&base, size);
    Ok(ResourcePackLayers {
        size,
        source: path.to_path_buf(),
        block_dir,
        fallback_layers,
        mips,
    })
}

/// Probes the supported pack layouts and returns the first block directory.
fn resolve_block_dir(path: &Path) -> Result<PathBuf, TextureLoadError> {
    let candidates = candidate_directories(path);
    for (index, candidate) in candidates.iter().enumerate() {
        let usable = match index {
            // `<root>/assets/minecraft/textures/block` and
            // `<assets>/textures/block` are accepted even when empty so the
            // loader can report an EmptyPack rather than PackNotFound.
            0 | 1 => candidate.is_dir(),
            // A bare `<path>/block` directory only counts when it holds PNGs.
            2 => candidate.is_dir() && directory_contains_png(candidate)?,
            // The path itself is accepted when it is the block directory or
            // directly contains expected files.
            _ => path.is_dir() && (path.ends_with("textures/block") || directory_has_expected(path)),
        };
        if usable {
            return Ok(candidate.clone());
        }
    }
    Err(TextureLoadError::PackNotFound { looked: candidates })
}

/// Candidate block directories for a user-supplied pack path, in probe order.
fn candidate_directories(path: &Path) -> Vec<PathBuf> {
    vec![
        path.join("assets/minecraft/textures/block"),
        path.join("textures/block"),
        path.join("block"),
        path.to_path_buf(),
    ]
}

/// Whether `dir` directly contains at least one `.png` file.
fn directory_contains_png(dir: &Path) -> Result<bool, TextureLoadError> {
    let entries = fs::read_dir(dir).map_err(|source| TextureLoadError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| TextureLoadError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        if entry.path().is_file() && is_png(&entry.path()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `dir` directly contains any of the expected block texture files.
fn directory_has_expected(dir: &Path) -> bool {
    NAMES.iter().any(|name| {
        candidate_files(name)
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

/// Whether the directory has no entries at all.
fn directory_is_empty(dir: &Path) -> Result<bool, TextureLoadError> {
    let mut entries = fs::read_dir(dir).map_err(|source| TextureLoadError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(entries.next().is_none())
}

/// Whether a path has a case-insensitive `.png` extension.
fn is_png(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
}

/// Candidate block texture files for one catalogue name, first match wins.
///
/// Suffixed normal/specular maps (`_n.png`, `_s.png`) are deliberately not
/// listed: OpenBlock has no normal mapping, so the colour texture must win.
fn candidate_files(name: &str) -> &'static [&'static str] {
    match name {
        "grass_top" => &["grass_block_top.png", "grass_top.png"],
        "grass_side" => &["grass_block_side.png", "grass_side.png"],
        "dirt" => &["dirt.png"],
        "stone" => &["stone.png"],
        "sand" => &["sand.png"],
        "water" => &["water_still.png", "water.png", "water_flow.png"],
        "log_side" => &["oak_log.png", "oak_log_side.png", "log_oak.png", "log.png"],
        "log_top" => &["oak_log_top.png", "log_oak_top.png"],
        "leaves" => &["oak_leaves.png", "leaves_oak.png", "leaves.png"],
        "bedrock" => &["bedrock.png"],
        "gravel" => &["gravel.png"],
        "clay" => &["clay.png"],
        "snow" => &["snow.png"],
        "planks" => &["oak_planks.png", "planks_oak.png", "planks.png"],
        "cobblestone" => &["cobblestone.png"],
        "crafting_table_top" => &["crafting_table_top.png"],
        "crafting_table_side" => &["crafting_table_side.png"],
        "coal_ore" => &["coal_ore.png"],
        "iron_ore" => &["iron_ore.png"],
        "copper_ore" => &["copper_ore.png"],
        "furnace_top" => &["furnace_top.png"],
        "furnace_side" => &["furnace_side.png"],
        "furnace_front" => &["furnace_front.png"],
        // Vanilla ships chest textures under `entity/chest`, not
        // `textures/block`, so many packs simply have no file here and the
        // chest faces fall back to the procedural Classic layers.
        "chest_top" => &["chest_top.png"],
        "chest_side" => &["chest_side.png"],
        "chest_front" => &["chest_front.png"],
        // 1.2 Light & Home layers. Farmland and wheat use the vanilla names;
        // beds usually live under `entity/bed` in a pack, so their block layers
        // commonly fall back to the procedural catalogue.
        "torch" => &["torch.png"],
        "farmland_dry" => &["farmland.png", "farmland_dry.png"],
        "farmland_wet" => &["farmland_moist.png", "farmland_wet.png"],
        "wheat_0" => &["wheat_stage0.png", "wheat_0.png"],
        "wheat_1" => &["wheat_stage1.png", "wheat_1.png"],
        "wheat_2" => &["wheat_stage2.png", "wheat_2.png"],
        "wheat_3" => &["wheat_stage3.png", "wheat_3.png"],
        "bed_foot_top" => &["bed_foot_top.png"],
        "bed_foot_side" => &["bed_foot_side.png"],
        "bed_head_top" => &["bed_head_top.png"],
        "bed_head_side" => &["bed_head_side.png"],
        // `debug` has no Minecraft equivalent and is procedural by design.
        _ => &[],
    }
}

/// Reads, validates and decodes one PNG into a square frame.
fn decode_texture(path: &Path) -> Result<DecodedTexture, TextureLoadError> {
    let metadata = fs::metadata(path).map_err(|source| TextureLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(TextureLoadError::TooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            limit: MAX_FILE_BYTES,
        });
    }

    let bytes = fs::read(path).map_err(|source| TextureLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Re-check after reading in case the file grew between the metadata call
    // and the read.
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(TextureLoadError::TooLarge {
            path: path.to_path_buf(),
            size: bytes.len() as u64,
            limit: MAX_FILE_BYTES,
        });
    }

    let mut decoder = png::Decoder::new(io::Cursor::new(bytes));
    // Normalise palettes, tRNS, grayscale and 16-bit samples to 8-bit RGBA or
    // grayscale-alpha so the conversion below only has to handle those two.
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().map_err(|error| TextureLoadError::InvalidImage {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;

    let (width, height) = reader.info().size();
    if width == 0 || height == 0 || width > MAX_TEXTURE_EDGE || height > MAX_TEXTURE_EDGE {
        return Err(TextureLoadError::UnsupportedSize {
            path: path.to_path_buf(),
            width,
            height,
        });
    }

    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| TextureLoadError::InvalidImage {
            path: path.to_path_buf(),
            message: "decoded image dimensions overflow the address space".to_owned(),
        })?;
    let mut buffer = vec![0u8; buffer_size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| TextureLoadError::InvalidImage {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    if frame.bit_depth != png::BitDepth::Eight {
        return Err(TextureLoadError::InvalidImage {
            path: path.to_path_buf(),
            message: format!("unsupported bit depth {:?} after normalisation", frame.bit_depth),
        });
    }
    let samples = match frame.color_type {
        png::ColorType::Rgba => 4usize,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => {
            return Err(TextureLoadError::InvalidImage {
                path: path.to_path_buf(),
                message: "indexed PNG was not expanded to RGBA".to_owned(),
            });
        }
    };
    if frame.width == 0 || frame.height == 0 {
        return Err(TextureLoadError::UnsupportedSize {
            path: path.to_path_buf(),
            width: frame.width,
            height: frame.height,
        });
    }
    let frame_bytes = frame
        .line_size
        .checked_mul(frame.height as usize)
        .filter(|frame_bytes| *frame_bytes <= buffer.len())
        .ok_or_else(|| TextureLoadError::InvalidImage {
            path: path.to_path_buf(),
            message: "decoded row data is truncated".to_owned(),
        })?;
    if frame.line_size < frame.width as usize * samples || frame_bytes > buffer.len() {
        return Err(TextureLoadError::InvalidImage {
            path: path.to_path_buf(),
            message: "decoded row data has an unexpected layout".to_owned(),
        });
    }

    // Minecraft animated textures are tall strips of square frames; a plain
    // tall image uses its left/top square and a wide image its top square.
    // In every case the first square frame starts at the top-left corner.
    let side = if height > width && height % width == 0 {
        // Animated texture: first frame of the strip.
        width
    } else if width >= height {
        // Wide image: top-left square.
        height
    } else {
        // Tall image: left square from the top.
        width
    };

    let mut rgba = vec![0u8; (side * side * 4) as usize];
    for y in 0..side {
        let row = y as usize * frame.line_size;
        for x in 0..side {
            let source = row + x as usize * samples;
            let pixel = match frame.color_type {
                png::ColorType::Rgba => [
                    buffer[source],
                    buffer[source + 1],
                    buffer[source + 2],
                    buffer[source + 3],
                ],
                png::ColorType::Rgb => [buffer[source], buffer[source + 1], buffer[source + 2], 255],
                png::ColorType::GrayscaleAlpha => {
                    let grey = buffer[source];
                    [grey, grey, grey, buffer[source + 1]]
                }
                png::ColorType::Grayscale => {
                    let grey = buffer[source];
                    [grey, grey, grey, 255]
                }
                png::ColorType::Indexed => {
                    return Err(TextureLoadError::InvalidImage {
                        path: path.to_path_buf(),
                        message: "indexed PNG was not expanded to RGBA".to_owned(),
                    });
                }
            };
            let destination = ((y * side + x) * 4) as usize;
            rgba[destination..destination + 4].copy_from_slice(&pixel);
        }
    }

    Ok(DecodedTexture { side, rgba })
}

/// Whether a catalogue layer is alpha-tested.
///
/// Leaves were the only 1.1 cut-out; 1.2 adds the torch and the four wheat
/// stages, whose cross quads must punch through the transparent background.
fn is_cut_out_layer(layer: usize) -> bool {
    layer == LEAVES.0 as usize
        || layer == TORCH.0 as usize
        || (WHEAT_0.0 as usize..=WHEAT_3.0 as usize).contains(&layer)
}

/// Applies the cut-out/opaque rule to one RGBA layer.
fn apply_alpha(rgba: &mut [u8], cut_out: bool) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel[3] = if cut_out {
            if pixel[3] < ALPHA_CUTOFF { 0 } else { 255 }
        } else {
            255
        };
    }
}

/// Minecraft's default plains grass tint (sRGB #91BD59) used for packs that ship
/// grayscale, biome-tinted grass textures.
const GRASS_TINT: [f32; 3] = [145.0 / 255.0, 189.0 / 255.0, 89.0 / 255.0];
/// Minecraft's default plains foliage tint (sRGB #77AB2F) for grayscale leaves.
const FOLIAGE_TINT: [f32; 3] = [119.0 / 255.0, 171.0 / 255.0, 47.0 / 255.0];
/// Below this average chroma the texture is treated as a grayscale biome mask.
const GRAYSCALE_CHROMA: f32 = 0.06;

/// Average chroma (max-min) of the opaque texels, in 0..=1.
fn average_chroma(rgba: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    let mut count = 0u32;
    for pixel in rgba.as_chunks::<4>().0 {
        if pixel[3] == 0 {
            continue;
        }
        let max = pixel[0].max(pixel[1]).max(pixel[2]) as f32;
        let min = pixel[0].min(pixel[1]).min(pixel[2]) as f32;
        sum += (max - min) / 255.0;
        count += 1;
    }
    if count == 0 { 0.0 } else { sum / count as f32 }
}

/// Multiplies the RGB channels of an RGBA image by an sRGB tint, keeping alpha.
fn tint_rgb(rgba: &mut [u8], tint: [f32; 3]) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        for channel in 0..3 {
            pixel[channel] = (f32::from(pixel[channel]) * tint[channel]).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Applies Minecraft's biome tint to grass tops, leaves and the grass-side
/// overlay when the pack stores them as grayscale masks.
///
/// Packs that already bake colour into these textures are left untouched: the
/// average chroma decides, so both faithful-style grayscale packs and fully
/// painted packs render correctly.
fn apply_biome_tints(block_dir: &Path, decoded: &mut [Option<DecodedTexture>]) -> Result<(), TextureLoadError> {
    if let Some(texture) = decoded.get_mut(GRASS_TOP.0 as usize).and_then(Option::as_mut)
        && average_chroma(&texture.rgba) < GRAYSCALE_CHROMA
    {
        tint_rgb(&mut texture.rgba, GRASS_TINT);
    }
    if let Some(texture) = decoded.get_mut(LEAVES.0 as usize).and_then(Option::as_mut)
        && average_chroma(&texture.rgba) < GRAYSCALE_CHROMA
    {
        tint_rgb(&mut texture.rgba, FOLIAGE_TINT);
    }

    // Vanilla packs paint the grassy fringe of the side face as a separate
    // grayscale overlay; composite it over the dirt base with the grass tint.
    let overlay_path = ["grass_block_side_overlay.png", "grass_side_overlay.png"]
        .iter()
        .map(|name| block_dir.join(name))
        .find(|candidate| candidate.is_file());
    if let Some(overlay_path) = overlay_path
        && let Some(base) = decoded.get_mut(GRASS_SIDE.0 as usize).and_then(Option::as_mut)
    {
        let overlay = decode_texture(&overlay_path)?;
        let tinted = average_chroma(&overlay.rgba) < GRAYSCALE_CHROMA;
        let mut scaled = if overlay.side == base.side {
            overlay.rgba
        } else {
            scale_square(&overlay.rgba, overlay.side, base.side)
        };
        if tinted {
            tint_rgb(&mut scaled, GRASS_TINT);
        }
        for (dst, src) in base
            .rgba
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(scaled.as_chunks::<4>().0)
        {
            let alpha = src[3] as f32 / 255.0;
            for channel in 0..3 {
                dst[channel] = (f32::from(src[channel]) * alpha + f32::from(dst[channel]) * (1.0 - alpha))
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(())
}

/// Scales one tightly packed square RGBA layer to `target_edge`.
///
/// Magnification is nearest-neighbour (crisp pixel art), reduction is an exact
/// integer box average over the covered source rectangle. Both directions are
/// deterministic and allocation-free beyond the output buffer.
fn scale_square(source: &[u8], source_edge: u32, target_edge: u32) -> Vec<u8> {
    debug_assert_eq!(source.len(), (source_edge * source_edge * 4) as usize);
    if source_edge == target_edge {
        return source.to_vec();
    }

    let mut output = vec![0u8; (target_edge * target_edge * 4) as usize];
    if target_edge > source_edge {
        for y in 0..target_edge {
            let source_y = (y * source_edge / target_edge).min(source_edge - 1);
            for x in 0..target_edge {
                let source_x = (x * source_edge / target_edge).min(source_edge - 1);
                let from = ((source_y * source_edge + source_x) * 4) as usize;
                let to = ((y * target_edge + x) * 4) as usize;
                output[to..to + 4].copy_from_slice(&source[from..from + 4]);
            }
        }
    } else {
        for y in 0..target_edge {
            let y0 = y * source_edge / target_edge;
            let y1 = ((y + 1) * source_edge).div_ceil(target_edge).min(source_edge);
            for x in 0..target_edge {
                let x0 = x * source_edge / target_edge;
                let x1 = ((x + 1) * source_edge).div_ceil(target_edge).min(source_edge);
                let mut sums = [0u32; 4];
                let mut count = 0u32;
                for sample_y in y0..y1.max(y0 + 1) {
                    for sample_x in x0..x1.max(x0 + 1) {
                        let from = ((sample_y * source_edge + sample_x) * 4) as usize;
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += u32::from(source[from + channel]);
                        }
                        count += 1;
                    }
                }
                let to = ((y * target_edge + x) * 4) as usize;
                for (channel, sum) in sums.iter().enumerate() {
                    output[to + channel] = (sum / count) as u8;
                }
            }
        }
    }
    output
}

/// Builds the full mip chain (`size`, `size / 2`, ..., 1) for a packed catalogue.
fn generate_mips(base: &[u8], size: u32) -> Vec<Vec<u8>> {
    let mut mips = Vec::with_capacity((u32::BITS - size.leading_zeros()) as usize);
    mips.push(base.to_vec());
    let mut edge = size;
    while edge > 1 {
        let next_edge = edge / 2;
        let source = mips.last().expect("the base mip is always present");
        mips.push(downsample(source, edge, next_edge));
        edge = next_edge;
    }
    mips
}

/// Box-filters every layer to `next_edge`, matching the procedural pack rules.
///
/// When `edge` is odd the final source row/column is not covered by the next
/// level (`floor(edge / 2)`); that is deterministic and keeps every level square.
fn downsample(source: &[u8], edge: u32, next_edge: u32) -> Vec<u8> {
    debug_assert_eq!(source.len(), (COUNT * (edge * edge * 4) as usize));
    debug_assert!(next_edge >= 1 && next_edge * 2 <= edge);
    let source_layer = (edge * edge * 4) as usize;
    let next_layer = (next_edge * next_edge * 4) as usize;
    let mut output = vec![0u8; COUNT * next_layer];
    for layer in 0..COUNT {
        let cut_out = is_cut_out_layer(layer);
        for y in 0..next_edge {
            for x in 0..next_edge {
                let mut sums = [0u32; 4];
                let mut count = 0u32;
                for dy in 0..2u32 {
                    let sample_y = y * 2 + dy;
                    if sample_y >= edge {
                        continue;
                    }
                    for dx in 0..2u32 {
                        let sample_x = x * 2 + dx;
                        if sample_x >= edge {
                            continue;
                        }
                        let from = layer * source_layer + ((sample_y * edge + sample_x) * 4) as usize;
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += u32::from(source[from + channel]);
                        }
                        count += 1;
                    }
                }
                let to = layer * next_layer + ((y * next_edge + x) * 4) as usize;
                for channel in 0..3 {
                    output[to + channel] = (sums[channel] / count) as u8;
                }
                let alpha = (sums[3] / count) as u8;
                output[to + 3] = if cut_out {
                    if alpha < ALPHA_CUTOFF { 0 } else { 255 }
                } else {
                    255
                };
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::textures::BlockTextureArray;
    use ob_content::textures::{COBBLESTONE, CRAFTING_TABLE_SIDE, CRAFTING_TABLE_TOP, PLANKS, WATER};

    use super::*;

    /// Non-debug catalogue files and the NAMES index they feed, in pack order.
    const PACK_FILES: [(&str, &str); 37] = [
        ("grass_top", "grass_block_top.png"),
        ("grass_side", "grass_block_side.png"),
        ("dirt", "dirt.png"),
        ("stone", "stone.png"),
        ("sand", "sand.png"),
        ("water", "water_still.png"),
        ("log_side", "oak_log.png"),
        ("log_top", "oak_log_top.png"),
        ("leaves", "oak_leaves.png"),
        ("bedrock", "bedrock.png"),
        ("gravel", "gravel.png"),
        ("clay", "clay.png"),
        ("snow", "snow.png"),
        ("planks", "oak_planks.png"),
        ("cobblestone", "cobblestone.png"),
        ("crafting_table_top", "crafting_table_top.png"),
        ("crafting_table_side", "crafting_table_side.png"),
        ("coal_ore", "coal_ore.png"),
        ("iron_ore", "iron_ore.png"),
        ("copper_ore", "copper_ore.png"),
        ("furnace_top", "furnace_top.png"),
        ("furnace_side", "furnace_side.png"),
        ("furnace_front", "furnace_front.png"),
        ("chest_top", "chest_top.png"),
        ("chest_side", "chest_side.png"),
        ("chest_front", "chest_front.png"),
        ("torch", "torch.png"),
        ("farmland_dry", "farmland.png"),
        ("farmland_wet", "farmland_moist.png"),
        ("wheat_0", "wheat_stage0.png"),
        ("wheat_1", "wheat_stage1.png"),
        ("wheat_2", "wheat_stage2.png"),
        ("wheat_3", "wheat_stage3.png"),
        ("bed_foot_top", "bed_foot_top.png"),
        ("bed_foot_side", "bed_foot_side.png"),
        ("bed_head_top", "bed_head_top.png"),
        ("bed_head_side", "bed_head_side.png"),
    ];

    /// Scratch directory that is removed when the test finishes.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "ob-resource-pack-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                unique
            ));
            fs::create_dir_all(&path).expect("create the test pack directory");
            Self { path }
        }

        fn block_dir(&self) -> PathBuf {
            self.path.join("assets/minecraft/textures/block")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the texture directory");
        }
        let file = fs::File::create(path).expect("create the test PNG");
        let mut encoder = png::Encoder::new(io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write the test PNG header");
        writer.write_image_data(rgba).expect("write the test PNG pixels");
    }

    /// Writes a paletted PNG with optional tRNS (alpha per palette entry).
    fn write_indexed_png(path: &Path, size: u32, palette: &[[u8; 3]], trns: Option<&[u8]>, indices: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the texture directory");
        }
        let file = fs::File::create(path).expect("create the indexed test PNG");
        let mut encoder = png::Encoder::new(io::BufWriter::new(file), size, size);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        let mut rgb = Vec::with_capacity(palette.len() * 3);
        for colour in palette {
            rgb.extend_from_slice(colour);
        }
        encoder.set_palette(rgb);
        if let Some(alpha) = trns {
            encoder.set_trns(alpha.to_vec());
        }
        let mut writer = encoder.write_header().expect("write indexed PNG header");
        writer.write_image_data(indices).expect("write indexed PNG pixels");
    }

    #[test]
    fn indexed_palette_and_trns_pngs_are_normalised() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        // Leaves: palette entry 1 is fully transparent and entry 2 is
        // semi-transparent; the cut-out rule turns both into 0/255.
        let indices = vec![1u8; (16 * 16) as usize];
        write_indexed_png(
            &block_dir.join("oak_leaves.png"),
            16,
            &[[255, 0, 0], [0, 255, 0], [0, 0, 255]],
            Some(&[255, 0, 200]),
            &indices,
        );
        // Stone: a semi-transparent palette entry must still become opaque.
        let stone_indices = vec![2u8; (16 * 16) as usize];
        write_indexed_png(
            &block_dir.join("stone.png"),
            16,
            &[[255, 0, 0], [0, 255, 0], [0, 0, 255], [1, 2, 3]],
            Some(&[255, 0, 200]),
            &stone_indices,
        );
        let layers = load_resource_pack(&pack.path).expect("indexed PNGs must decode");
        let leaves = layer_bytes(&layers.mips[0], 16, ob_content::textures::LEAVES.0 as usize);
        for pixel in leaves.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 0, "transparent palette entry must be cut out");
        }
        let stone = layer_bytes(&layers.mips[0], 16, ob_content::textures::STONE.0 as usize);
        for pixel in stone.as_chunks::<4>().0 {
            assert_eq!(pixel[3], 255, "non-leaf palette alpha must be forced opaque");
            assert_eq!(&pixel[..3], &[0, 0, 255]);
        }

        // A palette index beyond the table is a decoder error, never a panic.
        let mut invalid = vec![0u8; (16 * 16) as usize];
        invalid[0] = 5;
        write_indexed_png(
            &block_dir.join("stone.png"),
            16,
            &[[255, 0, 0], [0, 255, 0]],
            None,
            &invalid,
        );
        // The png crate normalises an out-of-range palette index rather than
        // panicking; either a valid layer or a typed error is acceptable, but
        // never a crash or an unbounded allocation.
        if let Ok(layers) = load_resource_pack(&pack.path) {
            assert_eq!(layers.size, 16);
            for layer in &layers.mips {
                assert!(!layer.is_empty());
            }
        }

        // A truncated indexed PNG is a typed error.
        write_solid(&block_dir.join("stone.png"), 16, [9, 9, 9, 255]);
        let bytes = fs::read(block_dir.join("stone.png")).expect("read valid PNG");
        fs::write(block_dir.join("stone.png"), &bytes[..bytes.len() / 2]).expect("truncate PNG");
        assert!(load_resource_pack(&pack.path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pathological_pack_paths_do_not_escape_or_panic() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        // A self-referential symlink is not a directory and must not loop.
        let loop_root = TempDir::new();
        let link = loop_root.path.join("loop");
        symlink(&link, &link).expect("create the symlink loop");
        assert!(matches!(
            load_resource_pack(&link),
            Err(TextureLoadError::PackNotFound { .. })
        ));

        // Non-UTF-8 path components are legal on Linux (but not on every
        // filesystem; macOS rejects invalid UTF-8 byte sequences with EILSEQ).
        // Where the platform can create the directory, the loader must load it
        // normally; where it cannot, the loader must still fail gracefully.
        let root = TempDir::new();
        let weird = root.path.join(OsString::from_vec(vec![b'p', 0x80, b'x']));
        let block_dir = weird.join("assets/minecraft/textures/block");
        if std::fs::create_dir_all(&block_dir).is_ok() {
            write_standard_pack(&block_dir, 16, true);
            let layers = load_resource_pack(&weird).expect("a non-UTF-8 path must load");
            assert_eq!(layers.size, 16);
            assert!(!layers.source.to_string_lossy().is_empty());
        } else {
            assert!(matches!(
                load_resource_pack(&weird),
                Err(TextureLoadError::PackNotFound { .. } | TextureLoadError::Io { .. })
            ));
        }

        // `..` components resolve against the caller's path and stay inside the
        // pack the caller named; no panic and no unexpected absolute path.
        let parent = TempDir::new();
        let pack = parent.path.join("pack");
        write_standard_pack(&pack.join("assets/minecraft/textures/block"), 16, true);
        let dotted = parent.path.join("pack/../pack");
        let layers = load_resource_pack(&dotted).expect("a `..` path must resolve");
        assert_eq!(layers.size, 16);
        assert!(layers.block_dir.ends_with("assets/minecraft/textures/block"));
    }

    fn solid(size: u32, colour: [u8; 4]) -> Vec<u8> {
        let mut pixels = vec![0u8; (size * size * 4) as usize];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&colour);
        }
        pixels
    }

    fn layer_colour(names_index: usize) -> [u8; 4] {
        let index = names_index as u8;
        [
            index.wrapping_mul(17),
            255 - index.wrapping_mul(11),
            index.wrapping_mul(7),
            255,
        ]
    }

    fn write_solid(path: &Path, size: u32, colour: [u8; 4]) {
        write_png(path, size, size, &solid(size, colour));
    }

    /// Writes every non-debug expected texture as a solid `size`-square image.
    fn write_standard_pack(block_dir: &Path, size: u32, include_water: bool) {
        fs::create_dir_all(block_dir).expect("create the block directory");
        for (index, (name, file)) in PACK_FILES.iter().enumerate() {
            if !include_water && *name == "water" {
                continue;
            }
            write_solid(
                &block_dir.join(file),
                size,
                layer_colour(index + 1), // +1: NAMES[0] is debug
            );
        }
    }

    fn layer_bytes(pixels: &[u8], size: u32, names_index: usize) -> &[u8] {
        let stride = (size * size * 4) as usize;
        &pixels[names_index * stride..(names_index + 1) * stride]
    }

    fn pixel(layer: &[u8], edge: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * edge + x) * 4) as usize;
        layer[offset..offset + 4].try_into().expect("four channels")
    }

    fn assert_solid_layer(layer: &[u8], edge: u32, colour: [u8; 4]) {
        for (index, pixel) in layer.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(*pixel, colour, "texel {index} of a {edge}x{edge} layer differs");
        }
    }

    #[test]
    fn standard_pack_files_cover_every_non_debug_layer_in_order() {
        assert_eq!(PACK_FILES.len(), COUNT - 1);
        for (index, (name, file)) in PACK_FILES.iter().enumerate() {
            assert_eq!(NAMES[index + 1], *name, "PACK_FILES must follow NAMES order");
            assert!(
                candidate_files(name).contains(file),
                "{file} is not a candidate for {name}"
            );
        }
    }

    #[test]
    fn resolves_pack_root_assets_and_block_directories() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);

        let from_root = load_resource_pack(&pack.path).expect("load from the pack root");
        assert_eq!(from_root.block_dir, block_dir);
        assert_eq!(from_root.source, pack.path);
        assert_eq!(from_root.size, 16);
        assert_eq!(from_root.fallback_layers, vec![0]);
        assert_eq!(from_root.mips.len(), 5);

        let from_assets = load_resource_pack(&pack.path.join("assets/minecraft")).expect("load from assets/minecraft");
        assert_eq!(from_assets.block_dir, block_dir);
        assert_eq!(from_assets.size, 16);

        let from_block = load_resource_pack(&block_dir).expect("load from textures/block");
        assert_eq!(from_block.block_dir, block_dir);
        assert_eq!(from_block.size, 16);

        // A pack laid out as `<root>/block/*.png`.
        let bare = TempDir::new();
        let bare_block = bare.path.join("block");
        write_standard_pack(&bare_block, 16, true);
        let from_bare_block = load_resource_pack(&bare.path).expect("load from <root>/block");
        assert_eq!(from_bare_block.block_dir, bare_block);

        // A directory that directly contains the expected PNGs, whatever its name.
        let flat = TempDir::new();
        write_standard_pack(&flat.path, 16, true);
        let from_flat = load_resource_pack(&flat.path).expect("load from a flat texture directory");
        assert_eq!(from_flat.block_dir, flat.path);
    }

    #[test]
    fn resolution_is_the_largest_texture_clamped_and_small_ones_scale_nearest() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 8, true);

        // 8x8 is below the minimum, so every layer lands at 16x16.
        let upscaled = load_resource_pack(&pack.path).expect("load the small pack");
        assert_eq!(upscaled.size, 16);
        for (index, name) in NAMES.iter().enumerate().skip(1) {
            let colour = layer_colour(index);
            assert_solid_layer(layer_bytes(&upscaled.mips[0], 16, index), 16, colour);
            assert_eq!(NAMES[index], *name);
        }

        // Raising one texture to 32 makes the whole array 32x32. Solid textures
        // survive nearest scaling unchanged, so every texel must still match.
        write_solid(
            &block_dir.join("stone.png"),
            32,
            layer_colour(ob_content::textures::STONE.0 as usize),
        );
        let larger = load_resource_pack(&pack.path).expect("load the 32px pack");
        assert_eq!(larger.size, 32);
        for (index, _) in NAMES.iter().enumerate().skip(1) {
            assert_solid_layer(layer_bytes(&larger.mips[0], 32, index), 32, layer_colour(index));
        }

        // A source above the maximum is box-reduced and the array stays 256.
        write_solid(&block_dir.join("stone.png"), 257, [40, 90, 140, 255]);
        let clamped = load_resource_pack(&pack.path).expect("load the oversized pack");
        assert_eq!(clamped.size, 256);
        assert_solid_layer(
            layer_bytes(&clamped.mips[0], 256, ob_content::textures::STONE.0 as usize),
            256,
            [40, 90, 140, 255],
        );
    }

    #[test]
    fn animated_textures_use_the_first_frame_and_wide_textures_use_the_top_square() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);

        // 16x32 animated strip: red top frame over a blue second frame.
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let mut strip = vec![0u8; (16 * 32 * 4) as usize];
        for (index, pixel) in strip.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let row = index / 16;
            pixel.copy_from_slice(if row < 16 { &red } else { &blue });
        }
        write_png(&block_dir.join("stone.png"), 16, 32, &strip);

        let layers = load_resource_pack(&pack.path).expect("load the animated pack");
        assert_eq!(layers.size, 16);
        let stone = ob_content::textures::STONE.0 as usize;
        assert_solid_layer(layer_bytes(&layers.mips[0], 16, stone), 16, red);

        // A 32x16 wide image keeps its top-left 16x16 square (left half red).
        let green = [0u8, 255, 0, 255];
        let mut wide = vec![0u8; (32 * 16 * 4) as usize];
        for y in 0..16 {
            for x in 0..32 {
                let offset = ((y * 32 + x) * 4) as usize;
                let colour = if x < 16 { red } else { green };
                wide[offset..offset + 4].copy_from_slice(&colour);
            }
        }
        write_png(&block_dir.join("stone.png"), 32, 16, &wide);

        let layers = load_resource_pack(&pack.path).expect("load the wide pack");
        assert_eq!(layers.size, 16);
        assert_solid_layer(layer_bytes(&layers.mips[0], 16, stone), 16, red);
    }

    #[test]
    fn leaf_alpha_is_cut_out_and_every_other_layer_is_opaque() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);

        let leaves = ob_content::textures::LEAVES.0 as usize;
        let mut leaf_pixels = solid(16, [30, 200, 40, 255]);
        let alphas = [0u8, 1, 64, 127, 128, 129, 200, 255];
        for (index, alpha) in alphas.iter().enumerate() {
            leaf_pixels[index * 4 + 3] = *alpha;
        }
        write_png(&block_dir.join("oak_leaves.png"), 16, 16, &leaf_pixels);

        // A pack with fully transparent non-leaf pixels must still be opaque.
        write_png(&block_dir.join("stone.png"), 16, 16, &solid(16, [90, 90, 90, 0]));

        let layers = load_resource_pack(&pack.path).expect("load the alpha pack");
        assert_eq!(layers.size, 16);
        let leaf_layer = layer_bytes(&layers.mips[0], 16, leaves);
        for (index, alpha) in alphas.iter().enumerate() {
            let expected = if *alpha < 128 { 0 } else { 255 };
            assert_eq!(pixel(leaf_layer, 16, index as u32, 0)[3], expected, "alpha {alpha}");
        }

        for (index, name) in NAMES.iter().enumerate() {
            if index == leaves {
                continue;
            }
            let layer = layer_bytes(&layers.mips[0], 16, index);
            for (texel, rgba) in layer.as_chunks::<4>().0.iter().enumerate() {
                assert_eq!(rgba[3], 255, "layer {index} ({name}) texel {texel} is not opaque");
            }
        }
    }

    #[test]
    fn missing_textures_fall_back_to_the_procedural_style() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, false);

        let layers = load_resource_pack(&pack.path).expect("load a pack without water");
        assert_eq!(layers.size, 16);
        assert_eq!(
            layers.fallback_layers,
            vec![
                ob_content::textures::DEBUG.0 as usize,
                ob_content::textures::WATER.0 as usize
            ]
        );
        assert!(layers.fallback_layers.windows(2).all(|pair| pair[0] < pair[1]));

        let classic = TexturePack::generate(TexturePackStyle::Classic);
        for &index in &layers.fallback_layers {
            assert_eq!(
                layer_bytes(&layers.mips[0], 16, index),
                classic.layer(0, index).expect("fallback layer"),
                "fallback layer {index} ({}) differs from Classic",
                NAMES[index]
            );
        }

        // The fallback style is configurable for the GPU helpers; Minimal is
        // also 16x16, so its water layer must be used verbatim.
        let minimal = load_resource_pack_with_fallback(&pack.path, TexturePackStyle::Minimal)
            .expect("load with the Minimal fallback");
        let minimal_water = TexturePack::generate(TexturePackStyle::Minimal)
            .layer(0, ob_content::textures::WATER.0 as usize)
            .expect("Minimal water layer")
            .to_vec();
        assert_eq!(
            layer_bytes(&minimal.mips[0], 16, ob_content::textures::WATER.0 as usize),
            minimal_water.as_slice()
        );
    }

    #[test]
    fn mip_chain_is_complete_and_box_filtered() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);

        // A per-texel gradient gives each 2x2 box filter a predictable average.
        let stone = ob_content::textures::STONE.0 as usize;
        let mut gradient = vec![0u8; (16 * 16 * 4) as usize];
        for y in 0..16u32 {
            for x in 0..16u32 {
                let offset = ((y * 16 + x) * 4) as usize;
                gradient[offset..offset + 4].copy_from_slice(&[(x * 16) as u8, (y * 16) as u8, 40, 255]);
            }
        }
        write_png(&block_dir.join("stone.png"), 16, 16, &gradient);

        // Semi-transparent leaves exercise the 128 cut-out in every mip.
        let leaves = ob_content::textures::LEAVES.0 as usize;
        let mut leaf_pixels = vec![0u8; (16 * 16 * 4) as usize];
        for (index, texel) in leaf_pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            texel.copy_from_slice(&[20, 160, 30, (index * 37 % 256) as u8]);
        }
        write_png(&block_dir.join("oak_leaves.png"), 16, 16, &leaf_pixels);

        let layers = load_resource_pack(&pack.path).expect("load the mip pack");
        assert_eq!(layers.size, 16);
        assert_eq!(layers.mips.len(), 5);
        for (level, mip) in layers.mips.iter().enumerate() {
            let edge = 16u32 >> level;
            assert_eq!(mip.len(), COUNT * (edge * edge * 4) as usize, "mip {level} length");
            for layer in mip.chunks_exact((edge * edge * 4) as usize) {
                assert!([0u8, 255].contains(&layer[3]));
            }
        }
        assert_eq!(
            pixel(layer_bytes(&layers.mips[1], 8, stone), 8, 0, 0),
            [8, 8, 40, 255],
            "mip 1 must average the top-left 2x2 texels"
        );
        let leaves_mip = layer_bytes(&layers.mips[1], 8, leaves);
        for (index, rgba) in leaves_mip.as_chunks::<4>().0.iter().enumerate() {
            assert!(
                rgba[3] == 0 || rgba[3] == 255,
                "mip 1 leaf texel {index} alpha is {}",
                rgba[3]
            );
        }
        for (level, mip) in layers.mips.iter().enumerate().skip(1) {
            let edge = 16u32 >> level;
            let stone_mip = layer_bytes(mip, edge, stone);
            assert!(
                stone_mip.as_chunks::<4>().0.iter().all(|rgba| rgba[3] == 255),
                "mip {level} must keep non-leaf layers opaque"
            );
        }

        // Non-power-of-two source sizes are used as-is; the chain follows
        // floor(edge / 2) so it still matches wgpu's mip dimensions.
        write_solid(&block_dir.join("stone.png"), 24, layer_colour(stone));
        let odd = load_resource_pack(&pack.path).expect("load the 24px pack");
        assert_eq!(odd.size, 24);
        let edges: Vec<u32> = odd
            .mips
            .iter()
            .map(|mip| mip.len() as u32 / (COUNT as u32 * 4))
            .map(u32::isqrt)
            .collect();
        assert_eq!(edges, vec![24, 12, 6, 3, 1]);
    }

    #[test]
    fn legacy_and_alternate_minecraft_file_names_resolve() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        fs::create_dir_all(&block_dir).expect("create the block directory");
        // Use the non-preferred spelling for every catalogue entry that has one.
        let files = [
            (
                "grass_top.png",
                layer_colour(ob_content::textures::GRASS_TOP.0 as usize),
            ),
            (
                "grass_side.png",
                layer_colour(ob_content::textures::GRASS_SIDE.0 as usize),
            ),
            ("dirt.png", layer_colour(ob_content::textures::DIRT.0 as usize)),
            ("stone.png", layer_colour(ob_content::textures::STONE.0 as usize)),
            ("sand.png", layer_colour(ob_content::textures::SAND.0 as usize)),
            ("water.png", layer_colour(ob_content::textures::WATER.0 as usize)),
            (
                "oak_log_side.png",
                layer_colour(ob_content::textures::LOG_SIDE.0 as usize),
            ),
            (
                "log_oak_top.png",
                layer_colour(ob_content::textures::LOG_TOP.0 as usize),
            ),
            ("leaves_oak.png", layer_colour(ob_content::textures::LEAVES.0 as usize)),
            ("bedrock.png", layer_colour(ob_content::textures::BEDROCK.0 as usize)),
            ("gravel.png", layer_colour(ob_content::textures::GRAVEL.0 as usize)),
            ("clay.png", layer_colour(ob_content::textures::CLAY.0 as usize)),
            ("snow.png", layer_colour(ob_content::textures::SNOW.0 as usize)),
            ("planks_oak.png", layer_colour(ob_content::textures::PLANKS.0 as usize)),
            (
                "cobblestone.png",
                layer_colour(ob_content::textures::COBBLESTONE.0 as usize),
            ),
            (
                "crafting_table_top.png",
                layer_colour(ob_content::textures::CRAFTING_TABLE_TOP.0 as usize),
            ),
            (
                "crafting_table_side.png",
                layer_colour(ob_content::textures::CRAFTING_TABLE_SIDE.0 as usize),
            ),
            ("coal_ore.png", layer_colour(ob_content::textures::COAL_ORE.0 as usize)),
            ("iron_ore.png", layer_colour(ob_content::textures::IRON_ORE.0 as usize)),
            (
                "copper_ore.png",
                layer_colour(ob_content::textures::COPPER_ORE.0 as usize),
            ),
            (
                "furnace_top.png",
                layer_colour(ob_content::textures::FURNACE_TOP.0 as usize),
            ),
            (
                "furnace_side.png",
                layer_colour(ob_content::textures::FURNACE_SIDE.0 as usize),
            ),
            (
                "furnace_front.png",
                layer_colour(ob_content::textures::FURNACE_FRONT.0 as usize),
            ),
            (
                "chest_top.png",
                layer_colour(ob_content::textures::CHEST_TOP.0 as usize),
            ),
            (
                "chest_side.png",
                layer_colour(ob_content::textures::CHEST_SIDE.0 as usize),
            ),
            (
                "chest_front.png",
                layer_colour(ob_content::textures::CHEST_FRONT.0 as usize),
            ),
            ("torch.png", layer_colour(ob_content::textures::TORCH.0 as usize)),
            (
                "farmland.png",
                layer_colour(ob_content::textures::FARMLAND_DRY.0 as usize),
            ),
            (
                "farmland_moist.png",
                layer_colour(ob_content::textures::FARMLAND_WET.0 as usize),
            ),
            (
                "wheat_stage0.png",
                layer_colour(ob_content::textures::WHEAT_0.0 as usize),
            ),
            (
                "wheat_stage1.png",
                layer_colour(ob_content::textures::WHEAT_1.0 as usize),
            ),
            (
                "wheat_stage2.png",
                layer_colour(ob_content::textures::WHEAT_2.0 as usize),
            ),
            (
                "wheat_stage3.png",
                layer_colour(ob_content::textures::WHEAT_3.0 as usize),
            ),
            (
                "bed_foot_top.png",
                layer_colour(ob_content::textures::BED_FOOT_TOP.0 as usize),
            ),
            (
                "bed_foot_side.png",
                layer_colour(ob_content::textures::BED_FOOT_SIDE.0 as usize),
            ),
            (
                "bed_head_top.png",
                layer_colour(ob_content::textures::BED_HEAD_TOP.0 as usize),
            ),
            (
                "bed_head_side.png",
                layer_colour(ob_content::textures::BED_HEAD_SIDE.0 as usize),
            ),
        ];
        for (file, colour) in files {
            write_solid(&block_dir.join(file), 16, colour);
        }

        let layers = load_resource_pack(&pack.path).expect("load the alternate-name pack");
        assert_eq!(layers.fallback_layers, vec![ob_content::textures::DEBUG.0 as usize]);
        for (file, colour) in files {
            let names_index = NAMES
                .iter()
                .position(|name| candidate_files(name).contains(&file))
                .expect("every alternate file maps to a catalogue entry");
            assert_solid_layer(layer_bytes(&layers.mips[0], 16, names_index), 16, colour);
        }

        // `water_flow.png` is the last water candidate.
        fs::remove_file(block_dir.join("water.png")).expect("remove water.png");
        write_solid(
            &block_dir.join("water_flow.png"),
            16,
            layer_colour(ob_content::textures::WATER.0 as usize),
        );
        let layers = load_resource_pack(&pack.path).expect("load with water_flow.png");
        assert!(
            !layers
                .fallback_layers
                .contains(&(ob_content::textures::WATER.0 as usize))
        );
    }

    #[test]
    fn new_block_candidates_match_vanilla_file_names() {
        assert_eq!(
            candidate_files("planks"),
            &["oak_planks.png", "planks_oak.png", "planks.png"]
        );
        assert_eq!(candidate_files("cobblestone"), &["cobblestone.png"]);
        assert_eq!(candidate_files("crafting_table_top"), &["crafting_table_top.png"]);
        assert_eq!(candidate_files("crafting_table_side"), &["crafting_table_side.png"]);
        assert_eq!(candidate_files("coal_ore"), &["coal_ore.png"]);
        assert_eq!(candidate_files("iron_ore"), &["iron_ore.png"]);
        assert_eq!(candidate_files("copper_ore"), &["copper_ore.png"]);
        assert_eq!(candidate_files("furnace_top"), &["furnace_top.png"]);
        assert_eq!(candidate_files("furnace_side"), &["furnace_side.png"]);
        assert_eq!(candidate_files("furnace_front"), &["furnace_front.png"]);
        assert_eq!(candidate_files("chest_top"), &["chest_top.png"]);
        assert_eq!(candidate_files("chest_side"), &["chest_side.png"]);
        assert_eq!(candidate_files("chest_front"), &["chest_front.png"]);
        // The debug layer keeps no Minecraft file; every other catalogue name
        // must have at least one candidate.
        for name in NAMES {
            if name == "debug" {
                assert!(candidate_files(name).is_empty());
            } else {
                assert!(!candidate_files(name).is_empty(), "{name} has no candidate file");
            }
        }
        // A file must resolve to at most one catalogue layer.
        let mut seen = std::collections::HashSet::new();
        for name in NAMES {
            for file in candidate_files(name) {
                assert!(seen.insert(*file), "{file} is a candidate for more than one layer");
            }
        }
    }

    #[test]
    fn invalid_pngs_are_reported_without_panicking() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        let stone_path = block_dir.join("stone.png");
        fs::write(&stone_path, b"this is definitely not a PNG").expect("write the corrupt texture");

        let error = load_resource_pack(&pack.path).expect_err("corrupt PNG must fail");
        assert!(error.to_string().contains("stone.png"));
        match error {
            TextureLoadError::InvalidImage { path, message } => {
                assert_eq!(path, stone_path);
                assert!(!message.is_empty());
            }
            other => panic!("expected InvalidImage, got {other:?}"),
        }
    }

    #[test]
    fn empty_unrelated_and_oversized_directories_are_diagnosed() {
        let empty = TempDir::new();
        let empty_block = empty.block_dir();
        fs::create_dir_all(&empty_block).expect("create the empty block dir");
        match load_resource_pack(&empty.path).expect_err("an empty block directory must fail") {
            TextureLoadError::EmptyPack { path } => assert_eq!(path, empty_block),
            other => panic!("expected EmptyPack, got {other:?}"),
        }

        let unrelated = TempDir::new();
        let unrelated_block = unrelated.block_dir();
        fs::create_dir_all(&unrelated_block).expect("create the unrelated block dir");
        write_solid(&unrelated_block.join("not_a_block.png"), 8, [1, 2, 3, 255]);
        match load_resource_pack(&unrelated.path).expect_err("unrelated PNGs must fail") {
            TextureLoadError::NoTextures { path } => assert_eq!(path, unrelated_block),
            other => panic!("expected NoTextures, got {other:?}"),
        }

        let missing = TempDir::new();
        let missing_path = missing.path.join("not-a-pack");
        match load_resource_pack(&missing_path).expect_err("a missing path must fail") {
            TextureLoadError::PackNotFound { looked } => {
                assert!(looked.contains(&missing_path.join("assets/minecraft/textures/block")));
                assert!(looked.contains(&missing_path.join("textures/block")));
                assert!(looked.contains(&missing_path.join("block")));
                assert!(looked.contains(&missing_path));
            }
            other => panic!("expected PackNotFound, got {other:?}"),
        }
    }

    #[test]
    fn oversized_files_and_dimensions_are_rejected() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);

        let stone_path = block_dir.join("stone.png");
        let file = fs::File::create(&stone_path).expect("create the oversized file");
        file.set_len(MAX_FILE_BYTES + 1).expect("make the file oversized");
        match load_resource_pack(&pack.path).expect_err("oversized files must fail") {
            TextureLoadError::TooLarge { path, size, limit } => {
                assert_eq!(path, stone_path);
                assert_eq!(size, MAX_FILE_BYTES + 1);
                assert_eq!(limit, MAX_FILE_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }

        write_solid(&block_dir.join("stone.png"), 16, [1, 2, 3, 255]);
        let wide = vec![255u8; (2048 * 4) as usize];
        write_png(&block_dir.join("stone.png"), 2048, 1, &wide);
        match load_resource_pack(&pack.path).expect_err("oversized dimensions must fail") {
            TextureLoadError::UnsupportedSize { path, width, height } => {
                assert_eq!(path, stone_path);
                assert_eq!((width, height), (2048, 1));
            }
            other => panic!("expected UnsupportedSize, got {other:?}"),
        }
    }

    #[test]
    fn suffixed_normal_and_specular_maps_are_ignored() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        fs::remove_file(block_dir.join("stone.png")).expect("remove the base stone texture");
        write_solid(&block_dir.join("stone_n.png"), 16, [255, 0, 255, 255]);
        write_solid(&block_dir.join("stone_s.png"), 16, [255, 255, 0, 255]);

        let layers = load_resource_pack(&pack.path).expect("suffixed maps must not be loaded");
        let stone = ob_content::textures::STONE.0 as usize;
        assert!(layers.fallback_layers.contains(&stone));
        assert_eq!(
            layer_bytes(&layers.mips[0], 16, stone),
            TexturePack::generate(TexturePackStyle::Classic)
                .layer(0, stone)
                .expect("classic stone")
        );
    }

    #[test]
    fn loading_twice_is_byte_identical() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, false);
        let first = load_resource_pack(&pack.path).expect("first load");
        let second = load_resource_pack(&pack.path).expect("second load");
        assert_eq!(first, second);
        assert_eq!(first.mips, second.mips);
    }

    #[test]
    fn texture_sources_label_and_default() {
        assert_eq!(
            TextureSource::default(),
            TextureSource::Style(TexturePackStyle::Classic)
        );
        assert_eq!(TextureSource::Style(TexturePackStyle::Lush).label(), "lush");
        let root = PathBuf::from("/packs/My Pack");
        assert_eq!(TextureSource::ResourcePack(root.clone()).label(), "My Pack");
        assert_eq!(
            TextureSource::ResourcePack(root.join("assets/minecraft")).label(),
            "My Pack"
        );
    }

    #[test]
    fn scale_square_is_deterministic_and_box_filters_downscales() {
        // 2x2 source: two red texels over two blue texels averages to purple.
        let source = [255u8, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let scaled = scale_square(&source, 2, 1);
        assert_eq!(scaled, vec![127, 0, 127, 255]);

        // Upscaling repeats each source texel exactly.
        let doubled = scale_square(&source, 2, 4);
        for y in 0..4 {
            for x in 0..4 {
                let offset = (y * 4 + x) * 4;
                assert_eq!(
                    doubled[offset..offset + 4],
                    source[(y / 2 * 2 + x / 2) * 4..(y / 2 * 2 + x / 2) * 4 + 4]
                );
            }
        }
        assert_eq!(scale_square(&source, 2, 2), source);
    }

    /// Builds a headless device, returning `None` without an adapter.
    fn test_gpu() -> Option<(wgpu::Device, wgpu::Queue, String)> {
        if std::env::var_os("OPENBLOCK_NO_GPU_TESTS").is_some() {
            return None;
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let options = |power_preference| wgpu::RequestAdapterOptions {
            power_preference,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        };
        let adapter = pollster::block_on(instance.request_adapter(&options(wgpu::PowerPreference::LowPower)))
            .or_else(|_| pollster::block_on(instance.request_adapter(&options(wgpu::PowerPreference::default()))))
            .ok()?;
        let info = adapter.get_info();
        let summary = format!("{} ({}, {})", info.name, info.backend, info.driver);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ob-resource-pack-test"),
            ..Default::default()
        }))
        .ok()?;
        Some((device, queue, summary))
    }

    #[test]
    fn gpu_uploads_a_synthetic_pack_with_the_expected_resolution_and_source() {
        let Some((device, queue, summary)) = test_gpu() else {
            eprintln!("skipping gpu_uploads_a_synthetic_pack_with_the_expected_resolution_and_source: no GPU adapter");
            return;
        };
        eprintln!("resource-pack GPU test: {summary}");

        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 8, true);
        let source = TextureSource::ResourcePack(pack.path.clone());

        let textures =
            BlockTextureArray::new_with_source(&device, &queue, &source).expect("upload the synthetic resource pack");
        assert_eq!(textures.source(), &source);
        assert_eq!(textures.resolution(), 16, "8x8 textures clamp up to 16x16");
        assert_eq!(textures.style(), TexturePackStyle::Classic);

        let lush = BlockTextureArray::from_resource_pack(&device, &queue, &pack.path, TexturePackStyle::Lush)
            .expect("upload with a Lush fallback");
        assert_eq!(lush.source(), &source);
        assert_eq!(lush.resolution(), 16);
        assert_eq!(lush.style(), TexturePackStyle::Lush);

        let procedural =
            BlockTextureArray::new_with_source(&device, &queue, &TextureSource::Style(TexturePackStyle::Minimal))
                .expect("style sources never fail");
        assert_eq!(procedural.source(), &TextureSource::Style(TexturePackStyle::Minimal));
        assert_eq!(procedural.style(), TexturePackStyle::Minimal);
        assert_eq!(procedural.resolution(), 16);

        // A non-power-of-two array size must still produce a wgpu-valid mip chain.
        write_solid(&block_dir.join("stone.png"), 24, [90, 90, 90, 255]);
        let odd = BlockTextureArray::new_with_source(&device, &queue, &source).expect("upload the 24x24 pack");
        assert_eq!(odd.resolution(), 24);
    }

    /// Manual smoke test against the rotrBLOCKS pack installed on this machine.
    ///
    /// Ignored by default because the pack is user-local and must never be a
    /// build or test dependency. Run with:
    /// `./dev-cargo test -p ob_render loads_the_local_rotrblocks_pack -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the user's local rotrBLOCKS resource pack from Downloads"]
    fn loads_the_local_rotrblocks_pack() {
        let root = Path::new("/home/martintondel/Downloads/rotrBLOCKS V88 [3D Foliage] 128x 26.3-1.13");
        if !root.is_dir() {
            eprintln!(
                "skipping loads_the_local_rotrblocks_pack: {} is not present",
                root.display()
            );
            return;
        }

        // Both the resource-pack root and its assets/minecraft subdirectory must
        // resolve to the same block directory.
        for candidate in [root.to_path_buf(), root.join("assets/minecraft")] {
            let layers = load_resource_pack(&candidate)
                .unwrap_or_else(|error| panic!("failed to load {}: {error}", candidate.display()));
            let fallbacks: Vec<&str> = layers.fallback_layers.iter().map(|&index| NAMES[index]).collect();
            eprintln!(
                "local rotrBLOCKS pack: source={} block_dir={} size={}x{} mip_levels={}",
                layers.source.display(),
                layers.block_dir.display(),
                layers.size,
                layers.size,
                layers.mips.len()
            );
            eprintln!("  fallback layers ({}): {:?}", fallbacks.len(), fallbacks);

            // The reference pack is 128x128. It ships the v1.0 ore and furnace
            // faces but keeps chest textures under `entity/chest` (outside the
            // block atlas), and has no still water; those layers fall back.
            assert_eq!(layers.size, 128, "unexpected reference-pack resolution");
            assert_eq!(
                fallbacks,
                vec!["debug", "water", "chest_top", "chest_side", "chest_front"]
            );
            for (label, index) in [
                ("leaves", LEAVES.0 as usize),
                ("grass_top", GRASS_TOP.0 as usize),
                ("stone", ob_content::textures::STONE.0 as usize),
                ("planks", PLANKS.0 as usize),
                ("cobblestone", COBBLESTONE.0 as usize),
                ("crafting_table_top", CRAFTING_TABLE_TOP.0 as usize),
                ("crafting_table_side", CRAFTING_TABLE_SIDE.0 as usize),
                ("coal_ore", ob_content::textures::COAL_ORE.0 as usize),
                ("iron_ore", ob_content::textures::IRON_ORE.0 as usize),
                ("copper_ore", ob_content::textures::COPPER_ORE.0 as usize),
                ("furnace_top", ob_content::textures::FURNACE_TOP.0 as usize),
                ("furnace_side", ob_content::textures::FURNACE_SIDE.0 as usize),
                ("furnace_front", ob_content::textures::FURNACE_FRONT.0 as usize),
            ] {
                assert!(
                    !layers.fallback_layers.contains(&index),
                    "{label} must be loaded from the pack, not fall back; fallbacks: {fallbacks:?}"
                );
            }
            assert!(
                layers.fallback_layers.contains(&(WATER.0 as usize)),
                "the reference pack has no water texture, so water must fall back"
            );
            for (label, index) in [
                ("chest_top", ob_content::textures::CHEST_TOP.0 as usize),
                ("chest_side", ob_content::textures::CHEST_SIDE.0 as usize),
                ("chest_front", ob_content::textures::CHEST_FRONT.0 as usize),
            ] {
                assert!(
                    layers.fallback_layers.contains(&index),
                    "{label} lives under entity/chest in the reference pack, so it must fall back"
                );
            }
        }
    }

    #[test]
    fn grayscale_grass_and_leaves_get_biome_tints() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        // Grayscale biome masks, as shipped by vanilla-style high-resolution packs.
        write_solid(&block_dir.join("grass_block_top.png"), 16, [128, 128, 128, 255]);
        write_solid(&block_dir.join("oak_leaves.png"), 16, [110, 110, 110, 255]);
        // A grayscale side overlay covering the top four rows only.
        let mut overlay = vec![0u8; 16 * 16 * 4];
        for y in 0..4u32 {
            for x in 0..16u32 {
                let at = ((y * 16 + x) * 4) as usize;
                overlay[at..at + 4].copy_from_slice(&[140, 140, 140, 255]);
            }
        }
        write_png(&block_dir.join("grass_block_side_overlay.png"), 16, 16, &overlay);

        let loaded = load_resource_pack(&pack.path).expect("load tinted pack");
        let grass_top = layer_bytes(&loaded.mips[0], 16, GRASS_TOP.0 as usize);
        let leaves = layer_bytes(&loaded.mips[0], 16, LEAVES.0 as usize);
        let side = layer_bytes(&loaded.mips[0], 16, GRASS_SIDE.0 as usize);
        for (label, layer) in [("grass_top", grass_top), ("leaves", leaves)] {
            let texel = pixel(layer, 16, 4, 4);
            assert!(
                texel[1] > texel[0] && texel[0] > texel[2],
                "{label} should be green after tinting, got {texel:?}"
            );
            assert_eq!(texel[3], 255, "{label} stays opaque");
        }
        // The overlay covers the top rows: tinted grass fringe.
        let fringe = pixel(side, 16, 4, 0);
        assert!(
            fringe[1] > fringe[0] && fringe[0] > fringe[2],
            "side fringe should be green: {fringe:?}"
        );
        // Rows below the overlay keep the dirt base (written by the standard pack).
        let dirt_base = pixel(side, 16, 4, 12);
        let expected_dirt = layer_colour(GRASS_SIDE.0 as usize);
        assert_eq!(dirt_base, expected_dirt, "uncovered side rows keep the base texture");
    }

    #[test]
    fn coloured_pack_textures_are_not_tinted() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        let green = [80, 160, 60, 255];
        write_solid(&block_dir.join("grass_block_top.png"), 16, green);
        write_solid(&block_dir.join("oak_leaves.png"), 16, [60, 130, 40, 255]);
        let loaded = load_resource_pack(&pack.path).expect("load coloured pack");
        let grass_top = layer_bytes(&loaded.mips[0], 16, GRASS_TOP.0 as usize);
        assert_eq!(pixel(grass_top, 16, 4, 4), green);
        let leaves = layer_bytes(&loaded.mips[0], 16, LEAVES.0 as usize);
        assert_eq!(pixel(leaves, 16, 4, 4), [60, 130, 40, 255]);
    }

    #[test]
    fn new_cutout_layers_can_be_overridden_by_a_resource_pack() {
        let pack = TempDir::new();
        let block_dir = pack.block_dir();
        write_standard_pack(&block_dir, 16, true);
        let mut torch = vec![0u8; (16 * 16 * 4) as usize];
        for y in 0..16u32 {
            for x in 0..16u32 {
                let offset = ((y * 16 + x) * 4) as usize;
                if x < 8 {
                    torch[offset..offset + 4].copy_from_slice(&[240, 180, 60, 200]);
                }
            }
        }
        write_png(&block_dir.join("torch.png"), 16, 16, &torch);
        let mut wheat = vec![0u8; (16 * 16 * 4) as usize];
        for y in 0..16u32 {
            for x in 0..16u32 {
                let offset = ((y * 16 + x) * 4) as usize;
                if x % 3 == 0 {
                    wheat[offset..offset + 4].copy_from_slice(&[80, 180, 60, 255]);
                }
            }
        }
        write_png(&block_dir.join("wheat_stage3.png"), 16, 16, &wheat);

        let layers = load_resource_pack(&pack.path).expect("new cut-out layers load");
        let torch_layer = layer_bytes(&layers.mips[0], 16, ob_content::textures::TORCH.0 as usize);
        let mut transparent = 0;
        let mut opaque = 0;
        for pixel in torch_layer.as_chunks::<4>().0 {
            assert!(pixel[3] == 0 || pixel[3] == 255, "torch alpha must be binary");
            if pixel[3] == 0 {
                transparent += 1;
            } else {
                opaque += 1;
                assert_eq!(&pixel[..3], &[240, 180, 60]);
            }
        }
        assert!(
            transparent > 0 && opaque > 0,
            "torch cut-out needs both holes and texels"
        );
        let wheat_layer = layer_bytes(&layers.mips[0], 16, ob_content::textures::WHEAT_3.0 as usize);
        assert!(wheat_layer.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 0));
        assert!(wheat_layer.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 255));

        // Farmland is opaque even when the pack supplies an alpha channel.
        let farmland = layer_bytes(&layers.mips[0], 16, ob_content::textures::FARMLAND_DRY.0 as usize);
        assert!(farmland.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
        let bed = layer_bytes(&layers.mips[0], 16, ob_content::textures::BED_HEAD_TOP.0 as usize);
        assert!(bed.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
    }
}

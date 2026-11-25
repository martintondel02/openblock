//! Runtime-rasterised Minecraft-style UI font atlas.
//!
//! The UI font is an embedded TrueType font ([`default_ui_font`], currently
//! Monocraft, SIL OFL 1.1) rendered into the same 16×8-cell, 128×64-texel RGBA
//! atlas layout as the built-in 8×8 bitmap title font. Keeping the layout
//! identical means [`crate::UiFrame::text`] and the title font share the same
//! glyph metrics: one printable ASCII character advances [`UI_GLYPH_SIZE`] (8)
//! pixels per line of scale and each line is [`UI_LINE_HEIGHT`] (8) pixels.
//!
//! Fonts are scaled by cap height rather than blindly by em size: uppercase
//! glyphs are normalised to `UI_CAP_HEIGHT_PIXELS` = 7 px, the classic
//! Minecraft cap height. Monocraft's caps are 840 of its 1080 units, so this
//! yields exactly a 9 px em, where every 120-unit design pixel maps to one
//! atlas texel. A font without a usable cap glyph falls back to
//! `UI_FALLBACK_EM_PIXELS` = 9 px. Each glyph outline is drawn with a baseline
//! at `UI_BASELINE_PIXELS` = 7 px from the top of its 8×8 cell with one pixel
//! of left bearing; caps and digits then occupy rows 0–6 and descenders row 7.
//! Coverage is thresholded at `UI_COVERAGE_THRESHOLD` = 0.5 to a crisp binary
//! alpha mask instead of an anti-aliased grey mask, matching the pixel-art
//! look of the bitmap font.
//!
//! User-provided fonts (for example a personal Minecraftia TTF) go through the
//! same rasteriser: [`crate::UiRenderer::set_ui_font`] replaces the UI atlas at
//! runtime and [`rasterize_ui_font`] reports why unusable bytes were rejected.

use std::fmt;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};

/// Width and height of one UI glyph cell, in logical pixels.
pub const UI_GLYPH_SIZE: f32 = 8.0;

/// Height of one UI text line at scale 1, in logical pixels.
pub const UI_LINE_HEIGHT: f32 = 8.0;

/// Number of glyph cells per atlas row.
pub const UI_ATLAS_COLUMNS: u32 = 16;

/// Number of glyph cell rows in the atlas.
pub const UI_ATLAS_ROWS: u32 = 8;

/// Width of the UI font atlas in texels.
pub const UI_ATLAS_WIDTH: u32 = UI_ATLAS_COLUMNS * 8;

/// Height of the UI font atlas in texels.
pub const UI_ATLAS_HEIGHT: u32 = UI_ATLAS_ROWS * 8;

/// First printable ASCII code point stored in the atlas.
pub const UI_FIRST_PRINTABLE: u32 = 32;

/// Last printable ASCII code point stored in the atlas.
pub const UI_LAST_PRINTABLE: u32 = 126;

/// Rendered cap height in pixels; caps and digits occupy rows 0–6.
const UI_CAP_HEIGHT_PIXELS: f32 = 7.0;

/// Fallback em size when a font has no measurable uppercase glyph.
///
/// Monocraft's outline coordinates are multiples of 120 units with 1080 units
/// per em, so 9 px maps every design pixel to exactly one atlas texel.
const UI_FALLBACK_EM_PIXELS: f32 = 9.0;

/// Baseline distance from the top of an 8×8 glyph cell, in pixels.
///
/// Caps and digits are 7 px tall, so they fill rows 0–6 and leave row 7 for
/// descenders such as `g` and `p`.
const UI_BASELINE_PIXELS: f32 = 7.0;

/// Horizontal distance from the cell's left edge to the glyph origin, in pixels.
///
/// Monocraft glyphs are 5 px wide; one pixel of left bearing leaves a balanced
/// 1 px gap before and 2 px after the ink inside the 8 px advance.
const UI_LEFT_BEARING_PIXELS: f32 = 1.0;

/// Coverage at or above which a texel becomes opaque font white.
const UI_COVERAGE_THRESHOLD: f32 = 0.5;

/// Largest glyph pixel extent accepted before rasterisation.
///
/// The atlas cell is only 8 px wide; anything dramatically larger indicates
/// pathological font metrics. Skipping it keeps a malformed user font from
/// asking the rasteriser to allocate an enormous coverage buffer.
const MAX_GLYPH_PIXELS: f32 = 64.0;

/// Embedded default UI font: Monocraft 4.2, SIL Open Font License 1.1.
const DEFAULT_UI_FONT: &[u8] = include_bytes!("../../../assets/ui-font/Monocraft.ttf");

/// Name reported for [`default_ui_font`].
const DEFAULT_UI_FONT_NAME: &str = "Monocraft";

/// A rasterised UI font atlas: 128×64 RGBA8 with white, binary-alpha glyphs.
///
/// The cells are laid out exactly like the title bitmap atlas: 16 columns by 8
/// rows of 8×8 cells, printable ASCII `32..=126` in reading order. Texels that
/// are not glyph ink are fully transparent `[0, 0, 0, 0]`; ink is opaque white
/// `[255, 255, 255, 255]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiFontAtlas {
    /// Atlas width in texels (always [`UI_ATLAS_WIDTH`]).
    pub width: u32,
    /// Atlas height in texels (always [`UI_ATLAS_HEIGHT`]).
    pub height: u32,
    /// Tightly packed RGBA8 pixels, top row first.
    pub pixels: Vec<u8>,
}

/// Why a font could not be loaded as the UI font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiFontError {
    /// The bytes are not a parseable TrueType/OpenType font.
    InvalidFont(String),
    /// The font parsed, but cannot produce a usable printable-ASCII atlas.
    UnsupportedFont(String),
}

impl fmt::Display for UiFontError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFont(reason) => write!(formatter, "invalid UI font: {reason}"),
            Self::UnsupportedFont(reason) => write!(formatter, "unsupported UI font: {reason}"),
        }
    }
}

impl std::error::Error for UiFontError {}

impl From<ab_glyph::InvalidFont> for UiFontError {
    fn from(error: ab_glyph::InvalidFont) -> Self {
        Self::InvalidFont(error.to_string())
    }
}

/// Returns the embedded default UI font bytes (Monocraft, SIL OFL 1.1).
pub fn default_ui_font() -> &'static [u8] {
    DEFAULT_UI_FONT
}

/// Returns the display name of the embedded default UI font.
pub fn default_ui_font_name() -> &'static str {
    DEFAULT_UI_FONT_NAME
}

/// Returns the unscaled height of a representative uppercase glyph.
///
/// `H`, `A` and `0` are tried in order because some fonts omit one of them.
fn cap_height_units(font: &FontVec) -> Option<f32> {
    ['H', 'A', '0'].into_iter().find_map(|character| {
        let glyph_id = font.glyph_id(character);
        if glyph_id.0 == 0 {
            return None;
        }
        font.outline(glyph_id).map(|outline| outline.bounds.height())
    })
}

/// Rasterises printable ASCII glyphs from `ttf` into the UI font atlas.
///
/// The atlas is deterministic for identical input bytes and never panics on
/// malformed or unusual fonts. Space and unsupported glyphs are transparent;
/// characters outside printable ASCII are not stored.
///
/// # Errors
///
/// Returns [`UiFontError::InvalidFont`] when the bytes are not a parseable
/// font, and [`UiFontError::UnsupportedFont`] when the font has unusable
/// metrics or no printable ASCII glyph produces any ink.
pub fn rasterize_ui_font(ttf: &[u8]) -> Result<UiFontAtlas, UiFontError> {
    let font = FontVec::try_from_vec(ttf.to_vec())?;

    // Scale so the font's em square is `UI_EM_PIXELS` tall. ab_glyph's pixel
    // scale is the font height (ascent - descent), not the em size, so convert
    // through the unscaled metrics.
    let units_per_em = font
        .units_per_em()
        .filter(|units| units.is_finite() && *units > 0.0)
        .ok_or_else(|| UiFontError::UnsupportedFont("font has no usable units-per-em".to_owned()))?;
    let unscaled_height = font.height_unscaled();
    if !unscaled_height.is_finite() || unscaled_height <= 0.0 {
        return Err(UiFontError::UnsupportedFont(
            "font has unusable vertical metrics".to_owned(),
        ));
    }
    // Normalise the cap height to 7 px so any Minecraft-style font (Monocraft,
    // Minecraftia, ...) fills the 8×8 cell like the classic bitmap font. For
    // Monocraft this evaluates to an exact 9 px em.
    let cap_units = cap_height_units(&font).filter(|cap| cap.is_finite() && *cap > 0.0);
    let pixel_scale = match cap_units {
        Some(cap) => unscaled_height * UI_CAP_HEIGHT_PIXELS / cap,
        None => UI_FALLBACK_EM_PIXELS * unscaled_height / units_per_em,
    };
    if !pixel_scale.is_finite() || pixel_scale <= 0.0 {
        return Err(UiFontError::UnsupportedFont(
            "font metrics produce an unusable pixel scale".to_owned(),
        ));
    }
    let scale = PxScale::from(pixel_scale);
    let scaled_font = font.as_scaled(scale);

    let mut atlas = UiFontAtlas {
        width: UI_ATLAS_WIDTH,
        height: UI_ATLAS_HEIGHT,
        pixels: vec![0u8; (UI_ATLAS_WIDTH * UI_ATLAS_HEIGHT * 4) as usize],
    };
    let mut any_ink = false;

    for code in UI_FIRST_PRINTABLE..=UI_LAST_PRINTABLE {
        let character = char::from_u32(code).unwrap_or('?');
        // Space advances the cursor but is never drawn.
        if character == ' ' {
            continue;
        }
        let glyph_id = scaled_font.glyph_id(character);
        // Glyph id 0 is `.notdef`; treat it as unsupported instead of drawing
        // a fallback box for a missing character.
        if glyph_id.0 == 0 {
            continue;
        }
        let index = code - UI_FIRST_PRINTABLE;
        let cell_x = index % UI_ATLAS_COLUMNS;
        let cell_y = index / UI_ATLAS_COLUMNS;
        let origin_x = (cell_x * 8) as f32 + UI_LEFT_BEARING_PIXELS;
        let origin_y = (cell_y * 8) as f32 + UI_BASELINE_PIXELS;
        let glyph = glyph_id.with_scale_and_position(scale, point(origin_x, origin_y));
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outlined.px_bounds();
        let (glyph_width, glyph_height) = (bounds.width(), bounds.height());
        if !glyph_width.is_finite()
            || !glyph_height.is_finite()
            || glyph_width > MAX_GLYPH_PIXELS
            || glyph_height > MAX_GLYPH_PIXELS
        {
            // Pathological metrics: drawing this glyph would allocate a huge
            // off-screen coverage buffer that is then clipped to 8×8 anyway.
            continue;
        }
        // `draw` reports coordinates relative to the outline's pixel bounds.
        // Offsets are kept as i64 so pathological metrics cannot overflow the
        // cell index arithmetic.
        let min_x = i64::from(bounds.min.x as i32);
        let min_y = i64::from(bounds.min.y as i32);
        let cell_left = i64::from(cell_x * 8);
        let cell_top = i64::from(cell_y * 8);
        outlined.draw(|x, y, coverage| {
            if coverage < UI_COVERAGE_THRESHOLD {
                return;
            }
            let texel_x = min_x + i64::from(x);
            let texel_y = min_y + i64::from(y);
            // A glyph may overhang its cell; clip rather than bleed into the
            // neighbouring character.
            if !(0..8).contains(&(texel_x - cell_left)) || !(0..8).contains(&(texel_y - cell_top)) {
                return;
            }
            let offset = ((texel_y * i64::from(UI_ATLAS_WIDTH) + texel_x) * 4) as usize;
            atlas.pixels[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            any_ink = true;
        });
    }

    if !any_ink {
        return Err(UiFontError::UnsupportedFont(
            "font has no printable ASCII glyphs".to_owned(),
        ));
    }
    Ok(atlas)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the `(x, y)` cell origin of a printable character.
    fn cell_origin(character: char) -> (u32, u32) {
        let index = character as u32 - UI_FIRST_PRINTABLE;
        (index % UI_ATLAS_COLUMNS, index / UI_ATLAS_COLUMNS)
    }

    /// Returns the four-byte RGBA texel at an absolute atlas coordinate.
    fn texel(atlas: &UiFontAtlas, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * atlas.width + x) * 4) as usize;
        [
            atlas.pixels[offset],
            atlas.pixels[offset + 1],
            atlas.pixels[offset + 2],
            atlas.pixels[offset + 3],
        ]
    }

    /// Counts opaque texels inside one glyph cell.
    fn cell_ink(atlas: &UiFontAtlas, character: char) -> usize {
        let (cell_x, cell_y) = cell_origin(character);
        let mut ink = 0;
        for y in 0..8 {
            for x in 0..8 {
                if texel(atlas, cell_x * 8 + x, cell_y * 8 + y)[3] != 0 {
                    ink += 1;
                }
            }
        }
        ink
    }

    /// FNV-1a hash, used to compare deterministic rasterisations.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    #[test]
    fn default_font_parses_into_the_expected_atlas() {
        assert_eq!(default_ui_font_name(), "Monocraft");
        assert!(!default_ui_font().is_empty());
        let atlas = rasterize_ui_font(default_ui_font()).expect("default font parses");
        assert_eq!(atlas.width, 128);
        assert_eq!(atlas.height, 64);
        assert_eq!(atlas.width, UI_ATLAS_WIDTH);
        assert_eq!(atlas.height, UI_ATLAS_HEIGHT);
        assert_eq!(atlas.pixels.len(), (128 * 64 * 4) as usize);
        assert!(atlas.pixels.as_chunks::<4>().0.iter().any(|texel| texel[3] == 255));
    }

    #[test]
    fn printable_glyphs_have_visible_pixels_and_space_is_empty() {
        let atlas = rasterize_ui_font(default_ui_font()).expect("default font parses");
        assert_eq!(cell_ink(&atlas, ' '), 0, "space must stay transparent");
        for code in UI_FIRST_PRINTABLE..=UI_LAST_PRINTABLE {
            let character = char::from_u32(code).expect("printable ASCII");
            if character == ' ' {
                continue;
            }
            assert!(
                cell_ink(&atlas, character) > 0,
                "glyph {character:?} has no visible texels"
            );
        }
        // Representative letters, digits and punctuation are visible and lie
        // inside their own cells.
        for character in ['A', 'Z', 'a', 'z', '0', '9', '!', '@', '~', 'g', 'p'] {
            assert!(
                cell_ink(&atlas, character) > 0,
                "glyph {character:?} has no visible texels"
            );
        }
    }

    #[test]
    fn atlas_alpha_is_binary_white_or_transparent() {
        let atlas = rasterize_ui_font(default_ui_font()).expect("default font parses");
        for texel in atlas.pixels.as_chunks::<4>().0.iter() {
            assert!(
                *texel == [0, 0, 0, 0] || *texel == [255, 255, 255, 255],
                "atlas texel is neither transparent nor white: {texel:?}"
            );
        }
        // The final two cell rows are unused and must stay transparent.
        for y in UI_ATLAS_HEIGHT - 16..UI_ATLAS_HEIGHT {
            for x in 0..UI_ATLAS_WIDTH {
                assert_eq!(texel(&atlas, x, y), [0, 0, 0, 0], "unused texel at ({x}, {y})");
            }
        }
    }

    #[test]
    fn rasterisation_is_deterministic() {
        let first = rasterize_ui_font(default_ui_font()).expect("default font parses");
        let second = rasterize_ui_font(default_ui_font()).expect("default font parses");
        assert_eq!(first, second);
        assert_eq!(fnv1a(&first.pixels), fnv1a(&second.pixels));
        assert_ne!(fnv1a(&first.pixels), 0);
        // The rasterised glyphs are crisp: every opaque texel is fully white.
        assert!(
            first
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|texel| *texel == [0; 4] || *texel == [255; 4])
        );
    }

    #[test]
    fn invalid_and_truncated_bytes_return_errors_without_panicking() {
        assert!(matches!(
            rasterize_ui_font(b"this is not a font"),
            Err(UiFontError::InvalidFont(_))
        ));
        assert!(matches!(rasterize_ui_font(&[]), Err(UiFontError::InvalidFont(_))));
        assert!(matches!(
            rasterize_ui_font(&vec![0x5a; 4096]),
            Err(UiFontError::InvalidFont(_))
        ));

        // Truncated prefixes of a valid font must never panic. Some truncations
        // still contain enough tables to parse; those must yield a well-formed
        // atlas, and the rest must be rejected.
        let font = default_ui_font();
        for length in [1, 4, 16, 64, 512, font.len() / 2, font.len() - 1] {
            let result = std::panic::catch_unwind(|| rasterize_ui_font(&font[..length]));
            assert!(result.is_ok(), "rasterising {length} truncated bytes panicked");
            if let Ok(atlas) = result.expect("not panicked") {
                assert_eq!(atlas.width, UI_ATLAS_WIDTH);
                assert_eq!(atlas.height, UI_ATLAS_HEIGHT);
                assert_eq!(atlas.pixels.len(), (UI_ATLAS_WIDTH * UI_ATLAS_HEIGHT * 4) as usize);
                assert!(
                    atlas
                        .pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|texel| *texel == [0; 4] || *texel == [255; 4])
                );
            }
        }

        // Errors carry a human-readable description and implement `Error`.
        let error = rasterize_ui_font(b"nope").expect_err("invalid font");
        assert!(error.to_string().contains("UI font"));
        assert!(std::error::Error::source(&error).is_none());
    }
}

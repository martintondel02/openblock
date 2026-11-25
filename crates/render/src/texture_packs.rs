//! Selectable procedural block-texture packs.
//!
//! The renderer ships four hand-authored pixel styles, all generated on the CPU
//! at start-up from an integer hash: no assets, no external RNG and no floating
//! point. Every style produces the same catalogue as
//! [`ob_content::textures::NAMES`] so the block table and the texture array stay
//! in lock-step.
//!
//! * [`TexturePackStyle::Classic`] — 16², clean Minecraft-inspired pixel art.
//! * [`TexturePackStyle::Painterly`] — 32², soft Hytale-inspired hand-painted
//!   surfaces: muted warm colours, low contrast and gentle ordered dithering.
//! * [`TexturePackStyle::Lush`] — 32², richer natural detail with mossy stone,
//!   detailed bark and turquoise water.
//! * [`TexturePackStyle::Minimal`] — 16², flat modern design: two or three
//!   crisp tones per block, thin accent lines and only a whisper of texture.
//!
//! Patterns are built from multi-scale value noise, short hand-placed strokes
//! and irregular feature shapes instead of per-texel salt. That keeps large
//! surfaces (grass fields, dirt cliffs, stone walls, canopies) calm at a
//! distance while still rewarding a close look.
//!
//! Leaves are alpha cut-outs: at mip 0 their alpha is exactly 0 or 255 and the
//! holes sit in clustered, irregular gaps. The box filter averages alpha into
//! the mip chain, and the world shader discards texels below 0.5, so distant
//! canopies stay solid instead of shimmering. Every other layer is fully opaque.
//!
//! A full box-filtered mip chain is generated for every layer. The sampler used
//! by [`crate::textures::BlockTextureArray`] keeps magnification crisp (nearest)
//! while minification and mip selection are linear, which preserves pixel-art
//! edges up close and avoids shimmering at distance.
//!
//! Determinism: every texel is a pure function of `(style, layer, x, y)`.
//! Generation never reads time, environment or floating-point state, so the
//! same style always yields byte-identical textures across runs and platforms.

use ob_content::textures::{COUNT, NAMES};

/// Number of layers in every generated pack (one per catalogue name).
pub const LAYER_COUNT: usize = COUNT;

/// Selected procedural texture-pack style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TexturePackStyle {
    /// 16² clean, saturated pixel art with a Minecraft-like read.
    #[default]
    Classic,
    /// 32² hand-painted feel: muted warm palette, soft gradients and dithering.
    Painterly,
    /// 32² high-detail natural look with rich saturation.
    Lush,
    /// 16² flat modern design with a pastel palette and crisp shapes.
    Minimal,
}

impl TexturePackStyle {
    /// Every style, in menu order.
    pub const ALL: [TexturePackStyle; 4] = [
        TexturePackStyle::Classic,
        TexturePackStyle::Painterly,
        TexturePackStyle::Lush,
        TexturePackStyle::Minimal,
    ];

    /// Stable identifier used by settings and command-line flags.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Painterly => "painterly",
            Self::Lush => "lush",
            Self::Minimal => "minimal",
        }
    }

    /// Parses a [`Self::name`] identifier.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "classic" => Some(Self::Classic),
            "painterly" => Some(Self::Painterly),
            "lush" => Some(Self::Lush),
            "minimal" => Some(Self::Minimal),
            _ => None,
        }
    }

    /// Edge length in texels of one layer in this style.
    pub fn resolution(&self) -> u32 {
        match self {
            Self::Classic | Self::Minimal => 16,
            Self::Painterly | Self::Lush => 32,
        }
    }

    /// Numeric seed mixed into every hash of this style.
    const fn seed(self) -> u32 {
        match self {
            Self::Classic => 0x0b1a_5eed,
            Self::Painterly => 0x9e37_79b9,
            Self::Lush => 0x51ed_2b3c,
            Self::Minimal => 0x7f4a_7c15,
        }
    }
}

/// A complete CPU-side pack: base layer plus every mip level.
///
/// Levels are stored packed in layer-major order, exactly as uploaded to the
/// GPU: level `l` is `COUNT` consecutive `edge × edge` RGBA8 layers with
/// `edge = resolution >> l`. Rows are tightly packed.
#[derive(Debug, Clone)]
pub struct TexturePack {
    style: TexturePackStyle,
    resolution: u32,
    mips: Vec<Vec<u8>>,
}

impl TexturePack {
    /// Generates a full style pack, including its mip chain.
    pub fn generate(style: TexturePackStyle) -> Self {
        let resolution = style.resolution();
        let layers = generate_layers(style, resolution);

        let mut mips = Vec::with_capacity(mip_level_count(resolution) as usize);
        let mut base = Vec::with_capacity(LAYER_COUNT * (resolution * resolution * 4) as usize);
        for layer in layers {
            debug_assert_eq!(layer.pixels.len(), layer.alpha.len());
            for (color, alpha) in layer.pixels.iter().zip(&layer.alpha) {
                base.extend_from_slice(&[color[0], color[1], color[2], *alpha]);
            }
        }
        mips.push(base);

        let mut edge = resolution;
        while edge > 1 {
            let next = downsample(mips.last().expect("base mip exists"), edge);
            edge /= 2;
            mips.push(next);
        }

        Self {
            style,
            resolution,
            mips,
        }
    }

    /// Style this pack was generated for.
    pub fn style(&self) -> TexturePackStyle {
        self.style
    }

    /// Base-level edge length in texels.
    pub fn resolution(&self) -> u32 {
        self.resolution
    }

    /// Number of mip levels in the chain (base level included).
    pub fn mip_level_count(&self) -> u32 {
        self.mips.len() as u32
    }

    /// Edge length of one mip level, or `None` when `level` is out of range.
    pub fn mip_edge(&self, level: u32) -> Option<u32> {
        (level < self.mip_level_count()).then(|| self.resolution >> level)
    }

    /// All layers of one mip level in packed RGBA8 order.
    pub fn mip(&self, level: u32) -> Option<&[u8]> {
        self.mips.get(level as usize).map(Vec::as_slice)
    }

    /// One layer (all rows, tightly packed) of one mip level.
    pub fn layer(&self, level: u32, layer: usize) -> Option<&[u8]> {
        if layer >= LAYER_COUNT {
            return None;
        }
        let edge = self.mip_edge(level)? as usize;
        let bytes = edge * edge * 4;
        self.mip(level).map(|data| &data[layer * bytes..(layer + 1) * bytes])
    }
}

/// Generates the pack for `style`.
pub fn generate_texture_pack(style: TexturePackStyle) -> TexturePack {
    TexturePack::generate(style)
}

/// Number of mip levels needed for a square `resolution` (`log2 + 1`).
fn mip_level_count(resolution: u32) -> u32 {
    debug_assert!(
        resolution.is_power_of_two(),
        "texture resolution must be a power of two"
    );
    resolution.trailing_zeros() + 1
}

/// Box-filters every layer of `source` down to half its edge length.
///
/// Colour and alpha are both averaged; the world shader thresholds alpha at
/// 0.5, so cut-out leaf holes dissolve into solid foliage as mip levels shrink.
fn downsample(source: &[u8], edge: u32) -> Vec<u8> {
    let half = edge / 2;
    let mut out = vec![0u8; LAYER_COUNT * (half * half * 4) as usize];
    for layer in 0..LAYER_COUNT {
        let src_layer = layer * (edge * edge * 4) as usize;
        let dst_layer = layer * (half * half * 4) as usize;
        for y in 0..half as usize {
            for x in 0..half as usize {
                let mut sums = [0u32; 4];
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let texel = src_layer + (((y * 2 + dy) * edge as usize + (x * 2 + dx)) * 4);
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += u32::from(source[texel + channel]);
                        }
                    }
                }
                let dst = dst_layer + (y * half as usize + x) * 4;
                for (channel, sum) in sums.iter().enumerate() {
                    out[dst + channel] = (sum / 4) as u8;
                }
            }
        }
    }
    out
}

/// Generates the base mip of every catalogue layer for `style`.
fn generate_layers(style: TexturePackStyle, resolution: u32) -> Vec<Canvas> {
    NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let index = index as u32;
            let canvas = match style {
                TexturePackStyle::Classic => classic::layer(index, name, resolution),
                TexturePackStyle::Painterly => painterly::layer(index, name, resolution),
                TexturePackStyle::Lush => lush::layer(index, name, resolution),
                TexturePackStyle::Minimal => minimal::layer(index, name, resolution),
            };
            debug_assert_eq!(canvas.size, resolution);
            canvas
        })
        .collect()
}

/// Deterministic per-layer seed folding the style and catalogue index together.
fn layer_seed(style: TexturePackStyle, index: u32) -> u32 {
    style.seed() ^ index.wrapping_mul(0x9e37_79b9) ^ 0x85eb_ca6b
}

/// Small deterministic 32-bit avalanche hash (no seed state, no floats).
const fn hash32(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    value
}

/// Hashes one texel coordinate for a layer seed.
fn hash_noise(seed: u32, x: u32, y: u32) -> u32 {
    hash32(seed ^ x.wrapping_mul(0x9e37_79b1) ^ hash32(y.wrapping_mul(0x85eb_ca6b)))
}

/// Smoothstep in 0..=256 fixed point.
fn smooth_fixed(value: u32) -> u32 {
    (value * value * (768 - 2 * value)) >> 16
}

/// Interpolates two channels with `t` in 0..=256.
fn lerp_u8(a: u8, b: u8, t: u32) -> u8 {
    let t = t.min(256);
    (((u32::from(a) * (256 - t)) + (u32::from(b) * t)) >> 8) as u8
}

/// Blends two RGB colours with `t` in 0..=256.
fn lerp_color(a: [u8; 3], b: [u8; 3], t: u32) -> [u8; 3] {
    [lerp_u8(a[0], b[0], t), lerp_u8(a[1], b[1], t), lerp_u8(a[2], b[2], t)]
}

/// Three-stop colour ramp for `t` in 0..=255.
fn ramp3(a: [u8; 3], b: [u8; 3], c: [u8; 3], t: u32) -> [u8; 3] {
    let t = t.min(255);
    if t < 128 {
        lerp_color(a, b, t * 2)
    } else {
        lerp_color(b, c, (t - 128) * 2)
    }
}

/// Adds a signed delta to every channel.
fn offset(color: [u8; 3], delta: i32) -> [u8; 3] {
    [
        clamp_channel(i32::from(color[0]) + delta),
        clamp_channel(i32::from(color[1]) + delta),
        clamp_channel(i32::from(color[2]) + delta),
    ]
}

/// Clamps one colour channel to `0..=255`.
fn clamp_channel(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// 4×4 ordered Bayer matrix.
const BAYER4: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Ordered-dither offset in `-amount..=amount`.
fn dither(x: u32, y: u32, amount: i32) -> i32 {
    let threshold = BAYER4[(y & 3) as usize][(x & 3) as usize] - 8;
    threshold * amount / 8
}

/// Deterministic smoothed value noise with a wrapping lattice.
///
/// `cells` must divide `size`; the caller owns that invariant. The result is in
/// `0..=255` and tiles seamlessly across the layer edge.
fn periodic_noise(seed: u32, x: u32, y: u32, size: u32, cells: u32) -> u8 {
    debug_assert!(
        cells > 0 && size.is_multiple_of(cells),
        "noise cells must divide the layer"
    );
    let cell = (size / cells).max(1);
    let x0 = x / cell;
    let y0 = y / cell;
    let fx = smooth_fixed((x % cell) * 256 / cell);
    let fy = smooth_fixed((y % cell) * 256 / cell);
    let sample = |cx: u32, cy: u32| (hash_noise(seed, cx % cells, cy % cells) & 0xff) as u8;
    let top = lerp_u8(sample(x0, y0), sample(x0 + 1, y0), fx);
    let bottom = lerp_u8(sample(x0, y0 + 1), sample(x0 + 1, y0 + 1), fx);
    lerp_u8(top, bottom, fy)
}

/// Fractal value noise in `0..=255`, three octaves by default at the call site.
fn fbm(seed: u32, x: u32, y: u32, size: u32, base_cells: u32, octaves: u32) -> u8 {
    let mut sum = 0u32;
    let mut total = 0u32;
    let mut cells = base_cells.max(1);
    let mut weight = 8u32;
    for octave in 0..octaves {
        if cells > size {
            break;
        }
        let octave_seed = seed.wrapping_add(octave.wrapping_mul(0x9e37_79b9));
        sum += u32::from(periodic_noise(octave_seed, x, y, size, cells)) * weight;
        total += weight;
        weight = (weight / 2).max(1);
        cells = cells.saturating_mul(2);
    }
    (sum / total.max(1)) as u8
}

/// Integer square root (floor) without floating point.
fn isqrt(value: u32) -> u32 {
    let mut remainder = value;
    let mut result = 0u32;
    let mut bit = 1u32 << 30;
    while bit > remainder {
        bit >>= 2;
    }
    while bit != 0 {
        if remainder >= result + bit {
            remainder -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result
}

/// Triangle wave in `0..=256` over `period` steps, peaking mid-period.
fn wave01(phase: u32, period: u32) -> u32 {
    let period = period.max(2);
    let p = phase % period;
    let half = period / 2;
    let peak = half.saturating_sub(1).max(1);
    let distance = if p < half { p } else { period - 1 - p };
    (distance * 256 / peak).min(256)
}

/// A tiny mutable RGBA canvas with wrapping coordinates.
///
/// Colour and cut-out alpha are kept in parallel arrays. Painting colour never
/// touches alpha, so decorative details cannot accidentally reopen a leaf
/// hole; [`Canvas::set_alpha`] is the explicit way to punch one.
struct Canvas {
    /// Edge length in pixels.
    size: u32,
    /// Row-major RGB pixels.
    pixels: Vec<[u8; 3]>,
    /// Row-major alpha cut-out mask (0 or 255 at authoring time).
    alpha: Vec<u8>,
}

impl Canvas {
    /// A fully opaque canvas filled with one colour.
    fn new(size: u32, color: [u8; 3]) -> Self {
        let texels = (size * size) as usize;
        Self {
            size,
            pixels: vec![color; texels],
            alpha: vec![255; texels],
        }
    }

    /// A fully opaque canvas filled texel by texel.
    fn from_fn(size: u32, mut color: impl FnMut(u32, u32) -> [u8; 3]) -> Self {
        let mut pixels = Vec::with_capacity((size * size) as usize);
        for y in 0..size {
            for x in 0..size {
                pixels.push(color(x, y));
            }
        }
        Self {
            size,
            alpha: vec![255; (size * size) as usize],
            pixels,
        }
    }

    /// Pixel at wrapped coordinates.
    fn get(&self, x: i32, y: i32) -> [u8; 3] {
        let edge = self.size as i32;
        let x = x.rem_euclid(edge) as usize;
        let y = y.rem_euclid(edge) as usize;
        self.pixels[y * self.size as usize + x]
    }

    /// Replaces the pixel at wrapped coordinates without changing its alpha.
    fn set(&mut self, x: i32, y: i32, color: [u8; 3]) {
        let edge = self.size as i32;
        let x = x.rem_euclid(edge) as usize;
        let y = y.rem_euclid(edge) as usize;
        self.pixels[y * self.size as usize + x] = color;
    }

    /// Blends `color` over the pixel with `alpha` in `0..=256`.
    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u32) {
        let alpha = alpha.min(256);
        if alpha == 256 {
            self.set(x, y, color);
            return;
        }
        if alpha == 0 {
            return;
        }
        let base = self.get(x, y);
        let inverse = 256 - alpha;
        self.set(
            x,
            y,
            [
                ((u32::from(base[0]) * inverse + u32::from(color[0]) * alpha) >> 8) as u8,
                ((u32::from(base[1]) * inverse + u32::from(color[1]) * alpha) >> 8) as u8,
                ((u32::from(base[2]) * inverse + u32::from(color[2]) * alpha) >> 8) as u8,
            ],
        );
    }

    /// Alpha at wrapped coordinates.
    fn alpha_at(&self, x: i32, y: i32) -> u8 {
        let edge = self.size as i32;
        let x = x.rem_euclid(edge) as usize;
        let y = y.rem_euclid(edge) as usize;
        self.alpha[y * self.size as usize + x]
    }

    /// Replaces the alpha at wrapped coordinates.
    fn set_alpha(&mut self, x: i32, y: i32, alpha: u8) {
        let edge = self.size as i32;
        let x = x.rem_euclid(edge) as usize;
        let y = y.rem_euclid(edge) as usize;
        self.alpha[y * self.size as usize + x] = alpha;
    }
}

/// Draws a one-pixel grass blade leaning by `lean` at its tip.
fn paint_blade(canvas: &mut Canvas, x: i32, base_y: i32, height: i32, color: [u8; 3], lean: i32) {
    for step in 0..height.max(1) {
        let px = if step == height - 1 { x + lean } else { x };
        canvas.set(px, base_y - step, color);
    }
}

/// Draws a hard or soft-edged filled ellipse.
fn paint_ellipse(canvas: &mut Canvas, cx: i32, cy: i32, rx: i32, ry: i32, color: [u8; 3], soft: i32) {
    let rx = rx.max(1);
    let ry = ry.max(1);
    let soft = soft.max(0);
    let edge = (rx * rx * ry * ry) as u32;
    let feather = edge / 3;
    for dy in -(ry + soft)..=(ry + soft) {
        for dx in -(rx + soft)..=(rx + soft) {
            let scaled = (dx * dx * ry * ry + dy * dy * rx * rx) as u32;
            if scaled > edge {
                continue;
            }
            if soft > 0 && scaled > edge.saturating_sub(feather) {
                let closeness = (edge - scaled) * 160 / feather.max(1);
                canvas.blend(cx + dx, cy + dy, color, 96 + closeness);
            } else {
                canvas.set(cx + dx, cy + dy, color);
            }
        }
    }
}

/// Draws a thin crack polyline in a mostly consistent direction.
///
/// The line walks in `(step_x, step_y)` with occasional one-texel jogs to the
/// side, and gets a faint highlight on one flank. It is deliberately far too
/// short-lived to wander into a scribble.
#[allow(clippy::too_many_arguments)]
fn paint_crack(
    canvas: &mut Canvas,
    seed: u32,
    start_x: i32,
    start_y: i32,
    steps: u32,
    step_x: i32,
    step_y: i32,
    depth: [u8; 3],
    highlight: [u8; 3],
) {
    let (dx, dy) = (step_x.signum(), step_y.signum());
    let (side_x, side_y) = (dy, -dx);
    let (mut x, mut y) = (start_x, start_y);
    for step in 0..steps {
        canvas.set(x, y, depth);
        if hash_noise(seed, step, 9).is_multiple_of(3) {
            canvas.blend(x + side_x, y + side_y, highlight, 52);
        }
        x += dx;
        y += dy;
        if hash_noise(seed, step, 3).is_multiple_of(6) {
            x += side_x;
            y += side_y;
        }
    }
}

/// One sample of a wrapping, jittered cellular lattice.
struct CellSample {
    /// Squared distance in texels to the nearest feature point.
    nearest: u32,
    /// Squared distance to the second-nearest feature point.
    second: u32,
}

/// Samples the nearest two feature points of a wrapping jittered lattice.
///
/// The nearest/second distance pair gives crisp angular cell boundaries, which
/// read as the angular cracks in bedrock.
fn cellular(seed: u32, x: u32, y: u32, size: u32, cells: u32) -> CellSample {
    let cells = cells.max(1);
    let cell = (size / cells).max(1);
    let grid_x = (x / cell) as i32;
    let grid_y = (y / cell) as i32;
    let mut nearest = u32::MAX;
    let mut second = u32::MAX;
    for ny in -1..=1 {
        for nx in -1..=1 {
            let cell_x = grid_x + nx;
            let cell_y = grid_y + ny;
            let wrapped_x = cell_x.rem_euclid(cells as i32) as u32;
            let wrapped_y = cell_y.rem_euclid(cells as i32) as u32;
            let hash = hash_noise(seed, wrapped_x, wrapped_y);
            let point_x = wrapped_x as i32 * cell as i32 + (hash % cell) as i32;
            let point_y =
                wrapped_y as i32 * cell as i32 + (hash_noise(seed ^ 0x9e37, wrapped_x, wrapped_y) % cell) as i32;
            // Use the periodic image of the point closest to the sample.
            let half = (size / 2) as i32;
            let point_x = if point_x - x as i32 > half {
                point_x - size as i32
            } else if x as i32 - point_x > half {
                point_x + size as i32
            } else {
                point_x
            };
            let point_y = if point_y - y as i32 > half {
                point_y - size as i32
            } else if y as i32 - point_y > half {
                point_y + size as i32
            } else {
                point_y
            };
            let dx = point_x - x as i32;
            let dy = point_y - y as i32;
            let distance = (dx * dx + dy * dy) as u32;
            if distance < nearest {
                second = nearest;
                nearest = distance;
            } else if distance < second {
                second = distance;
            }
        }
    }
    CellSample { nearest, second }
}

/// A rounded pebble placed by [`scatter_pebbles`].
struct Pebble {
    /// Centre in wrapped texel coordinates.
    x: i32,
    /// Centre in wrapped texel coordinates.
    y: i32,
    /// Radius in texels (at least one).
    radius: i32,
    /// Hash of the candidate, used to pick a colour.
    id: u32,
}

/// Scatters non-overlapping rounded pebbles across a wrapping square.
///
/// Candidate centres come from the integer hash, so the arrangement is
/// irregular and has no lattice axes; a candidate is kept only when it does not
/// collide with an already placed pebble. Drawing through the wrapping canvas
/// then makes the edge pebbles tile seamlessly.
fn scatter_pebbles(seed: u32, size: u32, attempts: u32, min_radius: i32, max_radius: i32) -> Vec<Pebble> {
    let edge = size as i32;
    let radius_span = (max_radius - min_radius + 1).max(1) as u32;
    let mut pebbles: Vec<Pebble> = Vec::new();
    for i in 0..attempts {
        let hash = hash_noise(seed ^ 0x5eed, i, 0);
        let radius = min_radius + ((hash >> 8) % radius_span) as i32;
        let x = (hash % size) as i32;
        let y = (hash_noise(seed ^ 0x5eed, i, 1) % size) as i32;
        let fits = pebbles.iter().all(|other| {
            let dx = (x - other.x).abs();
            let dx = dx.min(edge - dx);
            let dy = (y - other.y).abs();
            let dy = dy.min(edge - dy);
            let reach = radius + other.radius;
            dx * dx + dy * dy >= reach * reach
        });
        if fits {
            pebbles.push(Pebble { x, y, radius, id: hash });
        }
    }
    pebbles
}

/// Paints one shaded rounded pebble over a canvas.
fn paint_gravel_pebble(
    canvas: &mut Canvas,
    pebble: &Pebble,
    color: [u8; 3],
    contrast: i32,
    rim: i32,
    dither_amount: i32,
) {
    let radius = pebble.radius.max(1);
    let diameter = radius * 2 + 1;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let distance = dx * dx + dy * dy;
            if distance > radius * radius {
                continue;
            }
            let x = pebble.x + dx;
            let y = pebble.y + dy;
            let light = (-(dx + dy) * contrast / diameter).clamp(-contrast, contrast);
            let edge = if distance > (radius - 1) * (radius - 1) { rim } else { 0 };
            let edge = if dither_amount == 0 {
                edge
            } else {
                let wrap = canvas.size as i32;
                edge + dither(x.rem_euclid(wrap) as u32, y.rem_euclid(wrap) as u32, dither_amount)
            };
            canvas.set(x, y, offset(color, light + edge));
        }
    }
}

/// Paints a recessed 3×3 crafting grid onto a wooden canvas.
///
/// A raised frame surrounds the nine cells; two dividers cross the surface in
/// each direction, each with a single lit lip on its top and left. A tiny
/// centre mark and four corner studs make the layer read as a crafting table
/// rather than as a plain plank field.
#[allow(clippy::too_many_arguments)]
fn paint_crafting_grid(
    canvas: &mut Canvas,
    frame: [u8; 3],
    divider: [u8; 3],
    cell: [u8; 3],
    highlight: [u8; 3],
    stud: [u8; 3],
    inset: i32,
    thickness: i32,
) {
    let size = canvas.size as i32;
    let inset = inset.clamp(1, size / 4);
    let thickness = thickness.clamp(1, size / 8);
    let first = size / 3;
    let second = 2 * size / 3;

    // Recess the nine fields a little while keeping the underlying wood grain.
    for y in inset..size - inset {
        for x in inset..size - inset {
            canvas.blend(x, y, cell, 64);
        }
    }

    // Raised outer frame.
    for i in 0..size {
        for t in 0..inset {
            canvas.set(i, t, frame);
            canvas.set(i, size - 1 - t, frame);
            canvas.set(t, i, frame);
            canvas.set(size - 1 - t, i, frame);
        }
    }

    // The two dividers in each direction produce the 3×3 cell layout.
    for i in inset..size - inset {
        for t in 0..thickness {
            canvas.set(first + t, i, divider);
            canvas.set(second + t, i, divider);
            canvas.set(i, first + t, divider);
            canvas.set(i, second + t, divider);
        }
    }

    // One lit lip on the top and left of every divider.
    for i in inset..size - inset {
        canvas.blend(first - 1, i, highlight, 150);
        canvas.blend(second - 1, i, highlight, 150);
        canvas.blend(i, first - 1, highlight, 150);
        canvas.blend(i, second - 1, highlight, 150);
    }

    // Four corner studs and a small centre crafting mark.
    for (x, y) in [
        (inset, inset),
        (size - 1 - inset, inset),
        (inset, size - 1 - inset),
        (size - 1 - inset, size - 1 - inset),
    ] {
        canvas.set(x, y, stud);
    }
    let centre = size / 2;
    for (x, y) in [
        (centre, centre),
        (centre - 1, centre),
        (centre + 1, centre),
        (centre, centre - 1),
        (centre, centre + 1),
    ] {
        canvas.blend(x, y, divider, 110);
    }
    canvas.set(centre, centre, highlight);
}

/// Paints a small hand tool (diagonal handle with a metal head) onto a table side.
#[allow(clippy::too_many_arguments)]
fn paint_table_tool(
    canvas: &mut Canvas,
    x: i32,
    y: i32,
    metal: [u8; 3],
    metal_dark: [u8; 3],
    handle: [u8; 3],
    highlight: [u8; 3],
    scale: i32,
) {
    let scale = scale.max(1);
    // Two-texel-thick diagonal handle.
    for step in 0..(3 * scale) {
        canvas.set(x + step, y + step, handle);
        canvas.set(x + step, y + step + 1, offset(handle, -18));
    }
    // Metal head across the top of the handle, with a darker back edge.
    for dy in -scale..=scale {
        canvas.set(x - scale, y + dy, metal);
        canvas.set(x - scale - 1, y + dy, metal_dark);
    }
    canvas.set(x - scale - 1, y - scale, highlight);
    // Binding where the head meets the handle.
    canvas.set(x, y, metal_dark);
}

/// Paints a field of rounded cobbles separated by dark mortar.
///
/// The mortar stays visible wherever [`scatter_pebbles`] did not place a
/// pebble, so the dark gaps are as irregular as the stone shapes themselves.
#[allow(clippy::too_many_arguments)]
fn paint_cobblestone(
    size: u32,
    seed: u32,
    mortar: [u8; 3],
    palette: &[[u8; 3]],
    attempts: u32,
    min_radius: i32,
    max_radius: i32,
    contrast: i32,
    rim: i32,
    dither_amount: i32,
) -> Canvas {
    let mut canvas = Canvas::new(size, mortar);
    for pebble in scatter_pebbles(seed, size, attempts, min_radius, max_radius) {
        let color = palette[(pebble.id >> 8) as usize % palette.len()];
        paint_gravel_pebble(&mut canvas, &pebble, color, contrast, rim, dither_amount);
    }
    canvas
}

/// Punches clustered, irregular cut-out holes covering roughly `percent` of the tile.
///
/// Scores come from a coarse noise field plus a little texel-scale jitter, so the
/// holes form a few organic patches rather than even per-pixel salt. Lone
/// one-texel holes are closed again, which keeps the silhouette readable.
fn punch_leaf_holes(canvas: &mut Canvas, seed: u32, percent: u32) {
    let size = canvas.size;
    let total = (size * size) as usize;
    let target = (total * percent as usize / 100).clamp(1, total);
    let mut ranked: Vec<(u32, u32)> = Vec::with_capacity(total);
    for y in 0..size {
        for x in 0..size {
            let coarse = u32::from(fbm(seed ^ 0x31, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x57, x, y, size, 4));
            let grain = hash_noise(seed ^ 0x9b, x, y) & 0x0f;
            ranked.push((coarse * 8 + mid * 2 + grain, y * size + x));
        }
    }
    // The index breaks ties at the cut-off, keeping the hole set identical on
    // every platform even though the sort itself is unstable.
    ranked.sort_unstable_by_key(|entry| (entry.0, entry.1));
    for (_, index) in ranked.iter().take(target) {
        canvas.set_alpha((index % size) as i32, (index / size) as i32, 0);
    }
    let holes: Vec<(u32, u32)> = (0..size)
        .flat_map(|y| (0..size).map(move |x| (x, y)))
        .filter(|&(x, y)| canvas.alpha_at(x as i32, y as i32) == 0)
        .collect();
    for (x, y) in holes {
        let mut neighbours = 0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                if (dx != 0 || dy != 0) && canvas.alpha_at(x as i32 + dx, y as i32 + dy) == 0 {
                    neighbours += 1;
                }
            }
        }
        if neighbours == 0 {
            canvas.set_alpha(x as i32, y as i32, 255);
        }
    }
}

/// Fills an inclusive wrapped rectangle.
fn fill_rect(canvas: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 3]) {
    for y in y0..=y1 {
        for x in x0..=x1 {
            canvas.set(x, y, color);
        }
    }
}

/// Palette and chunk size of an ore overlay painted on a style's stone base.
struct OreLook {
    /// Shadowed rim and dark mineral body.
    dark: [u8; 3],
    /// Main mineral colour.
    mid: [u8; 3],
    /// Lit metallic facet.
    light: [u8; 3],
    /// Secondary mineral colour (the teal patina on copper ore).
    accent: Option<[u8; 3]>,
    /// Number of large mineral chunks per tile.
    clusters: u32,
    /// Base chunk radius in texels.
    radius: i32,
    /// Soft-edge feather in texels; 0 keeps hard pixel-art edges.
    soft: i32,
}

/// Paints rounded mineral chunks and loose flecks over an existing stone canvas.
fn paint_ore(canvas: &mut Canvas, seed: u32, look: &OreLook) {
    let size = canvas.size;
    let span = size.saturating_sub(2).max(1);
    for cluster in 0..look.clusters {
        let hash = hash_noise(seed ^ 0x0f, cluster, 1);
        let cx = 1 + (hash % span) as i32;
        let cy = 1 + (hash_noise(seed ^ 0x0f, cluster, 2) % span) as i32;
        let radius = look.radius + (hash_noise(seed ^ 0x0f, cluster, 3) % 2) as i32;
        // A dark rim below and to the right grounds the chunk in the stone.
        paint_ellipse(canvas, cx + 1, cy + 1, radius + 1, radius, look.dark, look.soft);
        let body = if hash & 0x100 == 0 { look.mid } else { look.dark };
        paint_ellipse(canvas, cx, cy, radius, radius, body, look.soft);
        // A small lit facet reads as a metallic glint.
        paint_ellipse(
            canvas,
            cx - 1,
            cy - 1,
            (radius / 2).max(1),
            (radius / 2).max(1),
            look.light,
            look.soft,
        );
        if let Some(accent) = look.accent {
            // The secondary mineral creeps out along the lower edge.
            let half = (radius * 2 + 1).max(1) as u32;
            for speck in 0..3u32 {
                let dx = (hash_noise(seed ^ 0x2c, cluster, speck) % half) as i32 - radius;
                let dy = radius + (hash_noise(seed ^ 0x2d, cluster, speck) % 2) as i32;
                canvas.set(cx + dx, cy + dy, accent);
            }
        }
        // One or two loose flecks around each chunk.
        for fleck in 0..2u32 {
            let dx = (hash_noise(seed ^ 0x3e, cluster, fleck) % 5) as i32 - 2;
            let dy = (hash_noise(seed ^ 0x3f, cluster, fleck) % 5) as i32 - 2;
            canvas.set(cx + dx, cy + dy + radius, look.light);
        }
    }
    // Isolated single-texel flecks keep the host stone from reading as plain rock.
    for fleck in 0..look.clusters * 3 {
        let x = (hash_noise(seed ^ 0x5a, fleck, 0) % size) as i32;
        let y = (hash_noise(seed ^ 0x5b, fleck, 0) % size) as i32;
        let color = if fleck & 1 == 0 { look.mid } else { look.dark };
        canvas.set(x, y, color);
    }
}

/// Colours shared by the three furnace faces, supplied per texture style.
struct FurnaceLook {
    /// Slab edge in shadow (bottom and right).
    edge_dark: [u8; 3],
    /// Slab edge catching light (top and left).
    edge_light: [u8; 3],
    /// Recessed masonry seam.
    seam: [u8; 3],
    /// Clean iron vent and rivets.
    metal: [u8; 3],
    /// Shaded iron.
    metal_dark: [u8; 3],
    /// Dark firebox opening.
    opening: [u8; 3],
    /// Deepest part of the firebox.
    opening_inner: [u8; 3],
    /// Smouldering ember shadow.
    ember_dark: [u8; 3],
    /// Ember body.
    ember: [u8; 3],
    /// Hottest ember highlight.
    ember_hot: [u8; 3],
    /// Border thickness in texels.
    trim: i32,
    /// Soft arch feather in texels; 0 keeps a crisp pixel-art opening.
    soft: i32,
    /// Per-texel dither amount; 0 for crisp styles.
    dither: i32,
    /// Whether the style draws raised corner rivets.
    rivets: bool,
}

/// Paints the light/shadow border shared by the three furnace faces.
fn paint_furnace_border(canvas: &mut Canvas, look: &FurnaceLook) {
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    for y in 0..size {
        for x in 0..size {
            if x >= trim && y >= trim && x < size - trim && y < size - trim {
                continue;
            }
            let color = if x < trim || y < trim {
                look.edge_light
            } else {
                look.edge_dark
            };
            let color = if look.dither == 0 {
                color
            } else {
                offset(color, dither(x as u32, y as u32, look.dither))
            };
            canvas.set(x, y, color);
        }
    }
}

/// Paints four raised metal rivets just inside the furnace border.
fn paint_furnace_rivets(canvas: &mut Canvas, look: &FurnaceLook) {
    if !look.rivets {
        return;
    }
    let size = canvas.size as i32;
    let inset = look.trim.max(1) + 1;
    for (x, y) in [
        (inset, inset),
        (size - inset - 1, inset),
        (inset, size - inset - 1),
        (size - inset - 1, size - inset - 1),
    ] {
        canvas.set(x, y, look.metal);
        canvas.set(x + 1, y + 1, look.metal_dark);
    }
}

/// Paints a furnace top: a bordered stone slab with a recessed iron vent.
fn furnace_top_from(mut canvas: Canvas, seed: u32, look: &FurnaceLook) -> Canvas {
    paint_furnace_border(&mut canvas, look);
    let size = canvas.size as i32;
    let slot_w = (size * 10 / 16).max(6);
    let slot_h = (size * 3 / 16).max(2);
    let x0 = (size - slot_w) / 2;
    let y0 = (size - slot_h) / 2;
    // Lit upper lip and shadowed lower lip make the vent read as recessed.
    for x in x0..x0 + slot_w {
        canvas.set(x, y0 - 1, look.edge_light);
        canvas.set(x, y0 + slot_h, look.edge_dark);
    }
    fill_rect(&mut canvas, x0, y0, x0 + slot_w - 1, y0 + slot_h - 1, look.opening);
    // Iron grate bars split the vent into readable slots.
    let bars = if size >= 32 { 4 } else { 2 };
    for bar in 1..=bars {
        let bar_x = x0 + slot_w * bar / (bars + 1);
        for y in y0..y0 + slot_h {
            canvas.set(bar_x, y, look.metal_dark);
            canvas.set(bar_x + 1, y, look.metal);
        }
    }
    for x in x0..x0 + slot_w {
        canvas.blend(x, y0 + slot_h - 1, look.metal_dark, 120);
    }
    // One worn spot on the vent lip varies with the layer seed.
    let wear = x0 + (hash_noise(seed ^ 0x5d, 0, 0) % slot_w.max(1) as u32) as i32;
    canvas.set(wear, y0, look.edge_light);
    paint_furnace_rivets(&mut canvas, look);
    canvas
}

/// Paints a furnace side: riveted stone courses with a chiselled panel frame.
fn furnace_side_from(mut canvas: Canvas, _seed: u32, look: &FurnaceLook) -> Canvas {
    paint_furnace_border(&mut canvas, look);
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    let inner = size - trim;
    let course_height = ((size - 2 * trim) / 2).max(3);
    // Two masonry courses with staggered vertical joints.
    let y = trim + course_height;
    for x in trim..inner {
        canvas.set(x, y, look.seam);
        canvas.blend(x, y + 1, look.edge_light, 90);
    }
    for course in 0..2 {
        let top = trim + course * course_height;
        let bottom = (top + course_height).min(inner);
        let joint_x = if course == 0 { size / 3 } else { size * 2 / 3 };
        for joint_y in top + 1..bottom {
            canvas.set(joint_x, joint_y, look.seam);
        }
        let short_x = if course == 0 { size * 2 / 3 } else { size / 3 };
        for joint_y in top + 1..(top + course_height / 2).min(bottom) {
            canvas.set(short_x, joint_y, look.seam);
        }
    }
    // Chiselled panel trim just inside the riveted frame.
    for x in trim + 1..inner - 1 {
        canvas.blend(x, trim + 1, look.edge_light, 60);
        canvas.blend(x, inner - 2, look.edge_dark, 80);
    }
    paint_furnace_rivets(&mut canvas, look);
    canvas
}

/// Paints a furnace front: a stone face with an arched firebox and ember glow.
fn furnace_front_from(mut canvas: Canvas, seed: u32, look: &FurnaceLook) -> Canvas {
    paint_furnace_border(&mut canvas, look);
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    let cx = size / 2;
    let half = ((size * 5) / 16).max(3);
    let spring = (size * 9) / 16;
    let bottom = size - trim - 2;
    let arch_ry = (half * 3 / 4).max(2);
    let left = cx - half;
    let right = cx + half - 1;
    // A masonry joint above the arch keeps the face from reading as a hole.
    for x in trim..size - trim {
        canvas.blend(x, spring - arch_ry - 2, look.seam, 110);
    }
    // Rectangular firebox mouth with a rounded arch on top.
    fill_rect(&mut canvas, left, spring, right, bottom, look.opening);
    paint_ellipse(&mut canvas, cx, spring, half, arch_ry, look.opening, look.soft);
    paint_ellipse(
        &mut canvas,
        cx,
        spring - arch_ry / 2,
        (half - 1).max(1),
        (arch_ry / 2).max(1),
        look.opening_inner,
        look.soft,
    );
    // Embers banked against the firebox floor; purely visual, no light source.
    for x in left + 1..right {
        let hash = hash_noise(seed ^ 0x71, (x - left) as u32, 1);
        canvas.blend(x, bottom - 2, look.ember_dark, 60 + (hash % 70));
        canvas.blend(x, bottom - 1, look.ember_dark, 110 + (hash % 100));
        canvas.set(
            x,
            bottom,
            match hash % 5 {
                0 => look.ember_hot,
                1 | 2 => look.ember,
                _ => look.ember_dark,
            },
        );
        if hash.is_multiple_of(7) {
            canvas.set(x, bottom - 1, look.ember);
        }
        if hash.is_multiple_of(19) {
            canvas.set(x, bottom - 2, look.ember_hot);
        }
    }
    // One iron fire bar crosses the firebox mouth.
    for x in left..=right {
        canvas.set(x, bottom - 3, look.metal_dark);
    }
    for x in (left + 2..right).step_by(4) {
        canvas.set(x, bottom - 3, look.metal);
    }
    // A lit stone lip catches the ember glow above the arch.
    for x in left..=right {
        canvas.blend(x, spring - arch_ry - 1, look.edge_light, 80);
    }
    paint_furnace_rivets(&mut canvas, look);
    canvas
}

/// Colours shared by the three chest faces, supplied per texture style.
struct ChestLook {
    /// Dark seam between lid and body.
    seam: [u8; 3],
    /// Lit plank edge below the seam.
    seam_light: [u8; 3],
    /// Clean iron band.
    metal: [u8; 3],
    /// Shaded iron band.
    metal_dark: [u8; 3],
    /// Lit iron edge.
    metal_light: [u8; 3],
    /// Keyhole shadow.
    keyhole: [u8; 3],
    /// Border thickness in texels.
    trim: i32,
    /// Per-texel dither amount; 0 for crisp styles.
    dither: i32,
}

/// Width of a chest's iron bands in texels.
fn chest_band_width(size: i32) -> i32 {
    (size / 10).max(2)
}

/// Paints one vertical iron strap with a lit inner edge and rivets.
fn paint_chest_band(canvas: &mut Canvas, x0: i32, x1: i32, top: i32, bottom: i32, look: &ChestLook) {
    fill_rect(canvas, x0, top, x1, bottom, look.metal_dark);
    fill_rect(canvas, x0, top, x1 - 1, bottom, look.metal);
    for y in top..=bottom {
        let color = if look.dither == 0 {
            look.metal_light
        } else {
            offset(look.metal_light, dither(x0 as u32, y as u32, look.dither))
        };
        canvas.set(x0, y, color);
    }
    // Rivets march down the middle of the strap.
    let step = ((bottom - top) / 4).max(3);
    let rivet_x = (x0 + x1) / 2;
    let mut y = top + step / 2;
    while y <= bottom {
        canvas.set(rivet_x, y, look.metal_light);
        canvas.set(rivet_x, y + 1, look.metal_dark);
        y += step;
    }
}

/// Paints a chest top: planks, a lid seam and iron straps running across it.
fn chest_top_from(mut canvas: Canvas, _seed: u32, look: &ChestLook) -> Canvas {
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    let band = chest_band_width(size);
    let seam_y = (size * 5 / 16).max(3);
    for x in 0..size {
        canvas.set(x, seam_y, look.seam);
        if seam_y + 1 < size {
            canvas.set(x, seam_y + 1, look.seam_light);
        }
    }
    for left in [trim - 1, size - trim - band + 1] {
        paint_chest_band(&mut canvas, left, left + band - 1, 0, size - 1, look);
    }
    for x in 0..size {
        canvas.blend(x, 0, look.seam, 140);
        canvas.blend(x, size - 1, look.seam, 140);
    }
    for y in 0..size {
        canvas.blend(0, y, look.seam, 140);
        canvas.blend(size - 1, y, look.seam, 140);
    }
    canvas
}

/// Paints a chest side: planks with a lid lip and full-height corner straps.
fn chest_side_from(mut canvas: Canvas, _seed: u32, look: &ChestLook) -> Canvas {
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    let band = chest_band_width(size);
    // Lid lip at the top and a plain trim board at the bottom.
    for x in 0..size {
        canvas.set(x, trim - 1, look.seam);
        canvas.set(x, trim, look.seam_light);
        canvas.set(x, size - trim, look.seam);
    }
    for y in trim..size - trim {
        canvas.set(trim - 1, y, look.seam);
        canvas.set(size - trim, y, look.seam);
    }
    for left in [trim, size - trim - band] {
        paint_chest_band(&mut canvas, left, left + band - 1, 0, size - 1, look);
    }
    canvas
}

/// Paints a chest front: lid seam, corner straps, centre latch and keyhole.
fn chest_front_from(mut canvas: Canvas, _seed: u32, look: &ChestLook) -> Canvas {
    let size = canvas.size as i32;
    let trim = look.trim.max(1);
    let band = chest_band_width(size);
    let seam_y = (size * 5 / 16).max(3);
    for x in 0..size {
        canvas.set(x, seam_y, look.seam);
        if seam_y + 1 < size {
            canvas.set(x, seam_y + 1, look.seam_light);
        }
    }
    for left in [trim, size - trim - band] {
        paint_chest_band(&mut canvas, left, left + band - 1, 0, size - 1, look);
    }
    // A metal latch plate straddles the lid seam.
    let latch_w = (size / 5).max(3);
    let latch_h = (size / 4).max(4);
    let lx = size / 2 - latch_w / 2;
    let ly = seam_y - latch_h / 2;
    fill_rect(&mut canvas, lx, ly, lx + latch_w - 1, ly + latch_h - 1, look.metal_dark);
    fill_rect(
        &mut canvas,
        lx + 1,
        ly + 1,
        lx + latch_w - 2,
        ly + latch_h - 2,
        look.metal,
    );
    for x in lx + 1..lx + latch_w - 1 {
        canvas.set(x, ly + 1, look.metal_light);
        canvas.set(x, ly + latch_h - 2, look.metal_dark);
    }
    // Keyhole below the seam, with a shadowed escutcheon.
    let key_x = size / 2;
    canvas.set(key_x, seam_y + 1, look.keyhole);
    canvas.set(key_x, seam_y + 2, look.keyhole);
    canvas.set(key_x - 1, seam_y + 2, look.metal_dark);
    canvas.set(key_x + 1, seam_y + 2, look.metal_dark);
    // One rivet on each latch cheek.
    canvas.set(lx + 1, ly + latch_h / 2, look.metal_light);
    canvas.set(lx + latch_w - 2, ly + latch_h / 2, look.metal_light);
    canvas
}

/// Classic 16² pixel-art pack: close to the genre default, saturated but soft.
mod classic {
    use super::*;

    const GRASS_DARK: [u8; 3] = [99, 160, 73];
    const GRASS: [u8; 3] = [106, 171, 79];
    const GRASS_LIGHT: [u8; 3] = [113, 178, 86];
    const GRASS_TUFT: [u8; 3] = [86, 143, 62];
    const FLOWER_GOLD: [u8; 3] = [188, 184, 100];

    const DIRT_DARK: [u8; 3] = [112, 78, 50];
    const DIRT: [u8; 3] = [138, 98, 64];
    const DIRT_LIGHT: [u8; 3] = [156, 115, 80];
    const DIRT_PEBBLE: [u8; 3] = [158, 124, 92];
    const DIRT_PEBBLE_LIGHT: [u8; 3] = [147, 112, 82];
    const DIRT_SHADOW: [u8; 3] = [87, 59, 38];
    const ROOT: [u8; 3] = [70, 48, 30];

    const STONE_SHADOW: [u8; 3] = [128, 128, 134];
    const STONE: [u8; 3] = [141, 141, 145];
    const STONE_LIGHT: [u8; 3] = [156, 156, 160];
    const STONE_CRACK: [u8; 3] = [112, 112, 119];
    const STONE_HIGHLIGHT: [u8; 3] = [166, 166, 171];
    const STONE_SPEC: [u8; 3] = [120, 120, 127];

    const SAND_DARK: [u8; 3] = [203, 183, 128];
    const SAND: [u8; 3] = [221, 201, 143];
    const SAND_LIGHT: [u8; 3] = [236, 218, 166];
    const SAND_SPECK: [u8; 3] = [188, 166, 116];

    const WATER_DEEP: [u8; 3] = [45, 96, 176];
    const WATER: [u8; 3] = [63, 118, 200];
    const WATER_CREST: [u8; 3] = [96, 150, 220];
    const WATER_SPARKLE: [u8; 3] = [137, 186, 236];

    const BARK_DARK: [u8; 3] = [52, 35, 20];
    const BARK_GROOVE: [u8; 3] = [72, 48, 28];
    const BARK: [u8; 3] = [107, 76, 46];
    const BARK_LIGHT: [u8; 3] = [132, 96, 58];

    const RING_DARK: [u8; 3] = [118, 82, 48];
    const RING_LIGHT: [u8; 3] = [154, 113, 68];
    const RING_HEART: [u8; 3] = [94, 64, 38];

    const LEAF_SHADOW: [u8; 3] = [27, 63, 31];
    const LEAF_DARK: [u8; 3] = [40, 87, 38];
    const LEAF: [u8; 3] = [63, 122, 53];
    const LEAF_LIGHT: [u8; 3] = [84, 147, 63];
    const LEAF_PALE: [u8; 3] = [108, 170, 78];

    const BEDROCK_DARK: [u8; 3] = [28, 29, 35];
    const BEDROCK: [u8; 3] = [46, 46, 53];
    const BEDROCK_LIGHT: [u8; 3] = [68, 69, 77];

    const GRAVEL_GAP: [u8; 3] = [84, 80, 76];
    const GRAVEL_GREY: [u8; 3] = [126, 122, 117];
    const GRAVEL_BEIGE: [u8; 3] = [150, 130, 104];
    const GRAVEL_TAN: [u8; 3] = [136, 113, 88];
    const GRAVEL_PALE: [u8; 3] = [161, 155, 145];

    const CLAY_DARK: [u8; 3] = [139, 149, 165];
    const CLAY: [u8; 3] = [155, 164, 178];
    const CLAY_LIGHT: [u8; 3] = [170, 179, 192];

    const SNOW_SHADOW: [u8; 3] = [234, 242, 251];
    const SNOW: [u8; 3] = [246, 249, 252];
    const SNOW_HIGHLIGHT: [u8; 3] = [255, 255, 255];

    // v0.0.8 wood, cobblestone and crafting-table palettes.
    const PLANK_SHADOW: [u8; 3] = [101, 69, 39];
    const PLANK: [u8; 3] = [157, 112, 63];
    const PLANK_LIGHT: [u8; 3] = [183, 138, 87];
    const PLANK_GRAIN: [u8; 3] = [131, 91, 50];
    const PLANK_HIGHLIGHT: [u8; 3] = [199, 158, 106];

    const COBBLE_MORTAR: [u8; 3] = [88, 88, 94];
    const COBBLE_DARK: [u8; 3] = [111, 112, 118];
    const COBBLE: [u8; 3] = [142, 143, 148];
    const COBBLE_LIGHT: [u8; 3] = [166, 167, 172];
    const COBBLE_HIGHLIGHT: [u8; 3] = [181, 182, 186];

    const TABLE_FRAME: [u8; 3] = [93, 62, 34];
    const TABLE_DIVIDER: [u8; 3] = [82, 54, 29];
    const TABLE_CELL: [u8; 3] = [146, 101, 56];
    const TABLE_STUD: [u8; 3] = [209, 172, 120];
    const TOOL_METAL: [u8; 3] = [126, 131, 140];
    const TOOL_METAL_DARK: [u8; 3] = [93, 98, 107];

    // v1.0 ore, furnace and chest palettes.
    const COAL_LOOK: OreLook = OreLook {
        dark: [30, 30, 34],
        mid: [50, 50, 56],
        light: [78, 78, 84],
        accent: None,
        clusters: 4,
        radius: 2,
        soft: 0,
    };
    const IRON_LOOK: OreLook = OreLook {
        dark: [124, 93, 60],
        mid: [181, 145, 104],
        light: [217, 183, 140],
        accent: None,
        clusters: 4,
        radius: 2,
        soft: 0,
    };
    const COPPER_LOOK: OreLook = OreLook {
        dark: [116, 63, 39],
        mid: [190, 104, 64],
        light: [226, 145, 98],
        accent: Some([77, 140, 118]),
        clusters: 4,
        radius: 2,
        soft: 0,
    };
    const FURNACE_LOOK: FurnaceLook = FurnaceLook {
        edge_dark: [104, 104, 111],
        edge_light: [170, 170, 176],
        seam: [92, 92, 99],
        metal: [151, 154, 160],
        metal_dark: [95, 98, 104],
        opening: [36, 33, 32],
        opening_inner: [22, 21, 22],
        ember_dark: [137, 58, 25],
        ember: [224, 116, 36],
        ember_hot: [249, 191, 76],
        trim: 1,
        soft: 0,
        dither: 0,
        rivets: true,
    };
    const CHEST_LOOK: ChestLook = ChestLook {
        seam: [84, 56, 31],
        seam_light: [199, 158, 106],
        metal: [139, 142, 148],
        metal_dark: [92, 95, 101],
        metal_light: [190, 193, 198],
        keyhole: [30, 28, 27],
        trim: 1,
        dither: 0,
    };

    /// Generates one classic layer.
    pub(super) fn layer(index: u32, name: &str, size: u32) -> Canvas {
        let seed = super::layer_seed(TexturePackStyle::Classic, index);
        match name {
            "grass_top" => grass_top(size, seed),
            "grass_side" => grass_side(size, seed),
            "dirt" => dirt(size, seed),
            "stone" => stone(size, seed),
            "sand" => sand(size, seed),
            "water" => water(size, seed),
            "log_side" => log_side(size, seed),
            "log_top" => log_top(size, seed),
            "leaves" => leaves(size, seed),
            "bedrock" => bedrock(size, seed),
            "gravel" => gravel(size, seed),
            "clay" => clay(size, seed),
            "snow" => snow(size, seed),
            "planks" => planks(size, seed),
            "cobblestone" => cobblestone(size, seed),
            "crafting_table_top" => crafting_table_top(size, seed),
            "crafting_table_side" => crafting_table_side(size, seed),
            "coal_ore" => coal_ore(size, seed),
            "iron_ore" => iron_ore(size, seed),
            "copper_ore" => copper_ore(size, seed),
            "furnace_top" => furnace_top(size, seed),
            "furnace_side" => furnace_side(size, seed),
            "furnace_front" => furnace_front(size, seed),
            "chest_top" => chest_top(size, seed),
            "chest_side" => chest_side(size, seed),
            "chest_front" => chest_front(size, seed),
            "torch" | "farmland_dry" | "farmland_wet" | "wheat_0" | "wheat_1" | "wheat_2" | "wheat_3"
            | "bed_foot_top" | "bed_foot_side" | "bed_head_top" | "bed_head_side" | "fire" => {
                super::extras::layer(TexturePackStyle::Classic, name, size, seed)
            }
            _ => debug(size),
        }
    }

    /// One grass texel shared by the top layer and the side fringe.
    ///
    /// Two- and four-texel clumps carry almost all of the variation; the
    /// eight-texel term only nudges the mean so the field does not read as a
    /// repeating patchwork.
    fn grass_color(seed: u32, x: u32, y: u32, size: u32) -> [u8; 3] {
        let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
        let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
        let broad = u32::from(periodic_noise(seed ^ 0x71, x, y, size, 2));
        let value = (fine * 3 + mid * 2 + broad) / 6;
        let base = if value < 66 {
            GRASS_DARK
        } else if value > 204 {
            GRASS_LIGHT
        } else {
            GRASS
        };
        // A whisper of per-texel tone breaks the unavoidable block repetition.
        offset(base, (hash_noise(seed ^ 0x9b, x, y) & 1) as i32 - 1)
    }

    fn grass_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| grass_color(seed, x, y, size));
        // Two very quiet tufts: enough life without a repeated dash pattern.
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x77, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x77, i, 1) % size) as i32;
            let lean = ((hash >> 12) % 3) as i32 - 1;
            let color = if i == 0 { GRASS_LIGHT } else { GRASS_TUFT };
            paint_blade(&mut canvas, x, y, 2, color, lean);
        }
        // One tiny, muted flower with a dark stem: a detail, not confetti.
        canvas.set(4, 5, FLOWER_GOLD);
        canvas.set(4, 6, GRASS_TUFT);
        canvas
    }

    /// Depth of the green fringe for one column.
    fn fringe_depth(seed: u32, x: u32) -> u32 {
        let clump = hash_noise(seed ^ 0x35, x / 2, 1) % 2;
        let jag = u32::from(hash_noise(seed ^ 0x36, x, 2).is_multiple_of(3));
        3 + clump + jag
    }

    fn grass_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = dirt(size, seed ^ 0x2211);
        for x in 0..size {
            let depth = fringe_depth(seed, x);
            for y in 0..depth {
                let dirt_tone = canvas.get(x as i32, y as i32);
                let grass = grass_color(seed ^ 0x5511, x, y, size);
                let t = ((y + 1) * 200 / depth).min(200);
                let mix = t * t / 256;
                let color = offset(
                    lerp_color(grass, dirt_tone, mix),
                    (hash_noise(seed ^ 0x57, x, y) % 3) as i32 - 1,
                );
                canvas.set(x as i32, y as i32, color);
            }
            // Ragged green tips and occasional dirt notches break the fringe line.
            if hash_noise(seed ^ 0x38, x, 3).is_multiple_of(3) {
                let tip = offset(grass_color(seed ^ 0x5511, x, depth, size), -18);
                canvas.set(x as i32, depth as i32, tip);
            }
            if hash_noise(seed ^ 0x39, x, 4).is_multiple_of(4) {
                let notch = offset(DIRT, (hash_noise(seed ^ 0x3a, x, 5) % 7) as i32 - 3);
                canvas.set(x as i32, depth as i32 - 1, notch);
            }
        }
        // A couple of short roots curling into the soil.
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x99, i, 0);
            let x = (3 + hash % (size - 6)) as i32 + i as i32;
            let depth = fringe_depth(seed, x as u32) as i32;
            let length = 2 + i as i32 + (hash % 2) as i32;
            for step in 0..length {
                let px = x + if step > 0 && ((hash >> (step as u32)) & 1) == 1 {
                    1
                } else {
                    0
                };
                canvas.blend(px, depth + 1 + step, ROOT, (160 - step * 25).max(0) as u32);
            }
        }
        canvas
    }

    fn dirt(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x6d, x, y, size, 4));
            let fine = u32::from(periodic_noise(seed ^ 0x7d, x, y, size, 8));
            let value = (broad * 3 + mid * 2 + fine * 2) / 7;
            if value < 70 {
                DIRT_DARK
            } else if value > 192 {
                DIRT_LIGHT
            } else {
                DIRT
            }
        });
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x44, i, 0);
            let x = (2 + hash % (size - 5)) as i32;
            let y = (size / 3 + hash_noise(seed ^ 0x44, i, 1) % (size - size / 3 - 4)) as i32;
            if i == 0 {
                canvas.set(x, y, DIRT_PEBBLE);
                canvas.blend(x + 1, y + 1, DIRT_SHADOW, 100);
            } else {
                canvas.set(x, y, DIRT_PEBBLE_LIGHT);
                canvas.set(x + 1, y, offset(DIRT_PEBBLE_LIGHT, -10));
                canvas.blend(x, y + 1, DIRT_SHADOW, 130);
                canvas.blend(x + 1, y + 1, DIRT_SHADOW, 90);
            }
        }
        canvas
    }

    fn stone(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let fine = u32::from(periodic_noise(seed ^ 0x77, x, y, size, 8));
            let mid = u32::from(periodic_noise(seed ^ 0x6d, x, y, size, 4));
            let broad = u32::from(periodic_noise(seed ^ 0x5f, x, y, size, 2));
            let value = (fine * 2 + mid * 3 + broad * 3) / 8;
            let base = if value < 72 {
                STONE_SHADOW
            } else if value > 196 {
                STONE_LIGHT
            } else {
                STONE
            };
            offset(base, (hash_noise(seed ^ 0x9d, x, y) % 3) as i32 - 1)
        });
        paint_crack(&mut canvas, seed ^ 0x31, 3, 4, 7, 1, 1, STONE_CRACK, STONE_HIGHLIGHT);
        paint_crack(&mut canvas, seed ^ 0x32, 12, 3, 3, 0, 1, STONE_CRACK, STONE_HIGHLIGHT);
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                STONE_SPEC,
            );
        }
        canvas
    }

    fn sand(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 2));
            let mid = u32::from(periodic_noise(seed ^ 0x9d, x, y, size, 4));
            let fine = u32::from(periodic_noise(seed ^ 0x7d, x, y, size, 8));
            let value = (broad * 3 + mid * 2 + fine * 2) / 7;
            if value < 70 {
                SAND_DARK
            } else if value > 196 {
                SAND_LIGHT
            } else {
                SAND
            }
        });
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x5a, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x5a, i, 1) % size) as i32,
                SAND_SPECK,
            );
        }
        canvas
    }

    fn water(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let wobble = (fbm(seed ^ 0x55, x, y, size, 4, 2) as i32 - 128) / 44;
            let period = 8u32;
            let phase = (y as i32 + wobble).rem_euclid(period as i32) as u32;
            let wave = smooth_fixed(wave01(phase, period));
            let base = lerp_color(WATER_DEEP, WATER, wave);
            let crest = (wave.saturating_sub(205) * 2).min(150);
            lerp_color(base, WATER_CREST, crest)
        });
        // Two short soft crests and a little sparkle: no hard full-width lines.
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x71, i, 0);
            let x = (1 + hash % (size - 5)) as i32;
            let y = (2 + hash_noise(seed ^ 0x71, i, 1) % (size - 4)) as i32;
            for dx in 0..3 {
                canvas.blend(x + dx, y, WATER_CREST, 130 - (dx as u32) * 18);
            }
        }
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x73, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x73, i, 1) % size) as i32,
                WATER_SPARKLE,
            );
        }
        canvas
    }

    /// Irregular vertical bark profile in 0..=255.
    fn bark_ridge(seed: u32, x: u32, size: u32) -> u32 {
        let broad = u32::from(periodic_noise(seed, x, 0, size, 4));
        let fine = u32::from(periodic_noise(seed ^ 0x11, x, 0, size, 8));
        (broad * 3 + fine) / 4
    }

    fn log_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let ridge = bark_ridge(seed, x, size);
            let base = if ridge < 62 {
                BARK_GROOVE
            } else if ridge > 198 {
                BARK_LIGHT
            } else {
                BARK
            };
            let grain = (hash_noise(seed ^ 0x31, x, y / 5) % 5) as i32 - 2;
            let lenticel = if hash_noise(seed ^ 0x53, x, y / 6).is_multiple_of(29) {
                -9
            } else {
                0
            };
            offset(base, grain + lenticel)
        });
        // A small ring-shaped knot, offset from the vertical grain, with a
        // single soft glint rather than a symmetric bright cross.
        paint_ellipse(&mut canvas, 4, 10, 3, 2, BARK_GROOVE, 1);
        paint_ellipse(&mut canvas, 4, 10, 2, 1, BARK, 0);
        canvas.blend(3, 9, BARK_LIGHT, 110);
        canvas.blend(5, 11, BARK_GROOVE, 110);
        canvas
    }

    fn log_top(size: u32, seed: u32) -> Canvas {
        let centre_x = (size * 5 / 16) as i32;
        let centre_y = (size * 6 / 16) as i32;
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let dx = 2 * (x as i32 - centre_x);
            let dy = 2 * (y as i32 - centre_y);
            let wobble = (periodic_noise(seed ^ 0x12, x, y, size, 4) as i32 - 128) / 36;
            let distance = isqrt((dx * dx + dy * dy) as u32) as i32 + wobble;
            let ring = (distance / 3).rem_euclid(2) == 0;
            let base = if distance < 5 {
                RING_HEART
            } else if ring {
                RING_LIGHT
            } else {
                RING_DARK
            };
            let rim = x == 0 || y == 0 || x == size - 1 || y == size - 1;
            offset(
                if rim { BARK_DARK } else { base },
                (hash_noise(seed, x, y) % 3) as i32 - 1,
            )
        });
        // Two radial cracks, stopped short of the bark rim.
        for (index, (dx, dy)) in [(1, -1), (1, 1)].into_iter().enumerate() {
            for step in 3..(size as i32 / 2 - 1) {
                let x = centre_x + step * dx;
                let y = centre_y + step * dy;
                if x > 0 && x < size as i32 - 1 && y > 0 && y < size as i32 - 1 {
                    canvas.blend(x, y, RING_HEART, if index == 0 { 120 } else { 80 });
                }
            }
        }
        canvas
    }

    fn leaves(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 2));
            let mid = u32::from(periodic_noise(seed ^ 0x22, x, y, size, 4));
            let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
            let value = (broad * 3 + mid * 3 + fine * 2) / 8;
            let pocket = u32::from(fbm(seed ^ 0x4d, x, y, size, 4, 2));
            let base = if value < 62 {
                LEAF_DARK
            } else if value < 138 {
                LEAF
            } else if value < 208 {
                LEAF_LIGHT
            } else {
                LEAF_PALE
            };
            if pocket < 60 {
                lerp_color(base, LEAF_SHADOW, 100)
            } else {
                base
            }
        });
        // Sparse leaf tips, mostly diagonal pairs.
        for i in 0..6u32 {
            let hash = hash_noise(seed ^ 0x55, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x55, i, 1) % size) as i32;
            canvas.set(x, y, LEAF_PALE);
            if hash & 1 == 0 {
                canvas.blend(x + 1, y - 1, LEAF_PALE, 150);
            }
        }
        punch_leaf_holes(&mut canvas, seed ^ 0x7a, 15);
        canvas
    }

    fn bedrock(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 2);
            let edge = sample.second - sample.nearest;
            let mottle = u32::from(fbm(seed ^ 0x31, x, y, size, 2, 2));
            let base = if edge < 4 {
                BEDROCK_DARK
            } else if edge < 9 && hash_noise(seed ^ 0x51, x, y) & 1 == 0 {
                BEDROCK_LIGHT
            } else {
                BEDROCK
            };
            offset(base, (mottle as i32 - 128) / 64)
        });
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                BEDROCK_LIGHT,
            );
        }
        canvas
    }

    fn gravel(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, GRAVEL_GAP);
        let palette = [GRAVEL_GREY, GRAVEL_BEIGE, GRAVEL_TAN, GRAVEL_PALE];
        for pebble in scatter_pebbles(seed, size, 140, 1, 2) {
            let color = palette[(pebble.id >> 8) as usize % palette.len()];
            paint_gravel_pebble(&mut canvas, &pebble, color, 8, -12, 0);
        }
        canvas
    }

    fn clay(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 2));
            let mid = u32::from(periodic_noise(seed ^ 0x23, x, y, size, 4));
            let value = (broad * 3 + mid) / 4;
            if value < 90 {
                CLAY_DARK
            } else if value > 180 {
                CLAY_LIGHT
            } else {
                CLAY
            }
        });
        // Two thin, gently waving strata rather than a striped wall.
        for (index, base_y) in [size / 3, size * 2 / 3].into_iter().enumerate() {
            for x in 0..size {
                let wobble = (periodic_noise(seed ^ 0x31, x, 0, size, 4) as i32 - 128) / 40;
                let y = base_y as i32 + index as i32 + wobble;
                canvas.blend(x as i32, y, CLAY_DARK, 110);
            }
        }
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x2f, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x2f, i, 1) % size) as i32,
                CLAY_LIGHT,
            );
        }
        canvas
    }

    fn snow(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x47, x, y, size, 4));
            let value = (broad * 3 + mid) / 4;
            if value < 84 {
                SNOW_SHADOW
            } else if value > 218 {
                SNOW_HIGHLIGHT
            } else {
                SNOW
            }
        })
    }

    fn planks(size: u32, seed: u32) -> Canvas {
        let boards = 4u32;
        let board_height = (size / boards).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let board = y / board_height;
            let row = y % board_height;
            let tone = (hash_noise(seed ^ 0x11, board, 0) % 5) as i32 - 2;
            let grain = if hash_noise(seed ^ 0x33, x / 3, y).is_multiple_of(9) {
                -13
            } else if hash_noise(seed ^ 0x35, x / 4, y).is_multiple_of(11) {
                9
            } else {
                (hash_noise(seed ^ 0x37, x, y) % 3) as i32 - 1
            };
            if row == 0 {
                offset(PLANK_SHADOW, tone)
            } else if row + 1 == board_height {
                offset(PLANK_LIGHT, tone + grain / 2)
            } else {
                offset(PLANK, tone + grain)
            }
        });
        // One offset butt joint per board, lit along its right edge.
        for board in 0..boards {
            let joint = 2 + (hash_noise(seed ^ 0x44, board, 1) % (size - 4).max(1)) as i32;
            for y in board * board_height + 1..(board + 1) * board_height {
                canvas.set(joint, y as i32, PLANK_GRAIN);
                canvas.set(joint + 1, y as i32, PLANK_HIGHLIGHT);
            }
        }
        canvas
    }

    fn cobblestone(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 4);
            let edge = sample.second - sample.nearest;
            let centre = isqrt(sample.nearest) as i32;
            let grain = (hash_noise(seed ^ 0x31, x, y) % 5) as i32 - 2;
            if edge < 2 {
                offset(COBBLE_MORTAR, grain)
            } else {
                let base = if centre == 0 {
                    COBBLE_HIGHLIGHT
                } else if centre == 1 {
                    COBBLE_LIGHT
                } else if centre <= 3 {
                    COBBLE
                } else {
                    COBBLE_DARK
                };
                offset(base, grain)
            }
        })
    }

    fn crafting_table_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let board = y / (size / 4).max(1);
            let tone = (hash_noise(seed ^ 0x61, board, 0) % 5) as i32 - 2;
            let grain = if hash_noise(seed ^ 0x63, x / 3, y).is_multiple_of(8) {
                -11
            } else {
                (hash_noise(seed ^ 0x65, x, y) % 3) as i32 - 1
            };
            offset(PLANK_LIGHT, tone + grain)
        });
        paint_crafting_grid(
            &mut canvas,
            TABLE_FRAME,
            TABLE_DIVIDER,
            TABLE_CELL,
            PLANK_HIGHLIGHT,
            TABLE_STUD,
            1,
            1,
        );
        canvas
    }

    fn crafting_table_side(size: u32, seed: u32) -> Canvas {
        let lip = (size / 5).max(3);
        let board_width = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            if y < lip {
                let tone = (hash_noise(seed ^ 0x71, y, 0) % 3) as i32 - 1;
                if y == 0 {
                    PLANK_HIGHLIGHT
                } else if y + 1 == lip {
                    PLANK_SHADOW
                } else {
                    offset(PLANK_LIGHT, tone)
                }
            } else {
                let board = x / board_width;
                let tone = (hash_noise(seed ^ 0x73, board, 0) % 5) as i32 - 2;
                let grain = if hash_noise(seed ^ 0x75, x, y / 3).is_multiple_of(11) {
                    -10
                } else {
                    0
                };
                if x % board_width == 0 {
                    offset(PLANK_SHADOW, tone)
                } else {
                    offset(PLANK, tone + grain)
                }
            }
        });
        paint_table_tool(
            &mut canvas,
            (size as i32 * 3) / 5,
            (size as i32 * 7) / 10,
            TOOL_METAL,
            TOOL_METAL_DARK,
            PLANK_GRAIN,
            PLANK_HIGHLIGHT,
            (size / 16).max(1) as i32,
        );
        // Two studs under the table lip hold the frame together.
        canvas.set(2, lip as i32 + 1, TOOL_METAL);
        canvas.set(size as i32 - 3, lip as i32 + 1, TOOL_METAL);
        canvas
    }

    fn coal_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x41);
        paint_ore(&mut canvas, seed ^ 0x51, &COAL_LOOK);
        canvas
    }

    fn iron_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x42);
        paint_ore(&mut canvas, seed ^ 0x52, &IRON_LOOK);
        canvas
    }

    fn copper_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x43);
        paint_ore(&mut canvas, seed ^ 0x53, &COPPER_LOOK);
        canvas
    }

    fn furnace_top(size: u32, seed: u32) -> Canvas {
        furnace_top_from(stone(size, seed ^ 0x61), seed, &FURNACE_LOOK)
    }

    fn furnace_side(size: u32, seed: u32) -> Canvas {
        furnace_side_from(stone(size, seed ^ 0x62), seed, &FURNACE_LOOK)
    }

    fn furnace_front(size: u32, seed: u32) -> Canvas {
        furnace_front_from(stone(size, seed ^ 0x63), seed, &FURNACE_LOOK)
    }

    fn chest_top(size: u32, seed: u32) -> Canvas {
        chest_top_from(planks(size, seed ^ 0x71), seed, &CHEST_LOOK)
    }

    fn chest_side(size: u32, seed: u32) -> Canvas {
        chest_side_from(planks(size, seed ^ 0x72), seed, &CHEST_LOOK)
    }

    fn chest_front(size: u32, seed: u32) -> Canvas {
        chest_front_from(planks(size, seed ^ 0x73), seed, &CHEST_LOOK)
    }

    fn debug(size: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            if (x / 4 + y / 4).is_multiple_of(2) {
                [235, 44, 208]
            } else {
                [32, 32, 38]
            }
        })
    }
}

/// Painterly 32² pack: muted warm colours, soft low-contrast gradients and
/// gentle Bayer dithering, in the spirit of hand-painted fantasy block games.
mod painterly {
    use super::*;

    const MOSS_SHADOW: [u8; 3] = [92, 120, 70];
    const MOSS: [u8; 3] = [108, 136, 82];
    const MOSS_LIGHT: [u8; 3] = [130, 158, 100];
    const MOSS_CLUMP: [u8; 3] = [144, 170, 110];
    const FLOWER_GOLD: [u8; 3] = [206, 181, 104];
    const FLOWER_BLUE: [u8; 3] = [138, 160, 178];
    const STEM: [u8; 3] = [74, 98, 56];

    const EARTH_SHADOW: [u8; 3] = [108, 82, 59];
    const EARTH: [u8; 3] = [128, 98, 70];
    const EARTH_LIGHT: [u8; 3] = [150, 118, 86];
    const EARTH_PEBBLE: [u8; 3] = [158, 128, 96];
    const ROOT: [u8; 3] = [83, 59, 39];

    const STONE_SHADOW: [u8; 3] = [126, 121, 111];
    const STONE: [u8; 3] = [144, 139, 129];
    const STONE_LIGHT: [u8; 3] = [164, 159, 148];
    const STONE_CRACK: [u8; 3] = [116, 111, 102];
    const STONE_HIGHLIGHT: [u8; 3] = [178, 173, 161];
    const STONE_SPEC: [u8; 3] = [120, 115, 106];

    const SAND_SHADOW: [u8; 3] = [211, 193, 157];
    const SAND: [u8; 3] = [228, 211, 173];
    const SAND_LIGHT: [u8; 3] = [241, 226, 193];
    const SHELL: [u8; 3] = [247, 238, 216];

    const WATER_DEEP: [u8; 3] = [32, 82, 88];
    const WATER: [u8; 3] = [45, 105, 111];
    const WATER_CREST: [u8; 3] = [104, 156, 153];
    const WATER_SHEEN: [u8; 3] = [146, 184, 176];

    const BARK_GROOVE: [u8; 3] = [92, 64, 38];
    const BARK: [u8; 3] = [140, 103, 64];
    const BARK_LIGHT: [u8; 3] = [166, 126, 79];
    const KNOT: [u8; 3] = [100, 70, 41];
    const BARK_HIGHLIGHT: [u8; 3] = [184, 146, 96];

    const RING_DARK: [u8; 3] = [122, 88, 51];
    const RING_LIGHT: [u8; 3] = [176, 136, 86];
    const RING_HEART: [u8; 3] = [105, 74, 44];
    const BARK_RIM: [u8; 3] = [80, 55, 34];

    const LEAF_SHADOW: [u8; 3] = [44, 74, 51];
    const LEAF: [u8; 3] = [68, 106, 63];
    const LEAF_LIGHT: [u8; 3] = [88, 128, 78];
    const LEAF_PALE: [u8; 3] = [112, 150, 94];
    const LEAF_TIP: [u8; 3] = [132, 166, 108];

    const BEDROCK_SHADOW: [u8; 3] = [33, 31, 37];
    const BEDROCK: [u8; 3] = [55, 52, 56];
    const BEDROCK_LIGHT: [u8; 3] = [89, 85, 84];

    const GRAVEL_GAP: [u8; 3] = [88, 83, 76];
    const GRAVEL_GREY: [u8; 3] = [130, 124, 115];
    const GRAVEL_BEIGE: [u8; 3] = [152, 132, 106];
    const GRAVEL_WARM: [u8; 3] = [138, 116, 92];
    const GRAVEL_PALE: [u8; 3] = [164, 157, 144];

    const CLAY_SHADOW: [u8; 3] = [145, 155, 169];
    const CLAY: [u8; 3] = [164, 173, 184];
    const CLAY_LIGHT: [u8; 3] = [182, 190, 199];

    const SNOW_SHADOW: [u8; 3] = [224, 234, 245];
    const SNOW: [u8; 3] = [246, 247, 249];
    const SNOW_PINK: [u8; 3] = [248, 238, 241];

    // v0.0.8 wood, cobblestone and crafting-table palettes.
    const PLANK_SHADOW: [u8; 3] = [108, 78, 50];
    const PLANK: [u8; 3] = [152, 114, 76];
    const PLANK_LIGHT: [u8; 3] = [183, 146, 104];
    const PLANK_GRAIN: [u8; 3] = [126, 90, 55];
    const PLANK_HIGHLIGHT: [u8; 3] = [203, 168, 122];

    const COBBLE_MORTAR: [u8; 3] = [81, 77, 69];
    const COBBLE_DARK: [u8; 3] = [116, 112, 102];
    const COBBLE: [u8; 3] = [147, 142, 131];
    const COBBLE_LIGHT: [u8; 3] = [175, 169, 154];
    const COBBLE_HIGHLIGHT: [u8; 3] = [196, 190, 173];

    const TABLE_FRAME: [u8; 3] = [99, 71, 45];
    const TABLE_DIVIDER: [u8; 3] = [88, 61, 37];
    const TABLE_CELL: [u8; 3] = [141, 104, 67];
    const TABLE_STUD: [u8; 3] = [201, 166, 116];
    const TOOL_METAL: [u8; 3] = [124, 128, 132];
    const TOOL_METAL_DARK: [u8; 3] = [94, 98, 103];

    // v1.0 ore, furnace and chest palettes.
    const COAL_LOOK: OreLook = OreLook {
        dark: [47, 43, 42],
        mid: [70, 64, 60],
        light: [96, 89, 82],
        accent: None,
        clusters: 3,
        radius: 4,
        soft: 3,
    };
    const IRON_LOOK: OreLook = OreLook {
        dark: [120, 94, 68],
        mid: [170, 137, 100],
        light: [205, 173, 132],
        accent: None,
        clusters: 3,
        radius: 4,
        soft: 3,
    };
    const COPPER_LOOK: OreLook = OreLook {
        dark: [128, 73, 53],
        mid: [190, 112, 78],
        light: [219, 150, 110],
        accent: Some([116, 158, 134]),
        clusters: 3,
        radius: 4,
        soft: 3,
    };
    const FURNACE_LOOK: FurnaceLook = FurnaceLook {
        edge_dark: [112, 107, 98],
        edge_light: [181, 176, 164],
        seam: [102, 97, 89],
        metal: [132, 128, 121],
        metal_dark: [90, 87, 82],
        opening: [44, 38, 34],
        opening_inner: [27, 24, 23],
        ember_dark: [150, 68, 31],
        ember: [219, 122, 49],
        ember_hot: [246, 185, 93],
        trim: 2,
        soft: 2,
        dither: 2,
        rivets: true,
    };
    const CHEST_LOOK: ChestLook = ChestLook {
        seam: [91, 66, 44],
        seam_light: [203, 168, 122],
        metal: [135, 131, 124],
        metal_dark: [92, 89, 84],
        metal_light: [180, 175, 165],
        keyhole: [29, 25, 22],
        trim: 2,
        dither: 2,
    };

    /// Generates one painterly layer.
    pub(super) fn layer(index: u32, name: &str, size: u32) -> Canvas {
        let seed = super::layer_seed(TexturePackStyle::Painterly, index);
        match name {
            "grass_top" => grass_top(size, seed),
            "grass_side" => grass_side(size, seed),
            "dirt" => dirt(size, seed),
            "stone" => stone(size, seed),
            "sand" => sand(size, seed),
            "water" => water(size, seed),
            "log_side" => log_side(size, seed),
            "log_top" => log_top(size, seed),
            "leaves" => leaves(size, seed),
            "bedrock" => bedrock(size, seed),
            "gravel" => gravel(size, seed),
            "clay" => clay(size, seed),
            "snow" => snow(size, seed),
            "planks" => planks(size, seed),
            "cobblestone" => cobblestone(size, seed),
            "crafting_table_top" => crafting_table_top(size, seed),
            "crafting_table_side" => crafting_table_side(size, seed),
            "coal_ore" => coal_ore(size, seed),
            "iron_ore" => iron_ore(size, seed),
            "copper_ore" => copper_ore(size, seed),
            "furnace_top" => furnace_top(size, seed),
            "furnace_side" => furnace_side(size, seed),
            "furnace_front" => furnace_front(size, seed),
            "chest_top" => chest_top(size, seed),
            "chest_side" => chest_side(size, seed),
            "chest_front" => chest_front(size, seed),
            "torch" | "farmland_dry" | "farmland_wet" | "wheat_0" | "wheat_1" | "wheat_2" | "wheat_3"
            | "bed_foot_top" | "bed_foot_side" | "bed_head_top" | "bed_head_side" | "fire" => {
                super::extras::layer(TexturePackStyle::Painterly, name, size, seed)
            }
            _ => debug(size),
        }
    }

    /// Soft mossy green used by the grass top and the side fringe.
    fn grass_color(seed: u32, x: u32, y: u32, size: u32) -> [u8; 3] {
        let broad = u32::from(fbm(seed, x, y, size, 2, 3));
        let mid = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
        let fine = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 16));
        let value = (broad * 5 + mid * 2 + fine) / 8;
        offset(lerp_color(MOSS_SHADOW, MOSS_LIGHT, value), dither(x, y, 1))
    }

    fn grass_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| grass_color(seed, x, y, size));
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x77, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x77, i, 1) % size) as i32;
            let lean = ((hash >> 12) % 3) as i32 - 1;
            let color = if hash.is_multiple_of(5) { MOSS } else { MOSS_CLUMP };
            paint_blade(&mut canvas, x, y, 2, color, lean);
        }
        // Two tiny, muted flowers: deliberately unbalanced across the tile.
        for (x, y, color) in [(7, 8, FLOWER_GOLD), (23, 5, FLOWER_BLUE)] {
            canvas.set(x, y, color);
            canvas.set(x, y + 1, STEM);
        }
        canvas
    }

    fn fringe_depth(seed: u32, x: u32, size: u32) -> u32 {
        let clump = hash_noise(seed ^ 0x35, x / 3, 1) % 3;
        let jag = u32::from(hash_noise(seed ^ 0x36, x, 2).is_multiple_of(3));
        (6 + clump + jag).min(size / 3)
    }

    fn grass_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = dirt(size, seed ^ 0x2211);
        for x in 0..size {
            let depth = fringe_depth(seed, x, size);
            for y in 0..depth {
                let soil = canvas.get(x as i32, y as i32);
                let grass = grass_color(seed ^ 0x5511, x, y, size);
                let t = ((y + 1) * 190 / depth).min(190);
                let mix = t * t / 256;
                canvas.set(x as i32, y as i32, lerp_color(grass, soil, mix));
            }
            if hash_noise(seed ^ 0x38, x, 3).is_multiple_of(3) {
                let tip = offset(grass_color(seed ^ 0x5511, x, depth, size), -14);
                canvas.set(x as i32, depth as i32, tip);
            }
            if hash_noise(seed ^ 0x39, x, 4).is_multiple_of(5) {
                let notch = offset(EARTH, (hash_noise(seed ^ 0x3a, x, 5) % 7) as i32 - 3);
                canvas.set(x as i32, depth as i32 - 1, notch);
            }
        }
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x99, i, 0);
            let x = (5 + hash % (size - 10)) as i32 + i as i32;
            let depth = fringe_depth(seed, x as u32, size) as i32;
            let length = 4 + i as i32 + (hash % 3) as i32;
            for step in 0..length {
                let px = x + if step > 0 && ((hash >> (step as u32)) & 1) == 1 {
                    1
                } else {
                    0
                };
                canvas.blend(px, depth + 1 + step, ROOT, (180 - step * 18).max(0) as u32);
            }
        }
        canvas
    }

    fn dirt(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x6d, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x7d, x, y, size, 16));
            let value = (broad * 5 + mid * 2 + fine) / 8;
            offset(lerp_color(EARTH_SHADOW, EARTH_LIGHT, value), dither(x, y, 2))
        });
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x44, i, 0);
            let x = (2 + hash % (size - 6)) as i32;
            let y = (size / 3 + hash_noise(seed ^ 0x44, i, 1) % (size - size / 3 - 5)) as i32;
            let color = if i == 0 { EARTH_LIGHT } else { EARTH_PEBBLE };
            paint_ellipse(&mut canvas, x, y, 2, 1, color, 1);
            canvas.blend(x + 2, y + 1, EARTH_SHADOW, 80);
        }
        canvas
    }

    fn stone(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x77, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 16));
            let value = (broad * 5 + mid * 2 + fine) / 8;
            offset(lerp_color(STONE_SHADOW, STONE_LIGHT, value), dither(x, y, 2))
        });
        paint_crack(&mut canvas, seed ^ 0x31, 8, 6, 10, 1, 1, STONE_CRACK, STONE_HIGHLIGHT);
        paint_crack(&mut canvas, seed ^ 0x32, 24, 8, 6, -1, 1, STONE_CRACK, STONE_HIGHLIGHT);
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            let color = if i % 2 == 0 { STONE_SPEC } else { STONE };
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                color,
            );
        }
        canvas
    }

    fn sand(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x9d, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x7d, x, y, size, 16));
            let value = (broad * 5 + mid * 2 + fine) / 8;
            offset(lerp_color(SAND_SHADOW, SAND_LIGHT, value), dither(x, y, 2))
        });
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x5a, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x5a, i, 1) % size) as i32;
            if i == 0 {
                canvas.blend(x, y, SHELL, 200);
            } else {
                canvas.set(x, y, offset(SAND, -12));
            }
        }
        canvas
    }

    fn water(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let wobble = (fbm(seed ^ 0x55, x, y, size, 4, 2) as i32 - 128) / 30;
            let period = 16u32;
            let phase = (y as i32 + wobble).rem_euclid(period as i32) as u32;
            let wave = smooth_fixed(wave01(phase, period));
            let base = lerp_color(WATER_DEEP, WATER, wave);
            let crest = (wave.saturating_sub(210) * 2).min(120);
            offset(lerp_color(base, WATER_CREST, crest), dither(x, y, 1))
        });
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x71, i, 0);
            let x = (2 + hash % (size - 8)) as i32;
            let y = (3 + hash_noise(seed ^ 0x71, i, 1) % (size - 6)) as i32;
            for dx in 0..4 {
                canvas.blend(x + dx, y, WATER_SHEEN, 90 - (dx as u32) * 12);
            }
        }
        canvas
    }

    fn bark_ridge(seed: u32, x: u32, size: u32) -> u32 {
        let broad = u32::from(periodic_noise(seed, x, 0, size, 4));
        let fine = u32::from(periodic_noise(seed ^ 0x11, x, 0, size, 8));
        (broad * 3 + fine) / 4
    }

    fn log_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let ridge = bark_ridge(seed, x, size);
            let base = ramp3(BARK_GROOVE, BARK, BARK_LIGHT, ridge);
            let grain = (hash_noise(seed ^ 0x31, x, y / 7) % 5) as i32 - 2;
            let glint = if hash_noise(seed ^ 0x51, x, y / 11).is_multiple_of(29) {
                10
            } else {
                0
            };
            offset(base, grain + glint + dither(x, y, 1))
        });
        paint_ellipse(&mut canvas, 22, 9, 4, 3, KNOT, 2);
        paint_ellipse(&mut canvas, 22, 9, 2, 1, BARK, 0);
        canvas.blend(21, 8, BARK_HIGHLIGHT, 80);
        canvas.blend(25, 10, KNOT, 80);
        paint_ellipse(&mut canvas, 7, 23, 3, 2, KNOT, 2);
        paint_ellipse(&mut canvas, 7, 23, 1, 1, BARK, 0);
        canvas.blend(6, 22, BARK_HIGHLIGHT, 70);
        canvas.blend(9, 24, KNOT, 80);
        canvas
    }

    fn log_top(size: u32, seed: u32) -> Canvas {
        let centre_x = (size * 13 / 32) as i32;
        let centre_y = (size * 14 / 32) as i32;
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let dx = 2 * (x as i32 - centre_x);
            let dy = 2 * (y as i32 - centre_y);
            let wobble = (fbm(seed ^ 0x12, x, y, size, 4, 2) as i32 - 128) / 30;
            let distance = isqrt((dx * dx + dy * dy) as u32) as i32 + wobble;
            let ring = smooth_fixed(wave01((distance.max(0) as u32) / 2, size / 2));
            let mut base = lerp_color(RING_DARK, RING_LIGHT, ring);
            if distance < 10 {
                base = lerp_color(base, RING_HEART, 130);
            }
            let rim = x < 2 || y < 2 || x >= size - 2 || y >= size - 2;
            if rim {
                offset(BARK_RIM, (hash_noise(seed, x, y / 4) % 7) as i32 - 3)
            } else {
                offset(base, dither(x, y, 2))
            }
        });
        for (index, (dx, dy)) in [(1, -1), (1, 1), (-1, 1)].into_iter().enumerate() {
            for step in 5..(size as i32 / 2 - 2) {
                let x = centre_x + step * dx;
                let y = centre_y + step * dy;
                if x > 1 && x < size as i32 - 2 && y > 1 && y < size as i32 - 2 {
                    canvas.blend(x, y, RING_HEART, if index == 0 { 90 } else { 60 });
                }
            }
        }
        canvas
    }

    fn leaves(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x22, x, y, size, 8));
            let value = (broad * 6 + mid * 2) / 8;
            let pocket = u32::from(fbm(seed ^ 0x4d, x, y, size, 4, 2));
            let mut color = offset(ramp3(LEAF_SHADOW, LEAF, LEAF_PALE, value), dither(x, y, 2));
            if pocket < 64 {
                color = lerp_color(color, LEAF_SHADOW, 110);
            } else if pocket > 196 {
                color = lerp_color(color, LEAF_LIGHT, 90);
            }
            color
        });
        for i in 0..8u32 {
            let hash = hash_noise(seed ^ 0x55, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x55, i, 1) % size) as i32;
            canvas.set(x, y, LEAF_TIP);
            if hash & 1 == 0 {
                canvas.blend(x + 1, y - 1, LEAF_TIP, 130);
            }
        }
        punch_leaf_holes(&mut canvas, seed ^ 0x7a, 11);
        canvas
    }

    fn bedrock(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 4);
            let edge = sample.second - sample.nearest;
            let broad = u32::from(fbm(seed ^ 0x31, x, y, size, 2, 3));
            let mut base = ramp3(BEDROCK_SHADOW, BEDROCK, BEDROCK_LIGHT, broad);
            if edge < 7 {
                base = lerp_color(base, BEDROCK_SHADOW, 170);
            } else if edge < 16 {
                base = lerp_color(base, BEDROCK_LIGHT, 60);
            }
            offset(base, dither(x, y, 2))
        });
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            paint_ellipse(
                &mut canvas,
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                2,
                1,
                BEDROCK_LIGHT,
                2,
            );
        }
        canvas
    }

    fn gravel(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, GRAVEL_GAP);
        let palette = [GRAVEL_GREY, GRAVEL_BEIGE, GRAVEL_WARM, GRAVEL_PALE];
        for pebble in scatter_pebbles(seed, size, 360, 2, 3) {
            let color = palette[(pebble.id >> 8) as usize % palette.len()];
            paint_gravel_pebble(&mut canvas, &pebble, color, 10, -10, 1);
        }
        canvas
    }

    fn clay(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x23, x, y, size, 8));
            let value = (broad * 6 + mid * 2) / 8;
            offset(ramp3(CLAY_SHADOW, CLAY, CLAY_LIGHT, value), dither(x, y, 2))
        });
        for (index, base_y) in [size / 3, size * 2 / 3].into_iter().enumerate() {
            for x in 0..size {
                let wobble = (periodic_noise(seed ^ 0x31, x, 0, size, 8) as i32 - 128) / 34;
                let y = base_y as i32 + index as i32 + wobble;
                canvas.blend(x as i32, y, CLAY_SHADOW, 75);
                canvas.blend(x as i32, y + 1, CLAY_LIGHT, 45);
            }
        }
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x2f, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x2f, i, 1) % size) as i32,
                CLAY_LIGHT,
            );
        }
        canvas
    }

    fn snow(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x47, x, y, size, 8));
            let value = (broad * 6 + mid * 2) / 8;
            let base = lerp_color(SNOW_SHADOW, SNOW, value);
            let warm = u32::from(fbm(seed ^ 0x93, x, y, size, 2, 2));
            let tint = if warm > 184 {
                lerp_color(base, SNOW_PINK, 38)
            } else {
                base
            };
            offset(tint, dither(x, y, 1))
        })
    }

    fn planks(size: u32, seed: u32) -> Canvas {
        let board_height = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let board = y / board_height;
            let row = y % board_height;
            let broad = u32::from(fbm(seed ^ 0x21, x, y, size, 4, 2));
            let fine = u32::from(periodic_noise(seed ^ 0x33, x, y, size, 16));
            let value = (broad * 3 + fine) / 4;
            let base = ramp3(PLANK_SHADOW, PLANK, PLANK_LIGHT, value);
            let grain = (periodic_noise(seed ^ 0x44, x / 2, y, size, 4) as i32 - 128) / 64;
            let tone = (hash_noise(seed ^ 0x11, board, 0) % 5) as i32 - 2;
            if row == 0 {
                offset(PLANK_SHADOW, tone)
            } else if row + 1 == board_height {
                offset(PLANK_LIGHT, tone + grain)
            } else {
                offset(base, tone + grain + dither(x, y, 2))
            }
        });
        // Soft knots and a dark butt joint with a lit edge on every board.
        for board in 0..4u32 {
            let joint = 3 + (hash_noise(seed ^ 0x55, board, 1) % (size - 6).max(1)) as i32;
            for y in board * board_height..(board + 1) * board_height {
                canvas.blend(joint, y as i32, PLANK_SHADOW, 190);
            }
        }
        paint_ellipse(
            &mut canvas,
            (size * 3 / 4) as i32,
            (size / 4) as i32 + 1,
            2,
            1,
            PLANK_GRAIN,
            1,
        );
        paint_ellipse(
            &mut canvas,
            (size / 5) as i32,
            (size * 3 / 4) as i32,
            2,
            1,
            PLANK_GRAIN,
            1,
        );
        canvas
    }

    fn cobblestone(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 4);
            let edge = sample.second - sample.nearest;
            let centre = isqrt(sample.nearest) as i32;
            let grain = (hash_noise(seed ^ 0x31, x, y) % 5) as i32 - 2 + dither(x, y, 2);
            if edge < 8 {
                offset(COBBLE_MORTAR, grain)
            } else {
                let base = if centre == 0 {
                    COBBLE_HIGHLIGHT
                } else if centre <= 1 {
                    COBBLE_LIGHT
                } else if centre <= 4 {
                    COBBLE
                } else {
                    COBBLE_DARK
                };
                offset(base, grain)
            }
        })
    }

    fn crafting_table_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = planks(size, seed ^ 0x1a2b);
        paint_crafting_grid(
            &mut canvas,
            TABLE_FRAME,
            TABLE_DIVIDER,
            TABLE_CELL,
            PLANK_HIGHLIGHT,
            TABLE_STUD,
            2,
            2,
        );
        canvas
    }

    fn crafting_table_side(size: u32, seed: u32) -> Canvas {
        let lip = (size / 5).max(4);
        let board_width = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            if y < lip {
                let value = u32::from(fbm(seed ^ 0x71, x, y, size, 4, 2));
                let base = lerp_color(PLANK_LIGHT, PLANK, value);
                if y == 0 {
                    offset(PLANK_HIGHLIGHT, -2)
                } else if y + 1 == lip {
                    PLANK_SHADOW
                } else {
                    offset(base, dither(x, y, 1))
                }
            } else {
                let board = x / board_width;
                let tone = (hash_noise(seed ^ 0x73, board, 0) % 3) as i32 - 1;
                let broad = u32::from(fbm(seed ^ 0x73, x, y, size, 4, 2));
                let base = ramp3(PLANK_SHADOW, PLANK, PLANK_LIGHT, broad);
                if x % board_width == 0 {
                    offset(PLANK_SHADOW, 2)
                } else {
                    offset(base, tone + dither(x, y, 2))
                }
            }
        });
        paint_table_tool(
            &mut canvas,
            (size as i32 * 3) / 5,
            (size as i32 * 7) / 10,
            TOOL_METAL,
            TOOL_METAL_DARK,
            PLANK_GRAIN,
            PLANK_HIGHLIGHT,
            (size / 16).max(1) as i32,
        );
        // A shelf shadow under the lip and two warm studs.
        for x in 0..size {
            canvas.blend(x as i32, (lip + size / 5) as i32, PLANK_SHADOW, 70);
        }
        canvas.set(4, lip as i32 + 2, TABLE_STUD);
        canvas.set(size as i32 - 5, lip as i32 + 2, TABLE_STUD);
        canvas
    }

    fn coal_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x41);
        paint_ore(&mut canvas, seed ^ 0x51, &COAL_LOOK);
        canvas
    }

    fn iron_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x42);
        paint_ore(&mut canvas, seed ^ 0x52, &IRON_LOOK);
        canvas
    }

    fn copper_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x43);
        paint_ore(&mut canvas, seed ^ 0x53, &COPPER_LOOK);
        canvas
    }

    fn furnace_top(size: u32, seed: u32) -> Canvas {
        furnace_top_from(stone(size, seed ^ 0x61), seed, &FURNACE_LOOK)
    }

    fn furnace_side(size: u32, seed: u32) -> Canvas {
        furnace_side_from(stone(size, seed ^ 0x62), seed, &FURNACE_LOOK)
    }

    fn furnace_front(size: u32, seed: u32) -> Canvas {
        furnace_front_from(stone(size, seed ^ 0x63), seed, &FURNACE_LOOK)
    }

    fn chest_top(size: u32, seed: u32) -> Canvas {
        chest_top_from(planks(size, seed ^ 0x71), seed, &CHEST_LOOK)
    }

    fn chest_side(size: u32, seed: u32) -> Canvas {
        chest_side_from(planks(size, seed ^ 0x72), seed, &CHEST_LOOK)
    }

    fn chest_front(size: u32, seed: u32) -> Canvas {
        chest_front_from(planks(size, seed ^ 0x73), seed, &CHEST_LOOK)
    }

    fn debug(size: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let cell = (x / 8 + y / 8).is_multiple_of(2);
            let base = if cell { [202, 92, 162] } else { [50, 48, 58] };
            offset(base, dither(x, y, 3))
        })
    }
}

/// Lush 32² pack: richer natural detail, mossy stone and brighter water, while
/// keeping the values close enough together to avoid a neon read.
mod lush {
    use super::*;

    const GRASS_SHADOW: [u8; 3] = [48, 120, 50];
    const GRASS_DARK: [u8; 3] = [58, 136, 53];
    const GRASS: [u8; 3] = [69, 151, 59];
    const GRASS_LIGHT: [u8; 3] = [80, 164, 66];
    const GRASS_BRIGHT: [u8; 3] = [92, 177, 74];
    const GRASS_TUFT: [u8; 3] = [48, 112, 42];
    const CLOVER: [u8; 3] = [100, 170, 72];

    const EARTH_SHADOW: [u8; 3] = [94, 67, 44];
    const EARTH: [u8; 3] = [118, 86, 56];
    const EARTH_LIGHT: [u8; 3] = [144, 108, 72];
    const EARTH_PEBBLE: [u8; 3] = [156, 124, 88];
    const ROOT: [u8; 3] = [70, 48, 30];

    const STONE_SHADOW: [u8; 3] = [102, 112, 104];
    const STONE: [u8; 3] = [122, 132, 122];
    const STONE_LIGHT: [u8; 3] = [148, 158, 144];
    const STONE_CRACK: [u8; 3] = [94, 104, 96];
    const STONE_HIGHLIGHT: [u8; 3] = [166, 176, 160];
    const MOSS: [u8; 3] = [74, 124, 56];
    const MOSS_LIGHT: [u8; 3] = [96, 148, 70];

    const SAND_SHADOW: [u8; 3] = [208, 187, 138];
    const SAND: [u8; 3] = [230, 210, 158];
    const SAND_LIGHT: [u8; 3] = [245, 227, 180];
    const SHELL: [u8; 3] = [250, 242, 222];
    const SHELL_PINK: [u8; 3] = [238, 206, 184];

    const WATER_DEEP: [u8; 3] = [20, 118, 134];
    const WATER: [u8; 3] = [34, 157, 164];
    const WATER_LIGHT: [u8; 3] = [82, 198, 196];
    const FOAM: [u8; 3] = [226, 250, 244];

    const BARK_GROOVE: [u8; 3] = [66, 42, 22];
    const BARK: [u8; 3] = [126, 86, 45];
    const BARK_LIGHT: [u8; 3] = [174, 126, 68];
    const BARK_HIGHLIGHT: [u8; 3] = [196, 152, 92];
    const KNOT: [u8; 3] = [88, 56, 27];

    const RING_DARK: [u8; 3] = [118, 78, 40];
    const RING_LIGHT: [u8; 3] = [180, 136, 72];
    const RING_HEART: [u8; 3] = [92, 61, 31];
    const BARK_RIM: [u8; 3] = [84, 52, 27];

    const LEAF_SHADOW: [u8; 3] = [22, 58, 33];
    const LEAF_DARK: [u8; 3] = [32, 78, 37];
    const LEAF: [u8; 3] = [56, 122, 48];
    const LEAF_LIGHT: [u8; 3] = [84, 154, 63];
    const LEAF_PALE: [u8; 3] = [120, 184, 84];
    const LEAF_TIP: [u8; 3] = [146, 202, 100];

    const BEDROCK_SHADOW: [u8; 3] = [28, 27, 33];
    const BEDROCK: [u8; 3] = [58, 56, 62];
    const BEDROCK_LIGHT: [u8; 3] = [104, 100, 102];

    const GRAVEL_GAP: [u8; 3] = [72, 68, 64];
    const GRAVEL_GREY: [u8; 3] = [124, 122, 116];
    const GRAVEL_BEIGE: [u8; 3] = [156, 134, 104];
    const GRAVEL_WARM: [u8; 3] = [136, 108, 82];
    const GRAVEL_COOL: [u8; 3] = [108, 110, 116];
    const GRAVEL_PALE: [u8; 3] = [166, 158, 144];

    const CLAY_SHADOW: [u8; 3] = [128, 144, 160];
    const CLAY: [u8; 3] = [146, 160, 174];
    const CLAY_LIGHT: [u8; 3] = [172, 184, 196];

    const SNOW_SHADOW: [u8; 3] = [236, 244, 252];
    const SNOW: [u8; 3] = [244, 248, 252];

    // v0.0.8 wood, cobblestone and crafting-table palettes.
    const PLANK_SHADOW: [u8; 3] = [94, 58, 27];
    const PLANK: [u8; 3] = [153, 101, 49];
    const PLANK_LIGHT: [u8; 3] = [194, 139, 76];
    const PLANK_GRAIN: [u8; 3] = [117, 72, 31];
    const PLANK_HIGHLIGHT: [u8; 3] = [219, 170, 103];

    const COBBLE_MORTAR: [u8; 3] = [59, 63, 58];
    const COBBLE_DARK: [u8; 3] = [93, 103, 92];
    const COBBLE: [u8; 3] = [127, 139, 126];
    const COBBLE_LIGHT: [u8; 3] = [161, 173, 156];
    const COBBLE_HIGHLIGHT: [u8; 3] = [187, 199, 181];

    const TABLE_FRAME: [u8; 3] = [80, 49, 22];
    const TABLE_DIVIDER: [u8; 3] = [70, 42, 19];
    const TABLE_CELL: [u8; 3] = [129, 82, 38];
    const TABLE_STUD: [u8; 3] = [222, 176, 105];
    const TOOL_METAL: [u8; 3] = [126, 132, 136];
    const TOOL_METAL_DARK: [u8; 3] = [91, 97, 102];

    // v1.0 ore, furnace and chest palettes.
    const COAL_LOOK: OreLook = OreLook {
        dark: [48, 50, 54],
        mid: [76, 79, 84],
        light: [105, 108, 112],
        accent: None,
        clusters: 4,
        radius: 5,
        soft: 2,
    };
    const IRON_LOOK: OreLook = OreLook {
        dark: [118, 88, 54],
        mid: [184, 142, 94],
        light: [223, 182, 129],
        accent: None,
        clusters: 4,
        radius: 5,
        soft: 2,
    };
    const COPPER_LOOK: OreLook = OreLook {
        dark: [151, 70, 38],
        mid: [211, 106, 54],
        light: [238, 150, 88],
        accent: Some([80, 158, 122]),
        clusters: 4,
        radius: 5,
        soft: 2,
    };
    const FURNACE_LOOK: FurnaceLook = FurnaceLook {
        edge_dark: [94, 104, 96],
        edge_light: [176, 186, 170],
        seam: [86, 96, 88],
        metal: [138, 148, 150],
        metal_dark: [94, 102, 104],
        opening: [33, 34, 33],
        opening_inner: [21, 23, 23],
        ember_dark: [156, 65, 22],
        ember: [233, 119, 35],
        ember_hot: [252, 200, 90],
        trim: 2,
        soft: 1,
        dither: 1,
        rivets: true,
    };
    const CHEST_LOOK: ChestLook = ChestLook {
        seam: [76, 48, 22],
        seam_light: [219, 170, 103],
        metal: [134, 144, 146],
        metal_dark: [91, 99, 101],
        metal_light: [184, 194, 196],
        keyhole: [28, 24, 19],
        trim: 2,
        dither: 1,
    };

    /// Generates one lush layer.
    pub(super) fn layer(index: u32, name: &str, size: u32) -> Canvas {
        let seed = super::layer_seed(TexturePackStyle::Lush, index);
        match name {
            "grass_top" => grass_top(size, seed),
            "grass_side" => grass_side(size, seed),
            "dirt" => dirt(size, seed),
            "stone" => stone(size, seed),
            "sand" => sand(size, seed),
            "water" => water(size, seed),
            "log_side" => log_side(size, seed),
            "log_top" => log_top(size, seed),
            "leaves" => leaves(size, seed),
            "bedrock" => bedrock(size, seed),
            "gravel" => gravel(size, seed),
            "clay" => clay(size, seed),
            "snow" => snow(size, seed),
            "planks" => planks(size, seed),
            "cobblestone" => cobblestone(size, seed),
            "crafting_table_top" => crafting_table_top(size, seed),
            "crafting_table_side" => crafting_table_side(size, seed),
            "coal_ore" => coal_ore(size, seed),
            "iron_ore" => iron_ore(size, seed),
            "copper_ore" => copper_ore(size, seed),
            "furnace_top" => furnace_top(size, seed),
            "furnace_side" => furnace_side(size, seed),
            "furnace_front" => furnace_front(size, seed),
            "chest_top" => chest_top(size, seed),
            "chest_side" => chest_side(size, seed),
            "chest_front" => chest_front(size, seed),
            "torch" | "farmland_dry" | "farmland_wet" | "wheat_0" | "wheat_1" | "wheat_2" | "wheat_3"
            | "bed_foot_top" | "bed_foot_side" | "bed_head_top" | "bed_head_side" | "fire" => {
                super::extras::layer(TexturePackStyle::Lush, name, size, seed)
            }
            _ => debug(size),
        }
    }

    fn grass_color(seed: u32, x: u32, y: u32, size: u32) -> [u8; 3] {
        let broad = u32::from(periodic_noise(seed ^ 0x71, x, y, size, 4));
        let mid = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
        let fine = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 16));
        let value = (broad * 2 + mid * 3 + fine * 2) / 7;
        if value < 52 {
            GRASS_SHADOW
        } else if value < 100 {
            GRASS_DARK
        } else if value < 198 {
            GRASS
        } else if value < 236 {
            GRASS_LIGHT
        } else {
            GRASS_BRIGHT
        }
    }

    fn grass_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| grass_color(seed, x, y, size));
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x77, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x77, i, 1) % size) as i32;
            let lean = ((hash >> 12) % 3) as i32 - 1;
            let color = if hash.is_multiple_of(5) {
                GRASS_TUFT
            } else {
                GRASS_LIGHT
            };
            paint_blade(&mut canvas, x, y, 2, color, lean);
        }
        // Three tiny green clover specks, each a short diagonal.
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x91, i, 0);
            let x = (2 + hash % (size - 4)) as i32;
            let y = (2 + hash_noise(seed ^ 0x91, i, 1) % (size - 4)) as i32;
            canvas.set(x, y, CLOVER);
            canvas.set(x + 1, y + 1, offset(CLOVER, -10));
        }
        canvas
    }

    fn fringe_depth(seed: u32, x: u32, size: u32) -> u32 {
        let clump = hash_noise(seed ^ 0x35, x / 3, 1) % 3;
        let jag = u32::from(hash_noise(seed ^ 0x36, x, 2).is_multiple_of(3));
        (6 + clump + jag).min(size / 3)
    }

    fn grass_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = dirt(size, seed ^ 0x2211);
        for x in 0..size {
            let depth = fringe_depth(seed, x, size);
            for y in 0..depth {
                let soil = canvas.get(x as i32, y as i32);
                let grass = grass_color(seed ^ 0x5511, x, y, size);
                let t = ((y + 1) * 190 / depth).min(190);
                let mix = t * t / 256;
                canvas.set(x as i32, y as i32, lerp_color(grass, soil, mix));
            }
            if hash_noise(seed ^ 0x38, x, 3).is_multiple_of(3) {
                let tip = offset(grass_color(seed ^ 0x5511, x, depth, size), -14);
                canvas.set(x as i32, depth as i32, tip);
            }
            if hash_noise(seed ^ 0x39, x, 4).is_multiple_of(5) {
                let notch = offset(EARTH, (hash_noise(seed ^ 0x3a, x, 5) % 7) as i32 - 3);
                canvas.set(x as i32, depth as i32 - 1, notch);
            }
        }
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x99, i, 0);
            let x = (5 + hash % (size - 10)) as i32 + i as i32;
            let depth = fringe_depth(seed, x as u32, size) as i32;
            let length = 5 + i as i32 + (hash % 2) as i32;
            for step in 0..length {
                let px = x + if step > 0 && ((hash >> (step as u32)) & 1) == 1 {
                    1
                } else {
                    0
                };
                canvas.blend(px, depth + 1 + step, ROOT, (180 - step * 15).max(0) as u32);
            }
        }
        canvas
    }

    fn dirt(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x6d, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x71, x, y, size, 16));
            let value = (broad * 4 + mid * 2 + fine) / 7;
            if value < 62 {
                EARTH_SHADOW
            } else if value < 176 {
                EARTH
            } else if value < 224 {
                EARTH_LIGHT
            } else {
                offset(EARTH_LIGHT, 8)
            }
        });
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x44, i, 0);
            let x = (2 + hash % (size - 6)) as i32;
            let y = (size / 3 + hash_noise(seed ^ 0x44, i, 1) % (size - size / 3 - 5)) as i32;
            if i == 0 {
                canvas.set(x, y, EARTH_PEBBLE);
                canvas.blend(x + 1, y + 1, EARTH_SHADOW, 110);
            } else {
                canvas.set(x, y, EARTH_PEBBLE);
                canvas.set(x + 1, y, offset(EARTH_PEBBLE, -12));
                canvas.blend(x, y + 1, EARTH_SHADOW, 140);
                canvas.blend(x + 1, y + 1, EARTH_SHADOW, 100);
            }
        }
        canvas
    }

    fn stone(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x77, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 16));
            let value = (broad * 4 + mid * 3 + fine) / 8;
            if value < 62 {
                STONE_SHADOW
            } else if value > 198 {
                STONE_LIGHT
            } else {
                STONE
            }
        });
        // Moss gathers in irregular low-noise pockets, not painted circles.
        for y in 0..size {
            for x in 0..size {
                let moss = u32::from(fbm(seed ^ 0x5a, x, y, size, 2, 3));
                if moss > 205 {
                    canvas.blend(x as i32, y as i32, MOSS, 150);
                } else if moss > 188 {
                    canvas.blend(x as i32, y as i32, MOSS, 70);
                } else if moss > 180 && hash_noise(seed ^ 0x5c, x, y) & 1 == 0 {
                    canvas.set(x as i32, y as i32, MOSS_LIGHT);
                }
            }
        }
        paint_crack(&mut canvas, seed ^ 0x31, 7, 5, 11, 1, 1, STONE_CRACK, STONE_HIGHLIGHT);
        paint_crack(&mut canvas, seed ^ 0x32, 25, 9, 7, -1, 1, STONE_CRACK, STONE_HIGHLIGHT);
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                if i % 2 == 0 { STONE_HIGHLIGHT } else { STONE_CRACK },
            );
        }
        canvas
    }

    fn sand(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x9d, x, y, size, 8));
            let fine = u32::from(periodic_noise(seed ^ 0x9f, x, y, size, 16));
            let value = (broad * 4 + mid * 2 + fine) / 7;
            if value < 62 {
                SAND_SHADOW
            } else if value < 178 {
                SAND
            } else if value < 228 {
                SAND_LIGHT
            } else {
                SHELL
            }
        });
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x5a, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x5a, i, 1) % size) as i32;
            let color = if i % 3 == 0 { SHELL_PINK } else { SHELL };
            canvas.set(x, y, color);
            if i % 2 == 0 {
                canvas.blend(x + 1, y + 1, SAND_SHADOW, 90);
            }
        }
        canvas
    }

    fn water_wave(seed: u32, x: u32, y: u32, size: u32) -> u32 {
        let wobble = (fbm(seed ^ 0x55, x, y, size, 4, 2) as i32 - 128) / 26;
        let period = 12u32;
        let phase = (y as i32 + wobble).rem_euclid(period as i32) as u32;
        smooth_fixed(wave01(phase, period))
    }

    fn water(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let wave = water_wave(seed, x, y, size);
            let base = lerp_color(WATER_DEEP, WATER, wave);
            let light = (wave.saturating_sub(200) * 2).min(150);
            lerp_color(base, WATER_LIGHT, light)
        });
        // Foam only on short stretches of the local crest: clustered, not speckled.
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x71, i, 0);
            let x = (2 + hash % (size - 8)) as i32;
            let mut crest_y = 0;
            let mut crest = 0;
            for y in 0..size {
                let wave = water_wave(seed, x as u32, y, size);
                if wave > crest {
                    crest = wave;
                    crest_y = y as i32;
                }
            }
            for dx in 0..3 {
                canvas.blend(x + dx, crest_y, FOAM, 170 - (dx as u32) * 30);
            }
            canvas.blend(x + 1, crest_y + 1, FOAM, 60);
        }
        canvas
    }

    fn bark_ridge(seed: u32, x: u32, size: u32) -> u32 {
        let broad = u32::from(periodic_noise(seed, x, 0, size, 4));
        let fine = u32::from(periodic_noise(seed ^ 0x11, x, 0, size, 8));
        (broad * 3 + fine) / 4
    }

    fn log_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let ridge = bark_ridge(seed, x, size);
            let base = if ridge < 52 {
                BARK_GROOVE
            } else if ridge < 84 {
                offset(BARK, -12)
            } else if ridge > 214 {
                BARK_LIGHT
            } else {
                BARK
            };
            let grain = (hash_noise(seed ^ 0x31, x, y / 4) % 9) as i32 - 4;
            let fleck = if hash_noise(seed ^ 0x51, x, y / 9).is_multiple_of(23) {
                12
            } else {
                0
            };
            offset(base, grain + fleck)
        });
        paint_ellipse(&mut canvas, 8, 22, 4, 3, KNOT, 1);
        paint_ellipse(&mut canvas, 8, 22, 2, 1, BARK, 0);
        canvas.blend(7, 21, BARK_HIGHLIGHT, 70);
        canvas.blend(11, 24, KNOT, 110);
        for dy in -5i32..=5 {
            if dy.abs() > 2 {
                canvas.blend(4, 22 + dy, BARK_GROOVE, 80);
            }
        }
        paint_ellipse(&mut canvas, 25, 7, 3, 2, KNOT, 1);
        paint_ellipse(&mut canvas, 25, 7, 1, 1, BARK, 0);
        canvas.blend(24, 6, BARK_HIGHLIGHT, 60);
        canvas.blend(27, 9, KNOT, 90);
        canvas
    }

    fn log_top(size: u32, seed: u32) -> Canvas {
        let centre_x = (size * 12 / 32) as i32;
        let centre_y = (size * 13 / 32) as i32;
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let dx = 2 * (x as i32 - centre_x);
            let dy = 2 * (y as i32 - centre_y);
            let wobble = (fbm(seed ^ 0x12, x, y, size, 4, 2) as i32 - 128) / 28;
            let distance = isqrt((dx * dx + dy * dy) as u32) as i32 + wobble;
            let ring = (distance / 4).rem_euclid(2) == 0;
            let base = if distance < 7 {
                RING_HEART
            } else if ring {
                RING_LIGHT
            } else {
                RING_DARK
            };
            let rim = x < 2 || y < 2 || x >= size - 2 || y >= size - 2;
            if rim {
                let grain = (hash_noise(seed ^ 0x33, x, y / 5) % 9) as i32 - 4;
                offset(BARK_RIM, grain)
            } else {
                offset(base, (hash_noise(seed, x, y) % 3) as i32 - 1)
            }
        });
        for (index, (dx, dy)) in [(1, -1), (1, 1), (-1, 1)].into_iter().enumerate() {
            for step in 4..(size as i32 / 2 - 2) {
                let x = centre_x + step * dx;
                let y = centre_y + step * dy;
                if x > 1 && x < size as i32 - 2 && y > 1 && y < size as i32 - 2 {
                    canvas.blend(x, y, RING_HEART, if index == 0 { 110 } else { 70 });
                }
            }
        }
        canvas
    }

    fn leaves(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 2));
            let mid = u32::from(periodic_noise(seed ^ 0x22, x, y, size, 4));
            let fine = u32::from(periodic_noise(seed ^ 0x5b, x, y, size, 8));
            let value = (broad * 3 + mid * 3 + fine * 2) / 8;
            let pocket = u32::from(fbm(seed ^ 0x4d, x, y, size, 4, 2));
            let base = if value < 50 {
                LEAF_SHADOW
            } else if value < 106 {
                LEAF_DARK
            } else if value < 174 {
                LEAF
            } else if value < 224 {
                LEAF_LIGHT
            } else {
                LEAF_PALE
            };
            if pocket < 70 {
                lerp_color(base, LEAF_SHADOW, 90)
            } else {
                base
            }
        });
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x55, i, 0);
            let x = (hash % size) as i32;
            let y = (hash_noise(seed ^ 0x55, i, 1) % size) as i32;
            canvas.set(x, y, LEAF_TIP);
            if hash & 1 == 0 {
                canvas.blend(x + 1, y - 1, LEAF_TIP, 140);
            }
        }
        punch_leaf_holes(&mut canvas, seed ^ 0x7a, 16);
        canvas
    }

    fn bedrock(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 4);
            let edge = sample.second - sample.nearest;
            let broad = u32::from(fbm(seed ^ 0x31, x, y, size, 2, 3));
            let base = if broad < 70 {
                BEDROCK_SHADOW
            } else if broad > 196 {
                BEDROCK_LIGHT
            } else {
                BEDROCK
            };
            if edge < 7 {
                lerp_color(base, BEDROCK_SHADOW, 150)
            } else if edge < 15 && hash_noise(seed ^ 0x51, x, y) & 1 == 0 {
                lerp_color(base, BEDROCK_LIGHT, 110)
            } else {
                base
            }
        });
        for i in 0..5u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                BEDROCK_LIGHT,
            );
        }
        canvas
    }

    fn gravel(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, GRAVEL_GAP);
        let palette = [GRAVEL_GREY, GRAVEL_BEIGE, GRAVEL_WARM, GRAVEL_COOL, GRAVEL_PALE];
        for pebble in scatter_pebbles(seed, size, 380, 2, 3) {
            let color = palette[(pebble.id >> 8) as usize % palette.len()];
            paint_gravel_pebble(&mut canvas, &pebble, color, 12, -12, 0);
        }
        canvas
    }

    fn clay(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x23, x, y, size, 8));
            let value = (broad * 5 + mid * 3) / 8;
            if value < 70 {
                CLAY_SHADOW
            } else if value > 198 {
                CLAY_LIGHT
            } else {
                CLAY
            }
        });
        for (index, base_y) in [size / 3, size * 2 / 3].into_iter().enumerate() {
            for x in 0..size {
                let wobble = (periodic_noise(seed ^ 0x31, x, 0, size, 8) as i32 - 128) / 34;
                let y = base_y as i32 + index as i32 + wobble;
                canvas.blend(x as i32, y, CLAY_SHADOW, 70);
                canvas.blend(x as i32, y + 1, CLAY_LIGHT, 40);
            }
        }
        for i in 0..4u32 {
            let hash = hash_noise(seed ^ 0x2f, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x2f, i, 1) % size) as i32,
                CLAY_LIGHT,
            );
        }
        canvas
    }

    fn snow(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 3));
            let mid = u32::from(periodic_noise(seed ^ 0x47, x, y, size, 8));
            let value = (broad * 5 + mid * 3) / 8;
            let base = lerp_color(SNOW_SHADOW, SNOW, value);
            let glint = if hash_noise(seed ^ 0x34, x, y).is_multiple_of(97) {
                3
            } else {
                0
            };
            offset(base, glint + dither(x, y, 1))
        })
    }

    fn planks(size: u32, seed: u32) -> Canvas {
        let board_height = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let board = y / board_height;
            let row = y % board_height;
            let broad = u32::from(fbm(seed ^ 0x21, x, y, size, 2, 3));
            let fine = u32::from(periodic_noise(seed ^ 0x33, x, y, size, 16));
            let value = (broad * 4 + fine * 2) / 6;
            let base = if value < 56 {
                PLANK_SHADOW
            } else if value < 128 {
                PLANK
            } else if value < 210 {
                PLANK_LIGHT
            } else {
                PLANK_HIGHLIGHT
            };
            let grain = (periodic_noise(seed ^ 0x44, x / 2, y, size, 4) as i32 - 128) / 48;
            let tone = (hash_noise(seed ^ 0x11, board, 0) % 5) as i32 - 2;
            if row == 0 {
                offset(PLANK_SHADOW, tone)
            } else if row + 1 == board_height {
                offset(PLANK_HIGHLIGHT, tone)
            } else {
                offset(base, tone + grain)
            }
        });
        // Deep knots and short butt joints with a lit edge.
        for board in 0..4u32 {
            let joint = 3 + (hash_noise(seed ^ 0x55, board, 1) % (size - 6).max(1)) as i32;
            for y in board * board_height..(board + 1) * board_height {
                canvas.set(joint, y as i32, PLANK_SHADOW);
                canvas.blend(joint + 1, y as i32, PLANK_HIGHLIGHT, 90);
            }
        }
        paint_ellipse(
            &mut canvas,
            (size * 3 / 4) as i32,
            (size / 5) as i32,
            2,
            1,
            PLANK_GRAIN,
            1,
        );
        paint_ellipse(
            &mut canvas,
            (size / 5) as i32,
            (size * 4 / 5) as i32,
            2,
            1,
            PLANK_GRAIN,
            1,
        );
        canvas
    }

    fn cobblestone(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 4);
            let edge = sample.second - sample.nearest;
            let centre = isqrt(sample.nearest) as i32;
            let grain = (hash_noise(seed ^ 0x31, x, y) % 5) as i32 - 2;
            let stone = if centre == 0 {
                COBBLE_HIGHLIGHT
            } else if centre <= 1 {
                COBBLE_LIGHT
            } else if centre <= 4 {
                COBBLE
            } else {
                COBBLE_DARK
            };
            if edge < 8 {
                offset(COBBLE_MORTAR, grain)
            } else {
                offset(stone, grain)
            }
        });
        // Moss settles into the mortar band and along the stone rims.
        for y in 0..size {
            for x in 0..size {
                let sample = cellular(seed, x, y, size, 4);
                let edge = sample.second - sample.nearest;
                let moss = u32::from(fbm(seed ^ 0x5a, x, y, size, 4, 2));
                if edge < 10 && moss > 150 {
                    canvas.blend(x as i32, y as i32, MOSS, 130);
                } else if edge < 16 && moss > 208 {
                    canvas.blend(x as i32, y as i32, MOSS_LIGHT, 70);
                }
            }
        }
        canvas
    }

    fn crafting_table_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = planks(size, seed ^ 0x1a2b);
        paint_crafting_grid(
            &mut canvas,
            TABLE_FRAME,
            TABLE_DIVIDER,
            TABLE_CELL,
            PLANK_HIGHLIGHT,
            TABLE_STUD,
            2,
            2,
        );
        canvas
    }

    fn crafting_table_side(size: u32, seed: u32) -> Canvas {
        let lip = (size / 5).max(4);
        let board_width = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |x, y| {
            if y < lip {
                let value = u32::from(fbm(seed ^ 0x71, x, y, size, 4, 3));
                let base = lerp_color(PLANK_LIGHT, PLANK, value);
                if y == 0 {
                    PLANK_HIGHLIGHT
                } else if y + 1 == lip {
                    PLANK_SHADOW
                } else {
                    offset(base, dither(x, y, 1))
                }
            } else {
                let board = x / board_width;
                let tone = (hash_noise(seed ^ 0x73, board, 0) % 3) as i32 - 1;
                let broad = u32::from(fbm(seed ^ 0x73, x, y, size, 2, 3));
                let grain = (periodic_noise(seed ^ 0x77, x, y / 2, size, 8) as i32 - 128) / 40;
                let base = ramp3(PLANK_SHADOW, PLANK, PLANK_LIGHT, broad);
                if x % board_width == 0 {
                    PLANK_SHADOW
                } else {
                    offset(base, tone + grain)
                }
            }
        });
        paint_table_tool(
            &mut canvas,
            (size as i32 * 3) / 5,
            (size as i32 * 7) / 10,
            TOOL_METAL,
            TOOL_METAL_DARK,
            PLANK_GRAIN,
            PLANK_HIGHLIGHT,
            (size / 16).max(1) as i32,
        );
        // Shelf shadow, two studs and a short hanging saw blade.
        for x in 0..size {
            canvas.blend(x as i32, (lip + size / 6) as i32, PLANK_SHADOW, 90);
        }
        for dx in 0..5 {
            canvas.set((size as i32 / 5) + dx, lip as i32 + 3, TOOL_METAL);
            canvas.set((size as i32 / 5) + dx, lip as i32 + 4, TOOL_METAL_DARK);
        }
        canvas.set(4, lip as i32 + 2, TABLE_STUD);
        canvas.set(size as i32 - 5, lip as i32 + 2, TABLE_STUD);
        canvas
    }

    fn coal_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x41);
        paint_ore(&mut canvas, seed ^ 0x51, &COAL_LOOK);
        canvas
    }

    fn iron_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x42);
        paint_ore(&mut canvas, seed ^ 0x52, &IRON_LOOK);
        canvas
    }

    fn copper_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x43);
        paint_ore(&mut canvas, seed ^ 0x53, &COPPER_LOOK);
        canvas
    }

    fn furnace_top(size: u32, seed: u32) -> Canvas {
        furnace_top_from(stone(size, seed ^ 0x61), seed, &FURNACE_LOOK)
    }

    fn furnace_side(size: u32, seed: u32) -> Canvas {
        furnace_side_from(stone(size, seed ^ 0x62), seed, &FURNACE_LOOK)
    }

    fn furnace_front(size: u32, seed: u32) -> Canvas {
        furnace_front_from(stone(size, seed ^ 0x63), seed, &FURNACE_LOOK)
    }

    fn chest_top(size: u32, seed: u32) -> Canvas {
        chest_top_from(planks(size, seed ^ 0x71), seed, &CHEST_LOOK)
    }

    fn chest_side(size: u32, seed: u32) -> Canvas {
        chest_side_from(planks(size, seed ^ 0x72), seed, &CHEST_LOOK)
    }

    fn chest_front(size: u32, seed: u32) -> Canvas {
        chest_front_from(planks(size, seed ^ 0x73), seed, &CHEST_LOOK)
    }

    fn debug(size: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let cell = (x / 4 + y / 4).is_multiple_of(2);
            let base = if cell { [72, 206, 180] } else { [38, 38, 46] };
            let glint = if hash_noise(0x51ed, x, y).is_multiple_of(11) {
                22
            } else {
                0
            };
            offset(base, glint)
        })
    }
}

/// Minimal 16² pack: flat modern design with natural (not minty) colours, two
/// or three crisp tones per block and a few one-pixel accent lines.
mod minimal {
    use super::*;

    const GRASS_DARK: [u8; 3] = [103, 164, 76];
    const GRASS: [u8; 3] = [111, 174, 82];
    const GRASS_LIGHT: [u8; 3] = [120, 183, 92];

    const DIRT_DARK: [u8; 3] = [139, 104, 73];
    const DIRT: [u8; 3] = [157, 121, 87];
    const DIRT_LIGHT: [u8; 3] = [174, 137, 101];
    const DIRT_PEBBLE: [u8; 3] = [180, 146, 111];
    const DIRT_SHADOW: [u8; 3] = [117, 85, 58];
    const ROOT: [u8; 3] = [95, 68, 45];

    const STONE_LIGHT: [u8; 3] = [199, 201, 207];
    const STONE: [u8; 3] = [184, 186, 193];
    const STONE_SEAM: [u8; 3] = [158, 160, 168];
    const STONE_ACCENT: [u8; 3] = [170, 172, 180];

    const SAND_SHADOW: [u8; 3] = [231, 218, 182];
    const SAND: [u8; 3] = [242, 232, 202];
    const SAND_ACCENT: [u8; 3] = [250, 243, 220];

    const WATER_DEEP: [u8; 3] = [76, 178, 198];
    const WATER: [u8; 3] = [92, 198, 214];
    const WATER_BAND: [u8; 3] = [124, 216, 228];

    const WOOD_KNOT: [u8; 3] = [147, 107, 65];
    const WOOD_DARK: [u8; 3] = [169, 127, 76];
    const WOOD: [u8; 3] = [195, 154, 98];
    const WOOD_LIGHT: [u8; 3] = [211, 171, 116];
    const BARK_RIM: [u8; 3] = [160, 118, 69];

    const LEAF_DARK: [u8; 3] = [82, 148, 66];
    const LEAF: [u8; 3] = [93, 163, 74];
    const LEAF_LIGHT: [u8; 3] = [105, 173, 84];

    const BEDROCK_DARK: [u8; 3] = [42, 43, 50];
    const BEDROCK: [u8; 3] = [59, 61, 69];
    const BEDROCK_LIGHT: [u8; 3] = [86, 89, 98];

    const GRAVEL_GAP: [u8; 3] = [142, 138, 131];
    const GRAVEL: [u8; 3] = [180, 176, 168];
    const GRAVEL_LIGHT: [u8; 3] = [194, 190, 182];
    const GRAVEL_BEIGE: [u8; 3] = [190, 178, 160];

    const CLAY_DARK: [u8; 3] = [174, 189, 203];
    const CLAY: [u8; 3] = [188, 202, 214];
    const CLAY_LIGHT: [u8; 3] = [201, 213, 223];

    const SNOW_SHADOW: [u8; 3] = [240, 246, 252];
    const SNOW: [u8; 3] = [250, 251, 253];

    // v0.0.8 wood, cobblestone and crafting-table accents.
    const COBBLE_SEAM: [u8; 3] = [151, 153, 160];
    const COBBLE: [u8; 3] = [184, 186, 193];
    const COBBLE_LIGHT: [u8; 3] = [199, 201, 207];
    const TABLE_FRAME: [u8; 3] = [169, 127, 76];
    const TABLE_DIVIDER: [u8; 3] = [147, 107, 65];
    const TABLE_CELL: [u8; 3] = [180, 139, 85];
    const TOOL_METAL: [u8; 3] = [158, 160, 168];

    // v1.0 ore, furnace and chest palettes.
    const COAL_LOOK: [u8; 3] = [55, 57, 62];
    const IRON_LOOK: [u8; 3] = [191, 156, 110];
    const IRON_SHADE: [u8; 3] = [160, 122, 82];
    const COPPER_LOOK: [u8; 3] = [219, 127, 78];
    const COPPER_PATINA: [u8; 3] = [98, 164, 142];
    const FURNACE_LOOK: FurnaceLook = FurnaceLook {
        edge_dark: [151, 153, 160],
        edge_light: [214, 216, 222],
        seam: [151, 153, 160],
        metal: [160, 162, 170],
        metal_dark: [126, 128, 136],
        opening: [67, 69, 76],
        opening_inner: [48, 50, 56],
        ember_dark: [214, 124, 52],
        ember: [244, 166, 66],
        ember_hot: [255, 214, 112],
        trim: 1,
        soft: 0,
        dither: 0,
        rivets: true,
    };
    const CHEST_LOOK: ChestLook = ChestLook {
        seam: [147, 107, 65],
        seam_light: [211, 171, 116],
        metal: [160, 162, 170],
        metal_dark: [126, 128, 136],
        metal_light: [199, 201, 207],
        keyhole: [95, 68, 45],
        trim: 1,
        dither: 0,
    };

    /// Generates one minimal layer.
    pub(super) fn layer(index: u32, name: &str, size: u32) -> Canvas {
        let seed = super::layer_seed(TexturePackStyle::Minimal, index);
        match name {
            "grass_top" => grass_top(size, seed),
            "grass_side" => grass_side(size, seed),
            "dirt" => dirt(size, seed),
            "stone" => stone(size, seed),
            "sand" => sand(size, seed),
            "water" => water(size),
            "log_side" => log_side(size),
            "log_top" => log_top(size),
            "leaves" => leaves(size, seed),
            "bedrock" => bedrock(size, seed),
            "gravel" => gravel(size, seed),
            "clay" => clay(size),
            "snow" => snow(size, seed),
            "planks" => planks(size, seed),
            "cobblestone" => cobblestone(size, seed),
            "crafting_table_top" => crafting_table_top(size, seed),
            "crafting_table_side" => crafting_table_side(size, seed),
            "coal_ore" => coal_ore(size, seed),
            "iron_ore" => iron_ore(size, seed),
            "copper_ore" => copper_ore(size, seed),
            "furnace_top" => furnace_top(size, seed),
            "furnace_side" => furnace_side(size, seed),
            "furnace_front" => furnace_front(size, seed),
            "chest_top" => chest_top(size, seed),
            "chest_side" => chest_side(size, seed),
            "chest_front" => chest_front(size, seed),
            "torch" | "farmland_dry" | "farmland_wet" | "wheat_0" | "wheat_1" | "wheat_2" | "wheat_3"
            | "bed_foot_top" | "bed_foot_side" | "bed_head_top" | "bed_head_side" | "fire" => {
                super::extras::layer(TexturePackStyle::Minimal, name, size, seed)
            }
            _ => debug(size),
        }
    }

    /// Scatters flat single- and double-texel mineral patches over stone.
    ///
    /// Minimal deliberately avoids soft blobs: the ore read comes from a handful
    /// of crisp black/gold/copper pixels and the occasional patina accent.
    fn flat_ore(canvas: &mut Canvas, seed: u32, patches: u32, color: [u8; 3], accent: Option<[u8; 3]>) {
        let size = canvas.size;
        let span = size.saturating_sub(2).max(1);
        for patch in 0..patches {
            let hash = hash_noise(seed ^ 0x11, patch, 0);
            let x = 1 + (hash % span) as i32;
            let y = 1 + (hash_noise(seed ^ 0x12, patch, 1) % span) as i32;
            canvas.set(x, y, color);
            match hash % 3 {
                0 => {
                    canvas.set(x + 1, y, color);
                    canvas.set(x, y + 1, color);
                }
                1 => canvas.set(x, y + 1, color),
                _ => canvas.set(x + 1, y, color),
            }
            if let Some(accent) = accent
                && hash.is_multiple_of(2)
            {
                canvas.set(x + 2, y + 1, accent);
                canvas.set(x + 1, y + 2, accent);
            }
        }
        // A few lone flecks bridge the flat patches.
        for fleck in 0..patches {
            let x = (hash_noise(seed ^ 0x21, fleck, 0) % size) as i32;
            let y = (hash_noise(seed ^ 0x22, fleck, 0) % size) as i32;
            canvas.set(x, y, accent.unwrap_or(color));
        }
    }

    /// Adds sparse ±`amount` tone variation (about one texel in `frequency`).
    ///
    /// Minimal surfaces are deliberately flat, but a touch of grain keeps the
    /// same few pixels from locking into a visible grid when a block texture
    /// repeats across a large wall or field.
    fn speckle(canvas: &mut Canvas, seed: u32, amount: i32, frequency: u32) {
        let size = canvas.size;
        for y in 0..size {
            for x in 0..size {
                let hash = hash_noise(seed, x, y);
                if hash.is_multiple_of(frequency) {
                    let delta = if hash & 0x100 == 0 { amount } else { -amount };
                    let color = canvas.get(x as i32, y as i32);
                    canvas.set(x as i32, y as i32, offset(color, delta));
                }
            }
        }
    }

    /// Three close green tones, with only rare extreme patches.
    fn grass_color(seed: u32, x: u32, y: u32, size: u32) -> [u8; 3] {
        let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
        let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
        let value = (fine * 2 + mid) / 3;
        if value < 62 {
            GRASS_DARK
        } else if value > 208 {
            GRASS_LIGHT
        } else {
            GRASS
        }
    }

    fn grass_top(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| grass_color(seed, x, y, size));
        speckle(&mut canvas, seed ^ 0x9b, 1, 29);
        // One single-texel highlight: minimal grass stays a clean flat field.
        let hash = hash_noise(seed ^ 0x77, 0, 0);
        canvas.set(
            (hash % size) as i32,
            (hash_noise(seed ^ 0x77, 0, 1) % size) as i32,
            GRASS_LIGHT,
        );
        canvas
    }

    fn fringe_depth(seed: u32, x: u32) -> u32 {
        let clump = hash_noise(seed ^ 0x35, x / 2, 1) % 2;
        let jag = u32::from(hash_noise(seed ^ 0x36, x, 2).is_multiple_of(3));
        3 + clump + jag
    }

    fn grass_side(size: u32, seed: u32) -> Canvas {
        let mut canvas = dirt(size, seed ^ 0x2211);
        for x in 0..size {
            let depth = fringe_depth(seed, x);
            for y in 0..depth {
                let grass = grass_color(seed ^ 0x5511, x, y, size);
                canvas.set(x as i32, y as i32, grass);
            }
            // One-pixel ragged tips break the lower fringe edge.
            if hash_noise(seed ^ 0x38, x, 3).is_multiple_of(4) {
                canvas.set(x as i32, depth as i32, GRASS_DARK);
            }
        }
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x99, i, 0);
            let x = (4 + hash % (size - 8)) as i32 + i as i32;
            let depth = fringe_depth(seed, x as u32) as i32;
            let length = 3 + i as i32;
            for step in 0..length {
                let px = x + if step > 0 && ((hash >> (step as u32)) & 1) == 1 {
                    1
                } else {
                    0
                };
                canvas.blend(px, depth + 1 + step, ROOT, (150 - step * 20).max(0) as u32);
            }
        }
        canvas
    }

    fn dirt(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
            let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
            let value = (fine * 2 + mid) / 3;
            if value < 58 {
                DIRT_DARK
            } else if value > 214 {
                DIRT_LIGHT
            } else {
                DIRT
            }
        });
        speckle(&mut canvas, seed ^ 0x9b, 2, 13);
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x44, i, 0);
            let x = (3 + hash % (size - 6)) as i32;
            let y = (size / 3 + hash_noise(seed ^ 0x44, i, 1) % (size - size / 3 - 4)) as i32;
            canvas.set(x, y, DIRT_PEBBLE);
            canvas.set(x + 1, y, DIRT_LIGHT);
            canvas.blend(x, y + 1, DIRT_SHADOW, 150);
            canvas.blend(x + 1, y + 1, DIRT_SHADOW, 110);
        }
        canvas
    }

    fn stone(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
            let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
            let value = (fine * 2 + mid) / 3;
            if value > 202 { STONE_LIGHT } else { STONE }
        });
        speckle(&mut canvas, seed ^ 0x9b, 3, 17);
        paint_crack(&mut canvas, seed ^ 0x31, 4, 4, 6, 1, 1, STONE_SEAM, STONE_LIGHT);
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x66, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x66, i, 1) % size) as i32,
                STONE_ACCENT,
            );
        }
        canvas
    }

    fn sand(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
            let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
            let value = (fine * 2 + mid) / 3;
            if value < 62 { SAND_SHADOW } else { SAND }
        });
        speckle(&mut canvas, seed ^ 0x9b, 2, 13);
        for i in 0..2u32 {
            let hash = hash_noise(seed ^ 0x5a, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x5a, i, 1) % size) as i32,
                SAND_ACCENT,
            );
        }
        canvas
    }

    fn water(size: u32) -> Canvas {
        let mut canvas = Canvas::new(size, WATER);
        // One crisp, flat band: the entire water read comes from it.
        for x in 0..size {
            for y in [size / 2, size / 2 + 1] {
                canvas.set(x as i32, y as i32, WATER_BAND);
            }
        }
        for x in 0..size {
            canvas.set(x as i32, size as i32 / 2 + 2, WATER_DEEP);
        }
        canvas
    }

    fn log_side(size: u32) -> Canvas {
        let mut canvas = Canvas::new(size, WOOD);
        for x in [3i32, 8, 13] {
            for y in 0..size {
                canvas.set(x, y as i32, WOOD_DARK);
            }
        }
        for y in 0..size {
            canvas.set(4, y as i32, WOOD_LIGHT);
        }
        canvas.set(11, 10, WOOD_KNOT);
        canvas.set(11, 11, WOOD_LIGHT);
        canvas
    }

    fn log_top(size: u32) -> Canvas {
        let centre_x = (size * 6 / 16) as i32;
        let centre_y = (size * 5 / 16) as i32;
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let dx = 2 * (x as i32 - centre_x);
            let dy = 2 * (y as i32 - centre_y);
            let distance = isqrt((dx * dx + dy * dy) as u32) as i32;
            if distance < 5 {
                WOOD_LIGHT
            } else if (distance / 4).rem_euclid(2) == 0 {
                WOOD
            } else {
                WOOD_DARK
            }
        });
        for x in 0..size {
            canvas.set(x as i32, 0, BARK_RIM);
            canvas.set(x as i32, 1, BARK_RIM);
            canvas.set(x as i32, size as i32 - 1, BARK_RIM);
            canvas.set(0, x as i32, BARK_RIM);
            canvas.set(1, x as i32, BARK_RIM);
            canvas.set(size as i32 - 1, x as i32, BARK_RIM);
        }
        for step in 3..(size as i32 / 2) {
            let x = centre_x + step;
            let y = centre_y + step / 2;
            if x > 1 && x < size as i32 - 2 && y > 1 && y < size as i32 - 2 {
                canvas.set(x, y, WOOD_KNOT);
            }
        }
        canvas
    }

    fn leaves(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::from_fn(size, |x, y| {
            let fine = u32::from(periodic_noise(seed ^ 0x3d, x, y, size, 8));
            let mid = u32::from(periodic_noise(seed ^ 0x4f, x, y, size, 4));
            let value = (fine * 2 + mid) / 3;
            if value < 60 {
                LEAF_DARK
            } else if value > 212 {
                LEAF_LIGHT
            } else {
                LEAF
            }
        });
        speckle(&mut canvas, seed ^ 0x9b, 2, 13);
        for i in 0..3u32 {
            let hash = hash_noise(seed ^ 0x55, i, 0);
            canvas.set(
                (hash % size) as i32,
                (hash_noise(seed ^ 0x55, i, 1) % size) as i32,
                LEAF_LIGHT,
            );
        }
        punch_leaf_holes(&mut canvas, seed ^ 0x7a, 5);
        canvas
    }

    fn bedrock(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let sample = cellular(seed, x, y, size, 2);
            let edge = sample.second - sample.nearest;
            let broad = u32::from(fbm(seed ^ 0x31, x, y, size, 2, 2));
            let base = if broad > 162 { BEDROCK_LIGHT } else { BEDROCK };
            if edge < 4 {
                BEDROCK_DARK
            } else if edge < 7 && hash_noise(seed ^ 0x51, x, y) & 1 == 0 {
                BEDROCK_LIGHT
            } else {
                base
            }
        })
    }

    fn gravel(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, GRAVEL_GAP);
        let palette = [GRAVEL, GRAVEL_LIGHT, GRAVEL_BEIGE];
        for pebble in scatter_pebbles(seed, size, 140, 1, 2) {
            let color = palette[(pebble.id >> 8) as usize % palette.len()];
            paint_gravel_pebble(&mut canvas, &pebble, color, 0, -12, 0);
        }
        canvas
    }

    fn clay(size: u32) -> Canvas {
        let mut canvas = Canvas::new(size, CLAY);
        for x in 0..size {
            for y in [size / 4, size / 4 + 1] {
                canvas.set(x as i32, y as i32, CLAY_LIGHT);
            }
            for y in [size * 3 / 4, size * 3 / 4 + 1] {
                canvas.set(x as i32, y as i32, CLAY_DARK);
            }
        }
        canvas
    }

    fn snow(size: u32, seed: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let broad = u32::from(fbm(seed, x, y, size, 2, 2));
            if broad < 82 { SNOW_SHADOW } else { SNOW }
        })
    }

    fn planks(size: u32, seed: u32) -> Canvas {
        let board_height = (size / 4).max(1);
        let mut canvas = Canvas::from_fn(size, |_x, y| {
            let board = y / board_height;
            let row = y % board_height;
            let tone = if hash_noise(seed ^ 0x11, board, 0) & 1 == 0 {
                WOOD
            } else {
                WOOD_LIGHT
            };
            if row == 0 {
                WOOD_DARK
            } else if row == 1 {
                offset(WOOD_LIGHT, 8)
            } else {
                tone
            }
        });
        // One short butt joint per board and a whisper of grain.
        for board in 0..4u32 {
            let joint = 2 + (hash_noise(seed ^ 0x55, board, 1) % (size - 4).max(1)) as i32;
            canvas.set(joint, (board * board_height + 1) as i32, WOOD_KNOT);
            canvas.set(joint, (board * board_height + 2) as i32, WOOD_KNOT);
        }
        speckle(&mut canvas, seed ^ 0x9b, 1, 23);
        canvas
    }

    fn cobblestone(size: u32, seed: u32) -> Canvas {
        let mut canvas = paint_cobblestone(
            size,
            seed,
            COBBLE_SEAM,
            &[COBBLE, COBBLE_LIGHT, COBBLE],
            80,
            2,
            3,
            0,
            -10,
            0,
        );
        speckle(&mut canvas, seed ^ 0x9b, 1, 29);
        canvas
    }

    fn crafting_table_top(size: u32, _seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, WOOD);
        paint_crafting_grid(
            &mut canvas,
            TABLE_FRAME,
            TABLE_DIVIDER,
            TABLE_CELL,
            WOOD_LIGHT,
            WOOD_LIGHT,
            1,
            1,
        );
        canvas
    }

    fn crafting_table_side(size: u32, _seed: u32) -> Canvas {
        let lip = (size / 5).max(3);
        let mut canvas = Canvas::new(size, WOOD);
        for y in 0..lip {
            for x in 0..size {
                canvas.set(x as i32, y as i32, if y == 0 { WOOD_LIGHT } else { WOOD_DARK });
            }
        }
        for x in [size / 4, size / 2, size * 3 / 4] {
            for y in lip..size {
                canvas.set(x as i32, y as i32, WOOD_DARK);
            }
        }
        paint_table_tool(
            &mut canvas,
            (size as i32 * 3) / 5,
            (size as i32 * 7) / 10,
            TOOL_METAL,
            STONE_SEAM,
            WOOD_KNOT,
            WOOD_LIGHT,
            (size / 16).max(1) as i32,
        );
        canvas.set(2, lip as i32 + 1, WOOD_LIGHT);
        canvas.set(size as i32 - 3, lip as i32 + 1, WOOD_LIGHT);
        canvas
    }

    fn coal_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x41);
        flat_ore(&mut canvas, seed ^ 0x51, 7, COAL_LOOK, None);
        canvas
    }

    fn iron_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x42);
        flat_ore(&mut canvas, seed ^ 0x52, 6, IRON_LOOK, Some(IRON_SHADE));
        canvas
    }

    fn copper_ore(size: u32, seed: u32) -> Canvas {
        let mut canvas = stone(size, seed ^ 0x43);
        flat_ore(&mut canvas, seed ^ 0x53, 6, COPPER_LOOK, Some(COPPER_PATINA));
        canvas
    }

    fn furnace_top(size: u32, seed: u32) -> Canvas {
        furnace_top_from(stone(size, seed ^ 0x61), seed, &FURNACE_LOOK)
    }

    fn furnace_side(size: u32, seed: u32) -> Canvas {
        furnace_side_from(stone(size, seed ^ 0x62), seed, &FURNACE_LOOK)
    }

    fn furnace_front(size: u32, seed: u32) -> Canvas {
        furnace_front_from(stone(size, seed ^ 0x63), seed, &FURNACE_LOOK)
    }

    fn chest_top(size: u32, seed: u32) -> Canvas {
        chest_top_from(planks(size, seed ^ 0x71), seed, &CHEST_LOOK)
    }

    fn chest_side(size: u32, seed: u32) -> Canvas {
        chest_side_from(planks(size, seed ^ 0x72), seed, &CHEST_LOOK)
    }

    fn chest_front(size: u32, seed: u32) -> Canvas {
        chest_front_from(planks(size, seed ^ 0x73), seed, &CHEST_LOOK)
    }

    fn debug(size: u32) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            if (x / 4 + y / 4).is_multiple_of(2) {
                [244, 170, 210]
            } else {
                [60, 60, 68]
            }
        })
    }
}

/// Procedural fallback layers for the 1.2 "Light & Home" catalogue.
///
/// These are deliberately style-aware but share one implementation: the four
/// built-in styles only change the palette, so a new layer cannot accidentally
/// fall back to the debug grid in one style. Torch and wheat layers are
/// alpha-cut-out; farmland and bed layers are opaque.
mod extras {
    use super::*;

    struct Palette {
        dirt: [u8; 3],
        wet: [u8; 3],
        stem: [u8; 3],
        stem_dark: [u8; 3],
        grain: [u8; 3],
        grain_light: [u8; 3],
        blanket: [u8; 3],
        blanket_dark: [u8; 3],
        pillow: [u8; 3],
        wood: [u8; 3],
        wood_dark: [u8; 3],
    }

    fn palette(style: TexturePackStyle) -> Palette {
        match style {
            TexturePackStyle::Classic => Palette {
                dirt: [138, 98, 64],
                wet: [86, 62, 42],
                stem: [92, 148, 58],
                stem_dark: [62, 110, 40],
                grain: [196, 168, 76],
                grain_light: [226, 202, 106],
                blanket: [168, 42, 44],
                blanket_dark: [116, 27, 31],
                pillow: [224, 218, 204],
                wood: [157, 112, 63],
                wood_dark: [101, 69, 39],
            },
            TexturePackStyle::Painterly => Palette {
                dirt: [151, 108, 71],
                wet: [91, 68, 48],
                stem: [103, 162, 63],
                stem_dark: [70, 120, 44],
                grain: [212, 182, 84],
                grain_light: [241, 219, 124],
                blanket: [182, 50, 56],
                blanket_dark: [122, 31, 38],
                pillow: [235, 229, 216],
                wood: [172, 126, 74],
                wood_dark: [112, 78, 45],
            },
            TexturePackStyle::Lush => Palette {
                dirt: [126, 92, 58],
                wet: [70, 58, 40],
                stem: [82, 158, 55],
                stem_dark: [52, 116, 38],
                grain: [198, 178, 70],
                grain_light: [234, 216, 98],
                blanket: [156, 46, 54],
                blanket_dark: [105, 29, 36],
                pillow: [230, 226, 208],
                wood: [146, 106, 61],
                wood_dark: [92, 64, 37],
            },
            TexturePackStyle::Minimal => Palette {
                dirt: [139, 102, 70],
                wet: [84, 62, 44],
                stem: [98, 152, 64],
                stem_dark: [66, 112, 44],
                grain: [200, 176, 80],
                grain_light: [226, 204, 112],
                blanket: [166, 46, 50],
                blanket_dark: [116, 30, 34],
                pillow: [226, 222, 210],
                wood: [158, 116, 70],
                wood_dark: [104, 73, 42],
            },
        }
    }

    /// Writes an opaque pixel (the canvas starts fully transparent for cut-outs).
    fn put(canvas: &mut Canvas, x: i32, y: i32, color: [u8; 3]) {
        canvas.set(x, y, color);
        canvas.set_alpha(x, y, 255);
    }

    /// Clears every alpha value so only explicitly painted texels survive.
    fn clear(canvas: &mut Canvas) {
        for y in 0..canvas.size as i32 {
            for x in 0..canvas.size as i32 {
                canvas.set_alpha(x, y, 0);
            }
        }
    }

    /// One 1.2 layer for `style`.
    pub(super) fn layer(style: TexturePackStyle, name: &str, size: u32, seed: u32) -> Canvas {
        let palette = palette(style);
        match name {
            "torch" => torch(size, seed, &palette),
            "farmland_dry" => farmland(size, seed, &palette, false),
            "farmland_wet" => farmland(size, seed, &palette, true),
            "wheat_0" => wheat(size, seed, &palette, 0),
            "wheat_1" => wheat(size, seed, &palette, 1),
            "wheat_2" => wheat(size, seed, &palette, 2),
            "wheat_3" => wheat(size, seed, &palette, 3),
            "bed_foot_top" | "bed_head_top" => bed_top(size, seed, &palette, name == "bed_foot_top"),
            "bed_foot_side" | "bed_head_side" => bed_side(size, seed, &palette, name == "bed_foot_side"),
            "fire" => fire(size, seed),
            _ => Canvas::new(size, [0, 0, 0]),
        }
    }

    fn torch(size: u32, seed: u32, palette: &Palette) -> Canvas {
        let mut canvas = Canvas::new(size, [0, 0, 0]);
        clear(&mut canvas);
        let edge = size as i32;
        let center = edge / 2;
        let stick_half = (edge / 16).max(1);
        let stick_top = (edge / 4).max(1);
        let stick_bottom = edge - 2;
        for y in stick_top..stick_bottom {
            let grain = i32::from(periodic_noise(seed ^ 0x51, y as u32, 0, size, 4)) / 24;
            for dx in -stick_half..=stick_half {
                put(&mut canvas, center + dx, y, offset(palette.wood, grain - dx * 4));
            }
        }
        // Flame: a small orange/yellow plume with a brighter core.
        let flame_bottom = stick_top;
        for y in (edge / 16).max(0)..flame_bottom {
            let t = flame_bottom - y;
            let half = (1 + t / 3).min(edge / 5);
            for dx in -half..=half {
                let hot = dx.abs() <= half / 2 && y < flame_bottom - 1;
                let color = if hot {
                    [255, 224, 116]
                } else if y > flame_bottom / 2 {
                    [238, 126, 38]
                } else {
                    [204, 82, 26]
                };
                put(
                    &mut canvas,
                    center + dx,
                    y,
                    offset(color, (hash_noise(seed, dx as u32, y as u32) % 8) as i32 - 4),
                );
            }
        }
        // A few ember specks around the flame keep the silhouette readable.
        for y in 0..flame_bottom {
            for x in 0..edge {
                if (x - center).abs() > stick_half + 2 && hash_noise(seed ^ 0xF1, x as u32, y as u32).is_multiple_of(17)
                {
                    put(&mut canvas, x, y, [236, 150, 52]);
                }
            }
        }
        canvas
    }

    /// One animated-looking flame cut-out.
    ///
    /// The shape is a ragged teardrop with a hot yellow core and an orange/red
    /// rim. The renderer animates the cut-out by wobbling the sampled UV and
    /// modulating the alpha threshold with time (see `shader.wgsl`), so a single
    /// 16²/32² layer reads as a moving flame without a texture atlas or a second
    /// layer. Alpha is binary: transparent or fully opaque, which keeps the
    /// cross-quad cut-out crisp through the mip chain.
    fn fire(size: u32, seed: u32) -> Canvas {
        let mut canvas = Canvas::new(size, [0, 0, 0]);
        clear(&mut canvas);
        let edge = size as i32;
        let center = edge / 2;
        let bright = [255, 238, 158];
        let hot = [250, 176, 54];
        let warm = [232, 104, 26];
        let deep = [168, 42, 14];
        for y in 0..edge {
            // ny: 0 at the tip, 256 at the base.
            let ny = (y * 256) / (edge - 1).max(1);
            // Half-width grows towards the base; the tip stays thin.
            let half_width = ((18 + 92 * ny / 256) * edge / 256).max(1);
            // A slow lateral sway makes the silhouette asymmetric.
            let noise = i32::from(periodic_noise(seed, 0, y as u32, size, 4)) - 128;
            let sway = noise * edge / 2048 * (256 - ny) / 256;
            let cx = center + sway;
            for x in 0..edge {
                let dx = (x - cx).abs();
                if dx > half_width {
                    continue;
                }
                let grain = (hash_noise(seed ^ 0xF1, x as u32, y as u32) & 0xff) as i32;
                // Ragged tip and occasional gaps keep the flame from reading as
                // a solid triangle.
                if grain < (256 - ny) / 5 + 18 {
                    continue;
                }
                if dx > half_width - 1 && grain < 96 {
                    continue;
                }
                let centrality = (half_width - dx) * 255 / half_width.max(1);
                let heat = (centrality + ny * 90 / 256).min(255);
                let colour = if heat > 205 {
                    bright
                } else if heat > 150 {
                    hot
                } else if heat > 90 {
                    warm
                } else {
                    deep
                };
                put(&mut canvas, x, y, offset(colour, grain / 24 - 6));
            }
        }
        // A couple of detached sparks above the flame.
        for spark in 0..(edge / 8).max(1) {
            let x = center + (hash_noise(seed ^ 0xA7, spark as u32, 0) % (edge as u32 / 3).max(1)) as i32 - edge / 6;
            let y = (hash_noise(seed ^ 0x5D, 0, spark as u32) % (edge as u32 / 4).max(1)) as i32 + 1;
            put(&mut canvas, x, y, hot);
        }
        canvas
    }

    fn farmland(size: u32, seed: u32, palette: &Palette, wet: bool) -> Canvas {
        let base = if wet { palette.wet } else { palette.dirt };
        Canvas::from_fn(size, |x, y| {
            let fine = i32::from(periodic_noise(seed, x, y, size, 8)) / 18 - 7;
            let broad = i32::from(periodic_noise(seed ^ 0x7F, x, y, size, 2)) / 24 - 5;
            // Three horizontal furrows and a darker ridge between them.
            let row = (y * 4 / size) as i32;
            let furrow = if y % (size / 4).max(1) == 0 { -22 } else { 0 };
            let ridge = match row {
                0 | 2 => 6,
                _ => 0,
            };
            offset(base, fine + broad + furrow + ridge)
        })
    }

    fn wheat(size: u32, seed: u32, palette: &Palette, stage: u32) -> Canvas {
        let mut canvas = Canvas::new(size, [0, 0, 0]);
        clear(&mut canvas);
        let edge = size as i32;
        let base_y = edge - 1;
        let height = (edge as u32 * (5 + stage * 3) / 16).max(2) as i32;
        let blades = 3 + stage as usize;
        for blade in 0..blades {
            let spread = edge / (blades as i32 + 2);
            let x = edge / 2 + (blade as i32 - blades as i32 / 2) * spread;
            let lean = if blade % 2 == 0 { 1 } else { -1 } * (1 + stage as i32 / 2);
            let blade_height = height + (hash_noise(seed, blade as u32, stage) % 3) as i32 - 1;
            for step in 0..blade_height.max(1) {
                let px = if step == blade_height - 1 { x + lean } else { x };
                let color = offset(palette.stem, -step / 3);
                put(&mut canvas, px, base_y - step, color);
            }
            if stage >= 1 {
                // Grain head: two or three rows of kernels on the top.
                let head = (stage.min(3) + 1) as i32;
                for row in 0..head {
                    for dx in -1..=1 {
                        let color = if (row + dx) % 2 == 0 {
                            palette.grain
                        } else {
                            palette.grain_light
                        };
                        put(&mut canvas, x + lean + dx, base_y - blade_height + row, color);
                    }
                }
            }
        }
        // A short green crown at the base breaks up the flat bottom edge.
        for x in 0..edge {
            if !hash_noise(seed ^ 0xA7, x as u32, 0).is_multiple_of(3) {
                put(&mut canvas, x, base_y, offset(palette.stem_dark, -2));
            }
        }
        canvas
    }

    fn bed_top(size: u32, seed: u32, palette: &Palette, foot: bool) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let edge = size as i32;
            let border = (edge / 16).max(1);
            let inset =
                x < border as u32 || y < border as u32 || x >= size - border as u32 || y >= size - border as u32;
            if inset {
                return palette.blanket_dark;
            }
            // Pillow at the head end; the foot has the folded blanket edge.
            if foot && y < size / 4 {
                let noise = i32::from(periodic_noise(seed ^ 0x12, x, y, size, 4)) / 22 - 5;
                return offset(palette.pillow, noise);
            }
            let fold = if !foot && y > size - size / 5 { -18 } else { 0 };
            let weave = i32::from(periodic_noise(seed, x / 2, y / 2, size.max(2), 4)) / 26 - 4;
            offset(palette.blanket, weave + fold)
        })
    }

    fn bed_side(size: u32, seed: u32, palette: &Palette, foot: bool) -> Canvas {
        Canvas::from_fn(size, |x, y| {
            let edge = size as i32;
            let top = edge / 3;
            if y < top as u32 {
                // Blanket drape over the wooden frame.
                let noise = i32::from(periodic_noise(seed ^ 0x33, x, y, size, 4)) / 24 - 5;
                let seam = if y == top as u32 - 1 { -20 } else { 0 };
                offset(palette.blanket, noise + seam)
            } else if y < (top + 2) as u32 {
                palette.blanket_dark
            } else {
                // Visible wooden slats below the blanket.
                let slat = if y % (size / 4).max(1) == 0 { -16 } else { 0 };
                let grain = i32::from(periodic_noise(seed ^ 0x44, x, y, size, 8)) / 22 - 5;
                let pillow = if foot && y > size - size / 5 { 8 } else { 0 };
                if y == (edge - 1) as u32 {
                    palette.wood_dark
                } else {
                    offset(palette.wood, slat + grain + pillow)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use bytemuck::{Pod, Zeroable};
    use ob_content::textures as catalogue;

    use super::*;
    use crate::textures::BlockTextureArray;

    #[test]
    fn fire_layer_is_a_visible_alpha_cutout_flame() {
        for style in TexturePackStyle::ALL {
            let pack = TexturePack::generate(style);
            let layer = pack.layer(0, catalogue::FIRE.0 as usize).expect("fire layer exists");
            let edge = style.resolution() as usize;
            let mut opaque = 0usize;
            let mut rows = Vec::new();
            for y in 0..edge {
                let mut row = 0usize;
                for x in 0..edge {
                    let offset = (y * edge + x) * 4;
                    if layer[offset + 3] > 0 {
                        row += 1;
                    }
                }
                opaque += row;
                rows.push(row);
            }
            let coverage = opaque as f64 / (edge * edge) as f64;
            eprintln!("{style:?} fire coverage {coverage:.2} rows {rows:?}");
            assert!(
                (0.15..0.75).contains(&coverage),
                "{style:?} fire coverage {coverage:.2} (rows {rows:?})"
            );
            assert!(
                rows[0] + rows[1] + rows[2] > 0,
                "{style:?} fire must reach the tip (rows {rows:?})"
            );
            assert!(rows[edge - 1] > 0, "{style:?} fire must have a base");
            assert!(
                rows[edge / 2] > 0,
                "{style:?} fire must have a full body (rows {rows:?})"
            );
        }
    }

    /// FNV-1a over one layer, used to check distinctness.
    fn layer_hash(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        hash
    }

    /// Mean RGB of one layer at mip 0.
    fn mean_color(pack: &TexturePack, layer: usize) -> [f64; 3] {
        let bytes = pack.layer(0, layer).expect("layer exists");
        let mut sums = [0u64; 3];
        for texel in bytes.as_chunks::<4>().0 {
            for channel in 0..3 {
                sums[channel] += u64::from(texel[channel]);
            }
        }
        let count = (bytes.len() / 4) as f64;
        [sums[0] as f64 / count, sums[1] as f64 / count, sums[2] as f64 / count]
    }

    /// Luma of an sRGB colour.
    fn luma(colour: [f64; 3]) -> f64 {
        0.2126 * colour[0] + 0.7152 * colour[1] + 0.0722 * colour[2]
    }

    /// Mean luma of one row (`vertical == false`) or column at mip 0.
    fn line_luma(pack: &TexturePack, layer: usize, line: usize, vertical: bool) -> f64 {
        let bytes = pack.layer(0, layer).expect("layer exists");
        let edge = pack.resolution() as usize;
        let mut sum = 0.0;
        for i in 0..edge {
            let (x, y) = if vertical { (line, i) } else { (i, line) };
            let offset = (y * edge + x) * 4;
            sum += 0.2126 * f64::from(bytes[offset])
                + 0.7152 * f64::from(bytes[offset + 1])
                + 0.0722 * f64::from(bytes[offset + 2]);
        }
        sum / edge as f64
    }

    #[test]
    fn style_names_round_trip_and_resolutions_match() {
        assert_eq!(TexturePackStyle::default(), TexturePackStyle::Classic);
        let expected = ["classic", "painterly", "lush", "minimal"];
        for (style, name) in TexturePackStyle::ALL.into_iter().zip(expected) {
            assert_eq!(style.name(), name);
            assert_eq!(TexturePackStyle::from_name(name), Some(style));
        }
        assert_eq!(TexturePackStyle::Classic.resolution(), 16);
        assert_eq!(TexturePackStyle::Painterly.resolution(), 32);
        assert_eq!(TexturePackStyle::Lush.resolution(), 32);
        assert_eq!(TexturePackStyle::Minimal.resolution(), 16);
        assert_eq!(TexturePackStyle::from_name("fancy"), None);
        assert_eq!(TexturePackStyle::from_name(""), None);
    }

    #[test]
    fn every_style_has_count_layers_at_the_right_resolution() {
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            let resolution = style.resolution();
            assert_eq!(pack.style(), style);
            assert_eq!(pack.resolution(), resolution);
            let expected_mips = resolution.trailing_zeros() + 1;
            assert_eq!(pack.mip_level_count(), expected_mips);
            for level in 0..expected_mips {
                let edge = resolution >> level;
                assert_eq!(pack.mip_edge(level), Some(edge));
                let mip = pack.mip(level).expect("mip exists");
                assert_eq!(mip.len(), LAYER_COUNT * (edge * edge * 4) as usize);
                for layer in 0..LAYER_COUNT {
                    let bytes = pack.layer(level, layer).expect("layer exists");
                    assert_eq!(bytes.len(), (edge * edge * 4) as usize);
                    let is_cut_out = layer == catalogue::LEAVES.0 as usize
                        || layer == catalogue::TORCH.0 as usize
                        || layer == catalogue::FIRE.0 as usize
                        || (catalogue::WHEAT_0.0 as usize..=catalogue::WHEAT_3.0 as usize).contains(&layer);
                    for texel in bytes.as_chunks::<4>().0 {
                        if is_cut_out && level == 0 {
                            assert!(
                                texel[3] == 0 || texel[3] == 255,
                                "cut-out alpha must be binary at mip 0 in {style:?}"
                            );
                        } else if !is_cut_out {
                            assert_eq!(texel[3], 255, "{style:?} layer {layer} must stay opaque");
                        }
                    }
                }
            }
            assert_eq!(pack.mip(expected_mips), None);
            assert_eq!(pack.layer(0, LAYER_COUNT), None);
        }
    }

    #[test]
    fn leaf_cutouts_are_clustered_and_cover_the_expected_fraction() {
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            let edge = pack.resolution() as usize;
            let bytes = pack.layer(0, catalogue::LEAVES.0 as usize).expect("leaves layer");
            let total = edge * edge;
            let holes: Vec<(usize, usize)> = (0..edge)
                .flat_map(|y| (0..edge).map(move |x| (x, y)))
                .filter(|&(x, y)| bytes[(y * edge + x) * 4 + 3] == 0)
                .collect();
            let percent = holes.len() * 100 / total;
            let (low, high) = match style {
                TexturePackStyle::Classic | TexturePackStyle::Lush => (12, 20),
                TexturePackStyle::Painterly => (8, 14),
                TexturePackStyle::Minimal => (2, 7),
            };
            assert!(
                (low..=high).contains(&percent),
                "{style:?} leaf holes cover {percent}% of the tile, expected {low}..={high}%"
            );
            for &(x, y) in &holes {
                let mut neighbours = 0;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = (x as i32 + dx).rem_euclid(edge as i32) as usize;
                        let ny = (y as i32 + dy).rem_euclid(edge as i32) as usize;
                        if bytes[(ny * edge + nx) * 4 + 3] == 0 {
                            neighbours += 1;
                        }
                    }
                }
                assert!(
                    neighbours > 0,
                    "{style:?} has an isolated leaf hole at ({x}, {y}); holes should cluster"
                );
            }
        }
    }

    #[test]
    fn generation_is_deterministic_for_every_style() {
        for style in TexturePackStyle::ALL {
            let first = generate_texture_pack(style);
            let second = generate_texture_pack(style);
            assert_eq!(first.mip_level_count(), second.mip_level_count());
            for level in 0..first.mip_level_count() {
                assert_eq!(first.mip(level), second.mip(level), "{style:?} mip {level} differs");
            }
        }
    }

    #[test]
    fn each_layer_and_level_varies() {
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            for layer in 0..LAYER_COUNT {
                let bytes = pack.layer(0, layer).expect("layer exists");
                let first = bytes[0..3].to_vec();
                let varies = bytes.as_chunks::<4>().0.iter().any(|texel| texel[0..3] != first[..]);
                assert!(
                    varies,
                    "{style:?} layer {layer} ({}) is a flat colour",
                    catalogue::NAMES[layer]
                );
            }
        }
    }

    #[test]
    fn layer_hashes_are_distinct_across_styles() {
        let mut seen = HashSet::new();
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            for layer in 0..LAYER_COUNT {
                let bytes = pack.layer(0, layer).expect("layer exists");
                assert!(
                    seen.insert(layer_hash(bytes)),
                    "duplicate texture bytes for {style:?} layer {layer} ({})",
                    catalogue::NAMES[layer]
                );
            }
        }
        assert_eq!(seen.len(), TexturePackStyle::ALL.len() * LAYER_COUNT);
    }

    #[test]
    fn stone_palettes_differ_between_every_style() {
        let stone = catalogue::STONE.0 as usize;
        let means: Vec<(TexturePackStyle, [f64; 3])> = TexturePackStyle::ALL
            .into_iter()
            .map(|style| (style, mean_color(&generate_texture_pack(style), stone)))
            .collect();
        for (index, (first_style, first)) in means.iter().enumerate() {
            for (second_style, second) in &means[index + 1..] {
                let distance =
                    ((first[0] - second[0]).powi(2) + (first[1] - second[1]).powi(2) + (first[2] - second[2]).powi(2))
                        .sqrt();
                assert!(
                    distance > 10.0,
                    "stone means for {first_style:?} {first:?} and {second_style:?} {second:?} are too close ({distance:.2})"
                );
            }
        }
    }

    #[test]
    fn new_block_layers_read_as_planks_cobblestone_and_crafting_tables() {
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            let edge = pack.resolution() as usize;

            // Planks: warm brown, four horizontal boards separated by dark seams.
            // Planks: warm brown, four horizontal boards separated by dark seams.
            let planks = catalogue::PLANKS.0 as usize;
            let planks_mean = mean_color(&pack, planks);
            assert!(
                planks_mean[0] > planks_mean[1] && planks_mean[1] > planks_mean[2],
                "{style:?} planks are not warm brown: {planks_mean:?}"
            );
            assert!(
                planks_mean[0] - planks_mean[2] > 30.0,
                "{style:?} planks are not warm enough: {planks_mean:?}"
            );
            let planks_luma = luma(planks_mean);
            let rows: Vec<f64> = (0..edge).map(|y| line_luma(&pack, planks, y, false)).collect();
            let seams = (1..edge - 1)
                .filter(|&y| {
                    rows[y] + 12.0 < rows[y - 1] && rows[y] + 12.0 < rows[y + 1] && rows[y] < planks_luma - 8.0
                })
                .count();
            assert!(seams >= 2, "{style:?} planks have only {seams} dark horizontal seams");

            // Cobblestone: grey stone with dark mortar and a rounded light/dark read.
            let cobble = catalogue::COBBLESTONE.0 as usize;
            let cobble_mean = mean_color(&pack, cobble);
            assert!(
                (cobble_mean[0] - cobble_mean[1]).abs() < 25.0 && (cobble_mean[1] - cobble_mean[2]).abs() < 30.0,
                "{style:?} cobblestone is not grey: {cobble_mean:?}"
            );
            let cobble_bytes = pack.layer(0, cobble).expect("cobblestone layer");
            let lumas: Vec<f64> = cobble_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|texel| 0.2126 * f64::from(texel[0]) + 0.7152 * f64::from(texel[1]) + 0.0722 * f64::from(texel[2]))
                .collect();
            let min_luma = lumas.iter().copied().fold(f64::INFINITY, f64::min);
            let max_luma = lumas.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            assert!(
                max_luma - min_luma > 40.0,
                "{style:?} cobblestone has too little contrast ({min_luma:.1}..{max_luma:.1})"
            );
            // The mortar is the darkest part of the luminance range.
            let threshold = min_luma + (max_luma - min_luma) * 0.22;
            let dark = lumas.iter().filter(|luma| **luma < threshold).count();
            let dark_fraction = dark as f64 / (edge * edge) as f64;
            assert!(
                (0.05..=0.65).contains(&dark_fraction),
                "{style:?} cobblestone mortar covers {:.1}% of the tile",
                dark_fraction * 100.0
            );

            // Crafting table top: darker grid lines cross the wooden surface.
            let table_top = catalogue::CRAFTING_TABLE_TOP.0 as usize;
            let top_luma = luma(mean_color(&pack, table_top));
            let dark_rows = (0..edge)
                .filter(|&y| line_luma(&pack, table_top, y, false) < top_luma - 12.0)
                .count();
            let dark_columns = (0..edge)
                .filter(|&x| line_luma(&pack, table_top, x, true) < top_luma - 12.0)
                .count();
            assert!(
                dark_rows >= 2 && dark_columns >= 2,
                "{style:?} crafting table top has {dark_rows} dark rows and {dark_columns} dark columns"
            );
            assert_ne!(
                pack.layer(0, table_top),
                pack.layer(0, planks),
                "{style:?} crafting table top must not be a plain plank field"
            );

            // Crafting table side: a lit table lip and a metal tool detail.
            let table_side = catalogue::CRAFTING_TABLE_SIDE.0 as usize;
            let lip = line_luma(&pack, table_side, 0, false);
            let lower = line_luma(&pack, table_side, edge - 2, false);
            assert!(
                lip > lower + 5.0,
                "{style:?} crafting table side lip is not lit ({lip:.1} vs {lower:.1})"
            );
            let side_bytes = pack.layer(0, table_side).expect("crafting table side");
            let tool_pixels = side_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|texel| i32::from(texel[2]) > i32::from(texel[0]) + 4)
                .count();
            assert!(
                tool_pixels >= 2,
                "{style:?} crafting table side has no cool metal tool pixels"
            );
        }
    }

    #[test]
    fn every_catalogue_layer_differs_from_debug_in_every_style() {
        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            let debug = pack.layer(0, catalogue::DEBUG.0 as usize).expect("debug layer");
            for layer in 1..LAYER_COUNT {
                assert_ne!(
                    pack.layer(0, layer).expect("catalogue layer"),
                    debug,
                    "{style:?} layer {layer} ({}) fell back to the debug checkerboard",
                    catalogue::NAMES[layer]
                );
            }
        }
    }

    #[test]
    fn ore_furnace_and_chest_layers_read_as_intended() {
        /// Counts texels matching a predicate.
        fn count(pack: &TexturePack, layer: usize, predicate: impl Fn([u8; 4]) -> bool) -> usize {
            pack.layer(0, layer)
                .expect("layer exists")
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|texel| predicate(**texel))
                .count()
        }

        for style in TexturePackStyle::ALL {
            let pack = generate_texture_pack(style);
            let stone = mean_color(&pack, catalogue::STONE.0 as usize);
            let coal = mean_color(&pack, catalogue::COAL_ORE.0 as usize);

            // Coal is a dark mineral on the same stone base; the mean luma drop
            // proves the chunks cover enough of the tile to read at a distance.
            assert!(
                luma(coal) + 4.0 < luma(stone),
                "{style:?} coal ore ({:.1}) should be darker than stone ({:.1})",
                luma(coal),
                luma(stone)
            );

            // Iron and copper add warm mineral flecks; coal has none. Counting
            // texels is more robust than a mean for the very flat Minimal style.
            let warm = |layer: usize| count(&pack, layer, |texel| i32::from(texel[0]) > i32::from(texel[2]) + 20);
            assert!(
                warm(catalogue::IRON_ORE.0 as usize) >= 10,
                "{style:?} iron ore has no warm beige/brown flecks"
            );
            assert!(
                warm(catalogue::COPPER_ORE.0 as usize) >= 10,
                "{style:?} copper ore has no warm orange flecks"
            );
            assert_eq!(
                warm(catalogue::COAL_ORE.0 as usize),
                0,
                "{style:?} coal ore must stay neutral black, not warm"
            );
            let patina = count(&pack, catalogue::COPPER_ORE.0 as usize, |texel| {
                i32::from(texel[1]) > i32::from(texel[0]) + 20 && i32::from(texel[1]) > i32::from(texel[2]) + 10
            });
            assert!(patina >= 2, "{style:?} copper ore has no teal-green patina pixels");

            // The furnace front is a dark firebox with warm embers at the bottom.
            let front = catalogue::FURNACE_FRONT.0 as usize;
            let dark = count(&pack, front, |texel| {
                let luma = 0.2126 * f64::from(texel[0]) + 0.7152 * f64::from(texel[1]) + 0.0722 * f64::from(texel[2]);
                luma < 80.0
            });
            let embers = count(&pack, front, |texel| {
                i32::from(texel[0]) > i32::from(texel[1]) + 40 && i32::from(texel[1]) > i32::from(texel[2]) + 15
            });
            assert!(
                dark >= 4,
                "{style:?} furnace front has no dark arched opening ({dark} dark texels)"
            );
            assert!(
                embers >= 2,
                "{style:?} furnace front has no warm ember glow ({embers} ember texels)"
            );
            let furnace_faces = [
                catalogue::FURNACE_TOP.0 as usize,
                catalogue::FURNACE_SIDE.0 as usize,
                catalogue::FURNACE_FRONT.0 as usize,
            ];
            for (index, &first) in furnace_faces.iter().enumerate() {
                for &second in &furnace_faces[index + 1..] {
                    assert_ne!(
                        pack.layer(0, first),
                        pack.layer(0, second),
                        "{style:?} furnace faces {} and {} must differ",
                        catalogue::NAMES[first],
                        catalogue::NAMES[second]
                    );
                }
            }

            // Chests stay wooden while their iron bands read as cool grey metal.
            let chest_faces = [
                catalogue::CHEST_TOP.0 as usize,
                catalogue::CHEST_SIDE.0 as usize,
                catalogue::CHEST_FRONT.0 as usize,
            ];
            for &face in &chest_faces {
                let mean = mean_color(&pack, face);
                assert!(
                    mean[0] > mean[1] && mean[1] > mean[2] && mean[0] - mean[2] > 25.0,
                    "{style:?} {} is not warm wood: {mean:?}",
                    catalogue::NAMES[face]
                );
                let metal = count(&pack, face, |texel| {
                    let max = texel[0].max(texel[1]).max(texel[2]);
                    let min = texel[0].min(texel[1]).min(texel[2]);
                    max - min <= 18 && texel[3] == 255
                });
                assert!(
                    metal >= 2,
                    "{style:?} {} has no iron band pixels ({metal} grey texels)",
                    catalogue::NAMES[face]
                );
            }
            for (index, &first) in chest_faces.iter().enumerate() {
                for &second in &chest_faces[index + 1..] {
                    assert_ne!(
                        pack.layer(0, first),
                        pack.layer(0, second),
                        "{style:?} chest faces {} and {} must differ",
                        catalogue::NAMES[first],
                        catalogue::NAMES[second]
                    );
                }
            }
        }
    }

    #[test]
    fn mips_are_box_filtered_from_the_base_level() {
        let pack = generate_texture_pack(TexturePackStyle::Classic);
        let resolution = pack.resolution() as usize;
        let layer = catalogue::STONE.0 as usize;
        let base = pack.layer(0, layer).expect("base layer");
        let mip = pack.layer(1, layer).expect("first mip");
        let half = resolution / 2;
        for (x, y) in [(0, 0), (3, 5), (half - 1, half - 1)] {
            let mut sums = [0u32; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let texel = ((y * 2 + dy) * resolution + x * 2 + dx) * 4;
                    for channel in 0..3 {
                        sums[channel] += u32::from(base[texel + channel]);
                    }
                }
            }
            let texel = (y * half + x) * 4;
            for channel in 0..3 {
                assert_eq!(mip[texel + channel], (sums[channel] / 4) as u8);
            }
            assert_eq!(mip[texel + 3], 255);
        }
    }

    #[test]
    fn classic_compatibility_data_matches_the_classic_pack() {
        let pack = generate_texture_pack(TexturePackStyle::Classic);
        let data = crate::textures::generate_texture_data();
        assert_eq!(Some(data.as_slice()), pack.mip(0));
    }

    /// Small vertex used by the preview harness.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable)]
    struct GridVertex {
        position: [f32; 2],
        uv: [f32; 2],
        layer: u32,
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
            label: Some("ob-texture-pack-test"),
            ..Default::default()
        }))
        .ok()?;
        Some((device, queue, summary))
    }

    /// Copies an RGBA8 texture back into tightly packed top-row-first bytes.
    fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let unpadded = width * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("texture-pack-test-readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("texture-pack-test-readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let mapped: Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> = Arc::new(Mutex::new(None));
        {
            let mapped = Arc::clone(&mapped);
            slice.map_async(wgpu::MapMode::Read, move |result| {
                if let Ok(mut slot) = mapped.lock() {
                    *slot = Some(result);
                }
            });
        }
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll readback");
        let map_result = pollster::block_on(std::future::poll_fn(|_cx| {
            let mut slot = mapped.lock().expect("map callback state poisoned");
            match slot.take() {
                Some(result) => std::task::Poll::Ready(result),
                None => std::task::Poll::Pending,
            }
        }));
        map_result.expect("map readback buffer");

        let mut pixels = Vec::with_capacity((unpadded * height) as usize);
        {
            let data = slice.get_mapped_range().expect("mapped range");
            for row in 0..height as usize {
                let start = row * padded as usize;
                pixels.extend_from_slice(&data[start..start + unpadded as usize]);
            }
        }
        buffer.unmap();
        pixels
    }

    /// Writes an RGBA8 image to disk.
    fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
        let file = std::fs::File::create(path).expect("create preview png");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write png header");
        writer.write_image_data(rgba).expect("write png pixels");
    }

    /// Texture-array preview shader: one quad per cell, layer from the vertex.
    const PREVIEW_SHADER: &str = r#"
struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
};

@group(0) @binding(0) var block_textures: texture_2d_array<f32>;
@group(0) @binding(1) var block_sampler: sampler;

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) layer: u32,
) -> VertexOut {
    var out: VertexOut;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = uv;
    out.layer = layer;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(block_textures, block_sampler, in.uv, i32(in.layer));
}
"#;

    /// Renders a 4×4 layer grid per style and writes `/tmp/ob-texture-<style>.png`.
    #[test]
    #[ignore = "GPU: renders texture-pack preview PNGs to /tmp"]
    fn gpu_previews_are_non_uniform_and_differ_between_styles() {
        let Some((device, queue, summary)) = test_gpu() else {
            eprintln!("skipping gpu_previews_are_non_uniform_and_differ_between_styles: no GPU adapter");
            return;
        };
        eprintln!("texture pack preview GPU: {summary}");

        // Six columns by five rows: all twenty-seven layers fit on one sheet.
        const WIDTH: u32 = 540;
        const HEIGHT: u32 = 450;
        const COLUMNS: u32 = 6;
        const ROWS: u32 = 5;
        const CELL: u32 = WIDTH / COLUMNS;
        const GAP: u32 = 8;
        const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

        debug_assert_eq!(CELL * ROWS, HEIGHT);
        debug_assert!(COLUMNS * ROWS >= LAYER_COUNT as u32);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture-pack-preview-target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut vertices = Vec::with_capacity(LAYER_COUNT * 6);
        for layer in 0..LAYER_COUNT as u32 {
            let row = layer / COLUMNS;
            let column = layer % COLUMNS;
            let x0 = (column * CELL + GAP / 2) as f32 / WIDTH as f32 * 2.0 - 1.0;
            let x1 = (column * CELL + CELL - GAP / 2) as f32 / WIDTH as f32 * 2.0 - 1.0;
            let y0 = 1.0 - (row * CELL + GAP / 2) as f32 / HEIGHT as f32 * 2.0;
            let y1 = 1.0 - (row * CELL + CELL - GAP / 2) as f32 / HEIGHT as f32 * 2.0;
            let corners = [
                ([x0, y0], [0.0, 0.0]),
                ([x1, y0], [1.0, 0.0]),
                ([x1, y1], [1.0, 1.0]),
                ([x0, y0], [0.0, 0.0]),
                ([x1, y1], [1.0, 1.0]),
                ([x0, y1], [0.0, 1.0]),
            ];
            for (position, uv) in corners {
                vertices.push(GridVertex { position, uv, layer });
            }
        }
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("texture-pack-preview-vertices"),
            size: (vertices.len() * std::mem::size_of::<GridVertex>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&vertex_buffer, 0, bytemuck::cast_slice(&vertices));

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("texture-pack-preview-shader"),
            source: wgpu::ShaderSource::Wgsl(PREVIEW_SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture-pack-preview-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("texture-pack-preview-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 16,
                shader_location: 2,
            },
        ];
        let vertex_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GridVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        })];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("texture-pack-preview-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &vertex_buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let mut previews = Vec::new();
        for style in TexturePackStyle::ALL {
            let blocks = BlockTextureArray::new_with_style(&device, &queue, style);
            assert_eq!(blocks.style(), style);
            assert_eq!(blocks.resolution(), style.resolution());

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("texture-pack-preview-bind-group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(blocks.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(blocks.sampler()),
                    },
                ],
            });

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("texture-pack-preview-encoder"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("texture-pack-preview-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.04,
                                g: 0.04,
                                b: 0.05,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                pass.draw(0..vertices.len() as u32, 0..1);
            }
            queue.submit([encoder.finish()]);

            let pixels = read_texture(&device, &queue, &target, WIDTH, HEIGHT);
            let unique: HashSet<[u8; 4]> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|texel| [texel[0], texel[1], texel[2], texel[3]])
                .collect();
            assert!(
                unique.len() > 32,
                "{style:?} preview is nearly uniform ({} colours)",
                unique.len()
            );
            for cell in 0..LAYER_COUNT as u32 {
                let row = cell / COLUMNS;
                let column = cell % COLUMNS;
                let cell_colors: HashSet<[u8; 4]> = (0..CELL)
                    .flat_map(|local_y| (0..CELL).map(move |local_x| (local_x, local_y)))
                    .map(|(local_x, local_y)| {
                        let x = column * CELL + local_x;
                        let y = row * CELL + local_y;
                        let offset = ((y * WIDTH + x) * 4) as usize;
                        [
                            pixels[offset],
                            pixels[offset + 1],
                            pixels[offset + 2],
                            pixels[offset + 3],
                        ]
                    })
                    .collect();
                assert!(
                    cell_colors.len() > 1,
                    "{style:?} grid cell {cell} ({}) is a flat colour",
                    catalogue::NAMES[cell as usize]
                );
            }

            let path = format!("/tmp/ob-texture-{}.png", style.name());
            write_png(Path::new(&path), WIDTH, HEIGHT, &pixels);
            let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            eprintln!("wrote {path} ({WIDTH}x{HEIGHT}, {bytes} bytes)");
            previews.push((style, pixels));
        }

        let default = BlockTextureArray::new(&device, &queue);
        assert_eq!(default.style(), TexturePackStyle::Classic);
        assert_eq!(default.resolution(), 16);

        for (index, (first_style, first)) in previews.iter().enumerate() {
            for (second_style, second) in &previews[index + 1..] {
                let differing = first
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(second.as_chunks::<4>().0.iter())
                    .filter(|(left, right)| left != right)
                    .count();
                assert!(
                    differing > (WIDTH * HEIGHT / 10) as usize,
                    "{first_style:?} and {second_style:?} previews look identical ({differing} pixels differ)"
                );
            }
        }
    }
}

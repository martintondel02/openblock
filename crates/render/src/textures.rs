//! Procedural block-texture array generated at start-up.
//!
//! One 16×16 RGBA8 layer is generated per entry of
//! [`ob_content::textures::NAMES`]; the layers live in a single
//! `Rgba8UnormSrgb` 2D array with repeat/nearest sampling. Generation uses a
//! small integer hash only, so the result is seed-free, deterministic and
//! identical on every platform. Real texture packs replace this module at
//! MS-05.

use ob_content::textures::{COUNT, NAMES};

/// Edge length of one block texture in texels.
pub const TEXTURE_SIZE: u32 = 16;

/// Bytes of one 16×16 RGBA8 layer.
const BYTES_PER_LAYER: usize = (TEXTURE_SIZE * TEXTURE_SIZE * 4) as usize;

// The catalogue the renderer generates and the engine's block table must agree.
const _: () = assert!(NAMES.len() == COUNT);

/// The GPU texture, its array view and the shared sampler.
///
/// The UI renderer borrows [`Self::view`] and [`Self::sampler`] so block icons
/// share the same procedural layers, filtering and repeat behaviour as world
/// geometry.
pub struct BlockTextureArray {
    /// The 2D-array texture holding every block layer.
    pub(crate) _texture: wgpu::Texture,
    /// D2Array view bound by the chunk pipeline.
    pub(crate) view: wgpu::TextureView,
    /// Repeat/nearest sampler shared by every layer.
    pub(crate) sampler: wgpu::Sampler,
}

impl BlockTextureArray {
    /// Uploads the procedural catalogue and creates the array view and sampler.
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let pixels = generate_texture_data();
        debug_assert_eq!(pixels.len(), COUNT * BYTES_PER_LAYER);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("block-textures"),
            size: wgpu::Extent3d {
                width: TEXTURE_SIZE,
                height: TEXTURE_SIZE,
                depth_or_array_layers: COUNT as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TEXTURE_SIZE * 4),
                rows_per_image: Some(TEXTURE_SIZE),
            },
            wgpu::Extent3d {
                width: TEXTURE_SIZE,
                height: TEXTURE_SIZE,
                depth_or_array_layers: COUNT as u32,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("block-textures-view"),
            format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("block-textures-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });

        Self {
            _texture: texture,
            view,
            sampler,
        }
    }

    /// The `D2Array` view of every block texture layer.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The nearest/repeat sampler shared with the world pipeline.
    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }
}

/// Generates every catalogue layer as tightly packed RGBA8 rows.
///
/// Layer `i` corresponds to `NAMES[i]` and is exactly
/// `TEXTURE_SIZE * TEXTURE_SIZE` pixels of four bytes.
pub fn generate_texture_data() -> Vec<u8> {
    let mut data = Vec::with_capacity(COUNT * BYTES_PER_LAYER);
    for (index, name) in NAMES.iter().enumerate() {
        data.extend_from_slice(&generate_layer(name, index as u32));
    }
    data
}

/// Generates one layer of `BYTES_PER_LAYER` RGBA8 pixels.
fn generate_layer(name: &str, layer: u32) -> Vec<u8> {
    const EDGE: u32 = TEXTURE_SIZE;
    let mut out = vec![0u8; BYTES_PER_LAYER];
    for y in 0..EDGE {
        for x in 0..EDGE {
            let rgb = match name {
                "debug" => debug_pixel(x, y),
                "grass_top" => grass_top_pixel(layer, x, y),
                "grass_side" => grass_side_pixel(layer, x, y),
                "dirt" => dirt_pixel(layer, x, y),
                "stone" => stone_pixel(layer, x, y),
                "sand" => sand_pixel(layer, x, y),
                "water" => water_pixel(layer, x, y),
                "log_side" => log_side_pixel(layer, x, y),
                "log_top" => log_top_pixel(layer, x, y),
                "leaves" => leaves_pixel(layer, x, y),
                "bedrock" => bedrock_pixel(layer, x, y),
                "gravel" => gravel_pixel(layer, x, y),
                "clay" => clay_pixel(layer, x, y),
                "snow" => snow_pixel(layer, x, y),
                // Unknown names fall back to the checkerboard rather than
                // panicking, mirroring `TextureId::DEBUG`.
                _ => debug_pixel(x, y),
            };
            let offset = ((y * EDGE + x) * 4) as usize;
            out[offset..offset + 3].copy_from_slice(&rgb);
            out[offset + 3] = 255;
        }
    }
    out
}

/// Small deterministic 32-bit avalanche hash. No floating point, no seed.
const fn hash32(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    value
}

/// Deterministic per-texel noise for one catalogue layer.
const fn noise(layer: u32, x: u32, y: u32) -> u32 {
    hash32(layer.wrapping_mul(0x9e37_79b9) ^ hash32(x.wrapping_add(0x1234_5678)) ^ hash32(y.wrapping_add(0x9abc_def0)))
}

/// Per-channel value jitter in `-jitter..=jitter`.
fn varied(base: [u8; 3], layer: u32, x: u32, y: u32, jitter: u32) -> [u8; 3] {
    let hash = noise(layer, x, y);
    let range = jitter * 2 + 1;
    let delta = |shift: u32| ((hash >> shift) % range) as i32 - jitter as i32;
    [
        clamp_channel(i32::from(base[0]) + delta(0)),
        clamp_channel(i32::from(base[1]) + delta(8)),
        clamp_channel(i32::from(base[2]) + delta(16)),
    ]
}

/// Clamps an 8-bit colour channel to its valid range.
fn clamp_channel(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Darkens or lightens every channel by `delta`.
fn shade(base: [u8; 3], delta: i32) -> [u8; 3] {
    [
        clamp_channel(i32::from(base[0]) + delta),
        clamp_channel(i32::from(base[1]) + delta),
        clamp_channel(i32::from(base[2]) + delta),
    ]
}

fn debug_pixel(x: u32, y: u32) -> [u8; 3] {
    if (x / 4 + y / 4).is_multiple_of(2) {
        [235, 44, 208]
    } else {
        [32, 32, 38]
    }
}

fn grass_top_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let color = varied([92, 150, 64], layer, x, y, 18);
    if noise(layer, x, y).is_multiple_of(29) {
        shade(color, -28)
    } else {
        color
    }
}

fn grass_side_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    // Four or five grass rows with a ragged lower edge, then dirt.
    let band = 4 + noise(layer, x, 0x5a) % 2;
    if y < band || (y == band && noise(layer, x, 0x3c).is_multiple_of(3)) {
        grass_top_pixel(layer, x, y)
    } else {
        dirt_pixel(layer, x, y)
    }
}

fn dirt_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let color = varied([134, 98, 66], layer, x, y, 15);
    if noise(layer, x, y).is_multiple_of(31) {
        shade(color, -26)
    } else {
        color
    }
}

fn stone_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let color = varied([126, 126, 132], layer, x, y, 11);
    if noise(layer, x, y).is_multiple_of(23) {
        shade(color, -38)
    } else {
        color
    }
}

fn sand_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    varied([221, 208, 160], layer, x, y, 9)
}

fn water_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    // Slight diagonal wave banding, then a little texel noise.
    let striped = ((x + y * 2) / 4).is_multiple_of(2);
    let base = if striped { [52, 112, 198] } else { [62, 122, 208] };
    varied(base, layer, x, y, 4)
}

fn log_side_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let base = match x % 4 {
        0 => [86, 58, 34],
        1 => [134, 96, 58],
        _ => [114, 80, 46],
    };
    varied(base, layer, x, y, 7)
}

fn log_top_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    // Concentric rings from squared integer distance: no `sqrt`, so the
    // pattern is bit-identical on every target.
    let dx = x as i32 * 2 - 15;
    let dy = y as i32 * 2 - 15;
    let radius_sq = (dx * dx + dy * dy) as u32;
    let ring = (radius_sq / 32).is_multiple_of(2);
    let base = if ring { [160, 118, 72] } else { [128, 90, 52] };
    varied(base, layer, x, y, 5)
}

fn leaves_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let hash = noise(layer, x, y);
    if hash.is_multiple_of(37) {
        [22, 52, 22]
    } else if hash.is_multiple_of(17) {
        shade([44, 98, 40], 18)
    } else {
        varied([44, 98, 40], layer, x, y, 16)
    }
}

fn bedrock_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let color = varied([38, 38, 44], layer, x, y, 12);
    if noise(layer, x, y).is_multiple_of(19) {
        shade(color, 26)
    } else {
        color
    }
}

fn gravel_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    let hash = noise(layer, x, y);
    let base = if hash % 5 < 2 { [122, 104, 86] } else { [132, 130, 126] };
    varied(base, layer, x, y, 12)
}

fn clay_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    varied([158, 166, 180], layer, x, y, 7)
}

fn snow_pixel(layer: u32, x: u32, y: u32) -> [u8; 3] {
    varied([242, 246, 252], layer, x, y, 3)
}

#[cfg(test)]
mod tests {
    use ob_content::textures as catalogue;

    use super::*;

    fn pixel(data: &[u8], layer: usize, x: usize, y: usize) -> [u8; 3] {
        let offset = (layer * BYTES_PER_LAYER) + (y * TEXTURE_SIZE as usize + x) * 4;
        [data[offset], data[offset + 1], data[offset + 2]]
    }

    #[test]
    fn data_has_one_rgba_layer_per_catalogue_name() {
        let data = generate_texture_data();
        assert_eq!(data.len(), catalogue::COUNT * BYTES_PER_LAYER);
        assert_eq!(data.len() % BYTES_PER_LAYER, 0);
    }

    #[test]
    fn generation_is_deterministic_and_fully_opaque() {
        let first = generate_texture_data();
        let second = generate_texture_data();
        assert_eq!(first, second);
        assert!(first.as_chunks::<4>().0.iter().all(|texel| texel[3] == 255));
    }

    #[test]
    fn every_layer_varies() {
        let data = generate_texture_data();
        for layer in 0..catalogue::COUNT {
            let first = pixel(&data, layer, 0, 0);
            let differs = (0..TEXTURE_SIZE as usize)
                .any(|y| (0..TEXTURE_SIZE as usize).any(|x| pixel(&data, layer, x, y) != first));
            assert!(differs, "layer {} ({}) is uniform", layer, catalogue::NAMES[layer]);
        }
    }

    #[test]
    fn debug_layer_is_a_checkerboard() {
        let data = generate_texture_data();
        let layer = catalogue::DEBUG.0 as usize;
        assert_ne!(pixel(&data, layer, 0, 0), pixel(&data, layer, 5, 0));
        assert_eq!(pixel(&data, layer, 0, 0), pixel(&data, layer, 4, 4));
    }

    #[test]
    fn grass_side_has_a_green_band_over_brown_dirt() {
        let data = generate_texture_data();
        let layer = catalogue::GRASS_SIDE.0 as usize;
        for x in 0..TEXTURE_SIZE as usize {
            let top = pixel(&data, layer, x, 0);
            assert!(
                top[1] > top[0] + 20 && top[1] > top[2] + 20,
                "grass row at x={x} is not green: {top:?}"
            );
            let bottom = pixel(&data, layer, x, TEXTURE_SIZE as usize - 1);
            assert!(
                bottom[0] > bottom[1] && bottom[1] > bottom[2],
                "dirt row at x={x} is not brown: {bottom:?}"
            );
        }
    }

    #[test]
    fn log_top_has_more_than_one_ring() {
        let data = generate_texture_data();
        let layer = catalogue::LOG_TOP.0 as usize;
        let center = pixel(&data, layer, 7, 7);
        let edge = pixel(&data, layer, 0, 7);
        assert_ne!(center, edge, "the centre and rim should fall on different rings");
    }
}

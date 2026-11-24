//! Procedural block-texture array generated at start-up.
//!
//! One RGBA8 layer is generated per entry of [`ob_content::textures::NAMES`];
//! the layers live in a single `Rgba8UnormSrgb` 2D array with a full mip chain.
//! Four selectable styles are available through [`TexturePackStyle`]:
//! the 16² [`TexturePackStyle::Classic`] default, 32² painterly and lush packs,
//! and a flat 16² [`TexturePackStyle::Minimal`] pack. Generation uses a small
//! integer hash only, so the result is seed-free, deterministic and identical on
//! every platform.
//!
//! The world and UI share the same array, view and sampler. Magnification stays
//! nearest-filtered for crisp pixel art while minification and mip selection are
//! linear, so distant blocks do not shimmer.

#[path = "texture_packs.rs"]
pub mod texture_packs;

pub use texture_packs::{TexturePack, TexturePackStyle, generate_texture_pack};

use ob_content::textures::{COUNT, NAMES};

/// Edge length of one layer in the default [`TexturePackStyle::Classic`] pack.
///
/// Other styles choose their own resolution; see
/// [`TexturePackStyle::resolution`].
pub const TEXTURE_SIZE: u32 = 16;

// The catalogue the renderer generates and the engine's block table must agree.
const _: () = assert!(NAMES.len() == COUNT);

/// The GPU texture, its array view and the shared sampler.
///
/// The UI renderer borrows [`Self::view`] and [`Self::sampler`] so block icons
/// share the same procedural layers, filtering and repeat behaviour as world
/// geometry.
pub struct BlockTextureArray {
    /// The 2D-array texture holding every block layer and its mip chain.
    pub(crate) _texture: wgpu::Texture,
    /// D2Array view bound by the chunk pipeline.
    pub(crate) view: wgpu::TextureView,
    /// Repeat sampler shared by every layer.
    pub(crate) sampler: wgpu::Sampler,
    /// Style this array was generated from.
    style: TexturePackStyle,
    /// Base-level edge length in texels.
    resolution: u32,
}

impl BlockTextureArray {
    /// Uploads the default [`TexturePackStyle::Classic`] catalogue.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self::new_with_style(device, queue, TexturePackStyle::Classic)
    }

    /// Uploads one style's catalogue, including a full box-filtered mip chain.
    ///
    /// Layers keep the [`NAMES`] order and use `Rgba8UnormSrgb`; the sampler
    /// magnifies with nearest filtering and minifies with linear filtering.
    pub fn new_with_style(device: &wgpu::Device, queue: &wgpu::Queue, style: TexturePackStyle) -> Self {
        let pack = TexturePack::generate(style);
        let resolution = pack.resolution();
        let mip_levels = pack.mip_level_count();
        debug_assert_eq!(mip_levels, style.resolution().trailing_zeros() + 1);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("block-textures"),
            size: wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: COUNT as u32,
            },
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for level in 0..mip_levels {
            let edge = resolution >> level;
            let pixels = pack.mip(level).expect("every mip level in the pack is uploaded");
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(edge * 4),
                    rows_per_image: Some(edge),
                },
                wgpu::Extent3d {
                    width: edge,
                    height: edge,
                    depth_or_array_layers: COUNT as u32,
                },
            );
        }

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
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            lod_min_clamp: 0.0,
            lod_max_clamp: (mip_levels - 1) as f32,
            compare: None,
            anisotropy_clamp: 1,
            border_color: None,
        });

        Self {
            _texture: texture,
            view,
            sampler,
            style,
            resolution,
        }
    }

    /// The `D2Array` view of every block texture layer and mip level.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The repeat sampler shared with the world pipeline.
    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// The style this array was generated from.
    pub fn style(&self) -> TexturePackStyle {
        self.style
    }

    /// Base-level edge length in texels.
    pub fn resolution(&self) -> u32 {
        self.resolution
    }
}

/// Generates the default (classic) catalogue's base mip as tightly packed RGBA8.
///
/// Layer `i` corresponds to `NAMES[i]` and is exactly
/// `TEXTURE_SIZE * TEXTURE_SIZE` pixels of four bytes. This compatibility helper
/// returns mip 0 only; GPU uploads use [`TexturePack`] for the full chain.
pub fn generate_texture_data() -> Vec<u8> {
    let pack = TexturePack::generate(TexturePackStyle::Classic);
    pack.mip(0).expect("the classic pack has a base mip").to_vec()
}

#[cfg(test)]
mod tests {
    use ob_content::textures as catalogue;

    use super::*;

    /// Bytes of one default-style RGBA8 layer, without mips.
    const BYTES_PER_LAYER: usize = (TEXTURE_SIZE * TEXTURE_SIZE * 4) as usize;

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
    fn generation_is_deterministic_and_only_leaves_are_cut_out() {
        let first = generate_texture_data();
        let second = generate_texture_data();
        assert_eq!(first, second);
        let leaves = catalogue::LEAVES.0 as usize;
        let mut had_hole = false;
        let mut had_solid = false;
        for (index, texel) in first.as_chunks::<4>().0.iter().enumerate() {
            let layer = index / (TEXTURE_SIZE * TEXTURE_SIZE) as usize;
            if layer == leaves {
                assert!(texel[3] == 0 || texel[3] == 255);
                had_hole |= texel[3] == 0;
                had_solid |= texel[3] == 255;
            } else {
                assert_eq!(texel[3], 255, "layer {layer} must stay opaque");
            }
        }
        assert!(had_hole, "leaf cut-outs should punch transparent holes");
        assert!(had_solid, "leaf cut-outs should leave most of the tile opaque");
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

    #[test]
    fn new_defaults_to_classic() {
        assert_eq!(TexturePackStyle::default(), TexturePackStyle::Classic);
        assert_eq!(TEXTURE_SIZE, TexturePackStyle::Classic.resolution());
    }
}

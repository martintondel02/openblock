//! Tiling cloud-density texture used by the terrain/water cloud shadows.
//!
//! The sky pass evaluates procedural value noise per pixel because it must
//! produce detailed, domain-warped cloud edges. Doing the same four-octave
//! noise on every terrain and water fragment is far too expensive: the v1.4
//! measured cost of the per-fragment version was about 4.5 ms at 1080p High.
//! Instead this module bakes the same value-noise family into a small tiling
//! texture once at start-up. Ground fragments then fetch one bilinear sample
//! and apply the same coverage threshold the sky uses, so the moving cloud
//! shadows stay visually coherent with the cloud layer at a fraction of the
//! cost.
//!
//! The texture is generated deterministically from the fixed seed below; it is
//! not tied to the world seed because cloud shadows are presentation only and
//! must not disclose or depend on world state.

/// Edge length of the baked cloud-density texture in texels.
pub(crate) const CLOUD_NOISE_SIZE: u32 = 128;

/// Fixed generation seed. Changing it changes the cloud field shape; the
/// renderer's visuals are deterministic for a binary, not across versions.
const CLOUD_NOISE_SEED: u64 = 0x434C_4F55_4453_4831;

/// The GPU texture, its view and the linear/repeat sampler used by terrain.
pub(crate) struct CloudNoise {
    /// Kept alive for the view.
    _texture: wgpu::Texture,
    /// Sampled by the near and far terrain/water shaders.
    pub(crate) view: wgpu::TextureView,
    /// Linear filtering with repeat addressing.
    pub(crate) sampler: wgpu::Sampler,
}

/// Small deterministic integer hash in `0.0..1.0`.
fn hash01(x: u32, y: u32, salt: u64) -> f32 {
    let hash = ob_math::mix64(
        CLOUD_NOISE_SEED
            ^ u64::from(x).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(y).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ salt,
    );
    (hash >> 40) as f32 / (1u64 << 24) as f32
}

/// Value noise on a lattice that wraps at `period` cells across the unit
/// square. This is what makes the baked texture tile without a visible seam.
fn tiling_value_noise(u: f32, v: f32, period: u32, salt: u64) -> f32 {
    let scaled_u = u * period as f32;
    let scaled_v = v * period as f32;
    let cell_u = scaled_u.floor() as i32;
    let cell_v = scaled_v.floor() as i32;
    let local_u = scaled_u - cell_u as f32;
    let local_v = scaled_v - cell_v as f32;
    let blend_u = local_u * local_u * (3.0 - 2.0 * local_u);
    let blend_v = local_v * local_v * (3.0 - 2.0 * local_v);
    let wrap = |value: i32| value.rem_euclid(period as i32) as u32;
    let sample = |du: i32, dv: i32| {
        hash01(
            wrap(cell_u + du),
            wrap(cell_v + dv),
            salt.wrapping_add(u64::from(period)),
        )
    };
    let a = sample(0, 0);
    let b = sample(1, 0);
    let c = sample(0, 1);
    let d = sample(1, 1);
    let top = a + (b - a) * blend_u;
    let bottom = c + (d - c) * blend_u;
    top + (bottom - top) * blend_v
}

/// Baked cloud density for one texel in `0.0..=1.0`.
///
/// Three octaves at period 1/2/4 with amplitudes 0.55/0.275/0.1375, matching the
/// sky shader's `cloud_noise` family closely enough that a shadow projected
/// from this field crosses the ground where the sky cloud does.
pub(crate) fn density_at(u: f32, v: f32) -> f32 {
    let octave0 = tiling_value_noise(u, v, 1, 0x434C_4F55_4430);
    let octave1 = tiling_value_noise(u, v, 2, 0x434C_4F55_4431);
    let octave2 = tiling_value_noise(u, v, 4, 0x434C_4F55_4432);
    let sum = octave0 * 0.55 + octave1 * 0.275 + octave2 * 0.1375;
    (sum / 0.9625).clamp(0.0, 1.0)
}

impl CloudNoise {
    /// Generates and uploads the texture.
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let size = CLOUD_NOISE_SIZE;
        let mut texels = Vec::with_capacity((size * size) as usize);
        for y in 0..size {
            for x in 0..size {
                let u = (x as f32 + 0.5) / size as f32;
                let v = (y as f32 + 0.5) / size as f32;
                texels.push((density_at(u, v) * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cloud-shadow-density"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
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
            &texels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cloud-shadow-sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_density_is_deterministic_and_in_range() {
        let first = density_at(0.3, 0.7);
        let second = density_at(0.3, 0.7);
        assert_eq!(first, second);
        for y in 0..32 {
            for x in 0..32 {
                let value = density_at(x as f32 / 32.0, y as f32 / 32.0);
                assert!((0.0..=1.0).contains(&value), "density out of range: {value}");
            }
        }
    }

    #[test]
    fn cloud_density_tiles_without_a_seam() {
        // Sampling the same wrapped coordinate must produce the same value.
        for y in [0.1f32, 0.37, 0.81] {
            let inside = density_at(0.25, y);
            let wrapped = density_at(1.25, y);
            assert!((inside - wrapped).abs() < 1e-6, "{inside} != {wrapped}");
        }
    }
}

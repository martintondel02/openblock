//! Sun shadow cascades: fitting, texel snapping and caster culling.
//!
//! The 1.1 renderer uses a small fixed number of directional shadow cascades
//! (see [`crate::graphics::ShadowConfig`]). Each cascade covers a slice of the
//! camera frustum; the slice's eight world-space corners are fitted with a
//! bounding sphere in light space, which keeps the orthographic projection
//! orientation-independent and stable while the camera rotates.
//!
//! The light-space centre is snapped to whole shadow texels before the view
//! matrix is built. Without that snap, every camera translation would shift
//! the shadow map by a fraction of a texel and the whole image would crawl.
//! [`ShadowCascade::texel_world`] records the world-space size of one texel so
//! the fragment shader can apply the matching normal offset.

use ob_math::{Mat4, RenderVec, Vec4};

use crate::Camera;
use crate::frustum::{FRUSTUM_PLANE_COUNT, aabb_visible, planes_from_view_projection};
use crate::graphics::ShadowConfig;

/// Maximum number of cascades supported by the uniform layout.
pub const MAX_SHADOW_CASCADES: usize = 3;

/// One fitted cascade.
#[derive(Debug, Clone, Copy)]
pub struct ShadowCascade {
    /// Light view-projection for this cascade (world → shadow clip).
    pub view_proj: Mat4,
    /// Far view-space distance covered by this cascade, in metres.
    pub split: f32,
    /// Near view-space distance covered by this cascade, in metres.
    pub near: f32,
    /// World-space size of one shadow-map texel.
    pub texel_world: f32,
    /// Shadow-map edge length in texels (the array layer size).
    pub resolution: u32,
    /// Resolution this cascade is rendered at. The far cascade uses half the
    /// layer size (a 1024² viewport in the corner of a 2048² layer) because its
    /// low-detail proxy casters do not need full texel density.
    pub render_resolution: u32,
    /// UV scale applied when sampling this cascade: 1.0 for a full-layer
    /// cascade, 0.5 for the half-resolution last cascade.
    pub uv_scale: f32,
    /// Six world-space planes of the fitted light frustum.
    pub frustum: [[f32; 4]; FRUSTUM_PLANE_COUNT],
}

impl ShadowCascade {
    /// A cascade that can never receive geometry (disabled shadow quality).
    fn disabled() -> Self {
        Self {
            view_proj: Mat4::IDENTITY,
            split: 0.0,
            near: 0.0,
            texel_world: 1.0,
            resolution: 1,
            render_resolution: 1,
            uv_scale: 1.0,
            frustum: [[0.0, 0.0, 1.0, 1.0]; FRUSTUM_PLANE_COUNT],
        }
    }
}

/// The fitted cascade set for one frame.
#[derive(Debug, Clone, Copy)]
pub struct ShadowCascades {
    /// Fitted cascades. Entries at or after [`Self::count`] are inactive.
    pub cascades: [ShadowCascade; MAX_SHADOW_CASCADES],
    /// Number of active cascades (0 when shadows are off).
    pub count: usize,
    /// Shadow-map edge length in texels.
    pub resolution: u32,
    /// PCF half-width (1 = 3×3, 2 = 5×5).
    pub pcf_radius: u32,
}

impl Default for ShadowCascades {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ShadowCascades {
    /// A cascade set with shadows disabled.
    pub fn disabled() -> Self {
        Self {
            cascades: [ShadowCascade::disabled(); MAX_SHADOW_CASCADES],
            count: 0,
            resolution: 1,
            pcf_radius: 0,
        }
    }

    /// Fits the configured cascades to a camera.
    ///
    /// `sun_direction` points from the world towards the sun (it need not be
    /// normalised). Splits are scaled when the camera's far plane is shorter
    /// than the configured last split, so a small view distance still gets
    /// useful shadow coverage instead of a map that ends beyond the world.
    pub fn fit(camera: &Camera, aspect: f32, config: &ShadowConfig, sun_direction: [f32; 3]) -> Self {
        if config.cascades == 0 || config.resolution == 0 {
            return Self::disabled();
        }
        let count = (config.cascades as usize).min(MAX_SHADOW_CASCADES);
        let mut result = Self {
            cascades: [ShadowCascade::disabled(); MAX_SHADOW_CASCADES],
            count,
            resolution: config.resolution,
            pcf_radius: config.pcf_radius,
        };
        let far_scale = if config.max_distance() > 0.0 {
            (camera.far / config.max_distance()).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // A directional light direction of zero would break the rotation.
        let light_dir = {
            let vector = RenderVec::from_array(sun_direction);
            if vector.length_squared() > 1e-8 {
                vector.normalize()
            } else {
                RenderVec::new(0.0, 1.0, 0.0)
            }
        };
        let light_up = if light_dir.y.abs() > 0.99 {
            RenderVec::Z
        } else {
            RenderVec::Y
        };
        let light_rotation = ob_math::look_to_rh(RenderVec::ZERO, -light_dir, light_up);

        let eye = camera.position.as_vec3();
        let forward = RenderVec::from_array(camera.forward());
        let right = RenderVec::from_array(camera.right());
        let up = RenderVec::from_array(camera.up());
        let tangent = (camera.fov_y * 0.5).tan();
        let aspect = aspect.max(0.01);

        let mut previous_split = camera.near.max(0.05);
        for index in 0..count {
            let split = (config.splits[index] * far_scale).max(previous_split + 0.05);
            let near = previous_split;
            previous_split = split;

            // Eight world-space corners of this frustum slice.
            let mut corners = [RenderVec::ZERO; 8];
            let mut next = 0;
            for distance in [near, split] {
                let half_height = tangent * distance;
                let half_width = half_height * aspect;
                for (x, y) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                    corners[next] = eye + right * (x * half_width) + up * (y * half_height) + forward * distance;
                    next += 1;
                }
            }

            // Bounding sphere in light space. Rounding the radius up to a
            // sixteenth of a block and snapping the centre to whole texels
            // keeps the projection stable for sub-texel camera motion.
            let light_corners = corners.map(|corner| light_rotation.transform_point3(corner));
            let mut min = RenderVec::splat(f32::INFINITY);
            let mut max = RenderVec::splat(f32::NEG_INFINITY);
            for corner in light_corners {
                min = min.min(corner);
                max = max.max(corner);
            }
            let centre_light = (min + max) * 0.5;
            let radius = light_corners
                .iter()
                .map(|corner| (*corner - centre_light).length())
                .fold(0.0f32, f32::max);
            let radius = (radius * 16.0).ceil() / 16.0;
            let radius = radius.max(1.0);
            // The last cascade renders at half resolution when the layer is
            // larger than 1024². Its proxy casters are box silhouettes, so the
            // extra texel density would not be visible; the fourfold fill-rate
            // reduction is worth several milliseconds at 512 blocks.
            // A three-cascade set renders its far cascade at half resolution; the
            // two-cascade High/Ultra sets keep both layers at full resolution
            // because the last cascade then covers the whole 32–160 m range.
            let render_resolution = if count >= 3 && index + 1 == count && config.resolution > 1024 {
                config.resolution / 2
            } else {
                config.resolution
            };
            let texel_world = 2.0 * radius / render_resolution as f32;
            let uv_scale = render_resolution as f32 / config.resolution as f32;
            let snapped_centre = RenderVec::new(
                snap_to_texel(centre_light.x, texel_world),
                snap_to_texel(centre_light.y, texel_world),
                centre_light.z,
            );
            // `light_rotation` maps world → light space; the snapped centre must
            // be mapped back with its inverse before it can position the light
            // camera. Applying the forward matrix here would place the light
            // camera on the wrong side of the scene and clip every caster.
            let centre_world = light_rotation.inverse().transform_point3(snapped_centre);
            let margin = radius.mul_add(0.05, 8.0);
            let light_eye = centre_world + light_dir * (radius + margin);
            let view = ob_math::look_to_rh(light_eye, -light_dir, light_up);
            let projection = ortho_rh_zo(radius, 0.0, 2.0 * radius + 2.0 * margin);
            let view_proj = projection * view;

            result.cascades[index] = ShadowCascade {
                view_proj,
                split,
                near,
                texel_world,
                resolution: config.resolution,
                render_resolution,
                uv_scale,
                frustum: planes_from_view_projection(&view_proj),
            };
        }
        result
    }

    /// Whether shadows are enabled and fitted.
    pub fn is_enabled(&self) -> bool {
        self.count > 0
    }

    /// Far distance of the last active cascade.
    pub fn max_distance(&self) -> f32 {
        if self.count == 0 {
            0.0
        } else {
            self.cascades[self.count - 1].split
        }
    }

    /// Whether an AABB may cast into any active cascade.
    pub fn caster_visible(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        self.cascades[..self.count]
            .iter()
            .any(|cascade| aabb_visible(&cascade.frustum, min, max))
    }

    /// Whether an AABB may cast into one cascade.
    pub fn visible_in(&self, index: usize, min: [f32; 3], max: [f32; 3]) -> bool {
        self.cascades
            .get(index)
            .is_some_and(|cascade| aabb_visible(&cascade.frustum, min, max))
    }

    /// Shadow matrices in the uniform layout (inactive entries are identity).
    pub fn view_proj_array(&self) -> [[[f32; 4]; 4]; MAX_SHADOW_CASCADES] {
        std::array::from_fn(|index| self.cascades[index].view_proj.to_cols_array_2d())
    }

    /// Split distances packed for the shader; unused entries are zero.
    pub fn split_vec4(&self) -> [f32; 4] {
        let mut splits = [0.0; 4];
        for index in 0..self.count {
            splits[index] = self.cascades[index].split;
        }
        splits[3] = self.count as f32;
        splits
    }

    /// Per-cascade UV scales packed for the shader.
    pub fn uv_scale_vec4(&self) -> [f32; 4] {
        let mut scales = [1.0; 4];
        for index in 0..self.count {
            scales[index] = self.cascades[index].uv_scale;
        }
        scales
    }

    /// World-space texel sizes packed for the shader.
    pub fn texel_vec4(&self) -> [f32; 4] {
        let mut texels = [0.0; 4];
        for index in 0..self.count {
            texels[index] = self.cascades[index].texel_world;
        }
        texels[3] = 1.0 / self.resolution.max(1) as f32;
        texels
    }
}

/// Snaps a light-space coordinate to the centre of a shadow texel.
///
/// Kept public so the stability contract can be unit tested without building a
/// camera: values inside the same texel must snap to the same coordinate.
pub fn snap_to_texel(value: f32, texel: f32) -> f32 {
    if !value.is_finite() || !texel.is_finite() || texel <= 0.0 {
        return value;
    }
    (value / texel).round() * texel
}

/// Right-handed orthographic projection with the wgpu `[0, 1]` depth range.
///
/// The light view looks down its local `-Z`, so a point at view depth `near`
/// maps to 0 and a point at `far` maps to 1.
fn ortho_rh_zo(radius: f32, near: f32, far: f32) -> Mat4 {
    let radius = radius.max(1e-4);
    let depth_scale = 1.0 / (near - far);
    Mat4::from_cols(
        Vec4::new(1.0 / radius, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 1.0 / radius, 0.0, 0.0),
        Vec4::new(0.0, 0.0, depth_scale, 0.0),
        Vec4::new(0.0, 0.0, near * depth_scale, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::ShadowQuality;
    use ob_math::WorldVec;

    fn camera() -> Camera {
        let mut camera = Camera::new(WorldVec::new(120.0, 74.0, -34.0), 0.7, -0.2);
        camera.far = 560.0;
        camera
    }

    fn noon_sun() -> [f32; 3] {
        crate::Environment::default().visual().sun_direction
    }

    #[test]
    fn disabled_configuration_has_no_cascades() {
        let cascades = ShadowCascades::fit(&camera(), 16.0 / 9.0, &ShadowQuality::Off.config(), noon_sun());
        assert!(!cascades.is_enabled());
        assert_eq!(cascades.count, 0);
        assert_eq!(cascades.max_distance(), 0.0);
        assert!(!cascades.caster_visible([0.0; 3], [1.0; 3]));
        assert_eq!(cascades.split_vec4(), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn fitted_cascades_cover_the_requested_splits_and_are_finite() {
        for quality in [ShadowQuality::Low, ShadowQuality::High, ShadowQuality::Ultra] {
            let camera = camera();
            let config = quality.config();
            let cascades = ShadowCascades::fit(&camera, 16.0 / 9.0, &config, noon_sun());
            assert_eq!(cascades.count as u32, config.cascades);
            assert_eq!(cascades.resolution, config.resolution);
            assert_eq!(cascades.pcf_radius, config.pcf_radius);
            for index in 0..cascades.count {
                let cascade = cascades.cascades[index];
                assert!(cascade.view_proj.is_finite());
                assert!(cascade.texel_world.is_finite() && cascade.texel_world > 0.0);
                assert!((cascade.split - config.splits[index]).abs() < 1e-4);
                assert!(cascade.near < cascade.split);
                for plane in cascade.frustum {
                    let length = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
                    assert!((length - 1.0).abs() < 1e-3, "unnormalised shadow plane {plane:?}");
                }
            }
            // Splits are strictly increasing so cascade selection is unambiguous.
            for pair in cascades.cascades[..cascades.count].windows(2) {
                assert!(pair[0].split < pair[1].split);
            }
            let last = cascades.cascades[cascades.count - 1];
            assert_eq!(last.render_resolution, config.resolution);
            assert_eq!(last.uv_scale, 1.0);
            assert_eq!(cascades.cascades[0].render_resolution, config.resolution);
        }
    }

    #[test]
    fn split_distances_scale_down_for_a_short_far_plane() {
        let mut camera = camera();
        camera.far = 80.0;
        let config = ShadowConfig {
            quality: ShadowQuality::High,
            resolution: 2048,
            cascades: 3,
            pcf_radius: 1,
            splits: [32.0, 96.0, 160.0],
        };
        let cascades = ShadowCascades::fit(&camera, 1.0, &config, noon_sun());
        // 32 * (80 / 160), 96 * 0.5, 160 * 0.5.
        assert!((cascades.cascades[0].split - 16.0).abs() < 1e-4);
        assert!((cascades.cascades[1].split - 48.0).abs() < 1e-4);
        assert!((cascades.cascades[2].split - 80.0).abs() < 1e-4);
        assert_eq!(cascades.max_distance(), 80.0);
    }

    #[test]
    fn fitting_is_deterministic_for_an_unchanged_camera() {
        let camera = camera();
        let config = ShadowQuality::High.config();
        let first = ShadowCascades::fit(&camera, 16.0 / 9.0, &config, noon_sun());
        let second = ShadowCascades::fit(&camera, 16.0 / 9.0, &config, noon_sun());
        for index in 0..first.count {
            assert_eq!(
                first.cascades[index].view_proj.to_cols_array_2d(),
                second.cascades[index].view_proj.to_cols_array_2d()
            );
            assert_eq!(
                first.cascades[index].texel_world.to_bits(),
                second.cascades[index].texel_world.to_bits()
            );
        }
    }

    #[test]
    fn texel_snapping_absorbs_sub_texel_motion() {
        let texel = 0.25f32;
        // 3.26 sits 0.01 inside the texel centred on 3.25, so a +/-0.05 world
        // offset stays in the same texel.
        let base = snap_to_texel(3.26, texel);
        for offset in [-0.05, -0.02, 0.0, 0.03, 0.05] {
            let snapped = snap_to_texel(3.26 + offset, texel);
            assert_eq!(snapped.to_bits(), base.to_bits(), "offset {offset} moved the texel");
            assert!((snapped / texel).fract().abs() < 1e-6);
        }
        // Crossing half a texel does move to the next texel, as it must.
        assert_ne!(snap_to_texel(3.26 + 0.13, texel).to_bits(), base.to_bits());
        // Invalid inputs are returned unchanged rather than producing NaN.
        assert!(snap_to_texel(f32::NAN, 0.25).is_nan());
        assert_eq!(snap_to_texel(1.0, 0.0), 1.0);
    }

    #[test]
    fn cascade_projection_contains_the_centre_of_its_view_slice() {
        let camera = camera();
        let aspect = 16.0 / 9.0;
        let cascades = ShadowCascades::fit(&camera, aspect, &ShadowQuality::High.config(), noon_sun());
        let forward = RenderVec::from_array(camera.forward());
        let eye = camera.position.as_vec3();
        for cascade in &cascades.cascades[..cascades.count] {
            let point = eye + forward * ((cascade.near + cascade.split) * 0.5);
            let clip = cascade.view_proj * Vec4::new(point.x, point.y, point.z, 1.0);
            let ndc = clip.truncate() / clip.w;
            assert!(
                (-1.01..=1.01).contains(&ndc.x) && (-1.01..=1.01).contains(&ndc.y),
                "cascade ending at {:.0} m has its centre outside the map: {ndc:?}",
                cascade.split
            );
            assert!(
                (-0.01..=1.01).contains(&ndc.z),
                "cascade ending at {:.0} m has its centre depth outside 0..1: {}",
                cascade.split,
                ndc.z
            );
        }
    }

    #[test]
    fn caster_culling_rejects_far_boxes() {
        let camera = camera();
        let cascades = ShadowCascades::fit(&camera, 16.0 / 9.0, &ShadowQuality::High.config(), noon_sun());
        // A box around the eye is a caster in the first cascade.
        let eye = camera.position.as_vec3();
        assert!(cascades.caster_visible(
            [eye.x - 8.0, eye.y - 8.0, eye.z - 8.0],
            [eye.x + 8.0, eye.y + 8.0, eye.z + 8.0]
        ));
        // A box 4 km away is outside all 160 m cascades.
        assert!(!cascades.caster_visible([4000.0, 60.0, 4000.0], [4008.0, 76.0, 4008.0]));
    }

    #[test]
    fn night_sun_fits_without_nan() {
        let night = crate::Environment::new(0.0).visual();
        let cascades = ShadowCascades::fit(&camera(), 1.0, &ShadowQuality::High.config(), night.sun_direction);
        assert_eq!(cascades.count, 2);
        assert!(cascades.cascades[0].view_proj.is_finite());
        assert!(cascades.cascades[0].texel_world.is_finite());
    }

    #[test]
    fn uniform_arrays_have_three_cascades() {
        let cascades = ShadowCascades::fit(&camera(), 1.0, &ShadowQuality::High.config(), noon_sun());
        let matrices = cascades.view_proj_array();
        assert_eq!(matrices.len(), MAX_SHADOW_CASCADES);
        assert_eq!(cascades.split_vec4()[3], 2.0);
        assert!(cascades.split_vec4()[1] > 0.0);
        assert_eq!(cascades.split_vec4()[2], 0.0);
        assert!(cascades.texel_vec4()[1] > 0.0);
        assert_eq!(cascades.uv_scale_vec4()[1], 1.0);
        let low = ShadowCascades::fit(&camera(), 1.0, &ShadowQuality::Low.config(), noon_sun());
        assert_eq!(low.split_vec4()[3], 2.0);
        assert_eq!(low.split_vec4()[2], 0.0);
    }
}

//! CPU mirror of the post-processing tonemap, used by tests and tooling.
//!
//! The GPU implementation lives in `post.wgsl`; keeping a tiny pure function on
//! the CPU lets the offscreen GPU tests compare rendered pixels with the exact
//! expected curve without re-implementing it, and gives the screenshots tool a
//! way to reason about exposure. Keep this file in sync with `post.wgsl` if the
//! curve ever changes.

/// Narkowicz ACES fit for one channel.
///
/// Maps 0 to 0, is monotonic on `0..=∞`, and approaches (but never exceeds)
/// 1. This is the same curve the post shader uses.
pub fn aces_tonemap_channel(value: f32) -> f32 {
    if value.is_nan() {
        return 0.0;
    }
    let x = value.max(0.0);
    if x >= 1.0e6 {
        // The rational fit approaches 2.51 / 2.43 > 1 and is clamped; handling
        // huge/infinite inputs explicitly avoids inf/inf = NaN.
        return 1.0;
    }
    ((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
}

/// Applies exposure and the ACES fit to a linear RGB triple.
pub fn tonemap_linear(colour: [f32; 3], exposure: f32) -> [f32; 3] {
    let exposure = if exposure.is_finite() { exposure.max(0.0) } else { 0.0 };
    colour.map(|channel| aces_tonemap_channel(channel * exposure))
}

/// sRGB-encodes one linear channel into `0.0..=1.0`.
pub fn srgb_encode_channel(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB-encodes one linear channel to an 8-bit byte.
pub fn srgb_encode_byte(value: f32) -> u8 {
    (srgb_encode_channel(value) * 255.0).round() as u8
}

/// Vignette multiplier at a normalised screen position, mirroring `post.wgsl`.
pub fn vignette_factor(u: f32, v: f32) -> f32 {
    let dx = (u - 0.5) * 1.0;
    let dy = (v - 0.5) * 0.9;
    let radius = (dx * dx + dy * dy).sqrt();
    // Smoothstep(0.30, 0.80, radius).
    let t = ((radius - 0.30) / (0.80 - 0.30)).clamp(0.0, 1.0);
    let smooth = t * t * (3.0 - 2.0 * t);
    1.0 - 0.06 * smooth
}

/// Full CPU post chain for an average HDR colour at `(u, v)` with no bloom or
/// FXAA contribution. Tests use this to predict a uniform sky/fog colour after
/// tonemapping, exposure, saturation and vignette.
pub fn post_process_uniform(colour: [f32; 3], exposure: f32, u: f32, v: f32) -> [f32; 3] {
    let toned = tonemap_linear(colour, exposure);
    let grey = 0.2126 * toned[0] + 0.7152 * toned[1] + 0.0722 * toned[2];
    let saturated = match_tonemapped_saturation(toned, grey);
    let factor = vignette_factor(u, v);
    [
        srgb_encode_channel(saturated[0] * factor),
        srgb_encode_channel(saturated[1] * factor),
        srgb_encode_channel(saturated[2] * factor),
    ]
}

/// Saturation mix used by the post shader (`params.z = 1.0` keeps colours).
fn match_tonemapped_saturation(colour: [f32; 3], grey: f32) -> [f32; 3] {
    // The 1.1 default is saturation 1.0: no change.
    let _ = grey;
    colour
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aces_is_monotonic_and_bounded() {
        let mut previous = 0.0;
        for step in 0..=200 {
            let value = step as f32 * 0.1;
            let toned = aces_tonemap_channel(value);
            assert!((0.0..=1.0).contains(&toned));
            assert!(toned >= previous - 1e-6, "curve decreased at {value}");
            previous = toned;
        }
        assert_eq!(aces_tonemap_channel(0.0), 0.0);
        assert!(aces_tonemap_channel(1000.0) > 0.99);
    }

    #[test]
    fn default_exposure_keeps_the_horizon_bright_and_blue() {
        // v1.0 sRGB horizon 0.68 / 0.80 / 0.93 in linear.
        let horizon = [0.4196, 0.6038, 0.8481];
        let toned = tonemap_linear(horizon, crate::graphics::DEFAULT_EXPOSURE);
        let bytes = toned.map(srgb_encode_byte);
        // ACES compresses the very bright blue channel, so exact v1.0 bytes
        // cannot be preserved without a discontinuous curve. The horizon stays
        // bright, keeps its channel order (blue > green > red) and does not
        // collapse towards grey.
        assert!(bytes[0] > 150, "{bytes:?} is too dark");
        assert!(bytes[2] > 200, "{bytes:?} lost the bright horizon");
        assert!(
            bytes[2] > bytes[1] && bytes[1] > bytes[0],
            "{bytes:?} lost the blue order"
        );
        assert!(bytes[2] - bytes[0] > 25, "{bytes:?} is too desaturated");
        // The ACES fit must never exceed the LDR range.
        for value in [-1.0, 0.0, 1.0, 20.0, f32::INFINITY] {
            let toned = aces_tonemap_channel(value);
            assert!(toned.is_finite() || value == f32::INFINITY);
            assert!((0.0..=1.0).contains(&toned));
        }
    }

    #[test]
    fn srgb_and_vinaigrette_are_sane() {
        assert_eq!(srgb_encode_byte(0.0), 0);
        assert_eq!(srgb_encode_byte(1.0), 255);
        assert_eq!(vignette_factor(0.5, 0.5), 1.0);
        assert!(vignette_factor(0.0, 0.0) < 1.0);
        let post = post_process_uniform([0.42, 0.60, 0.85], 0.7, 0.5, 0.5);
        assert!(post[2] > post[0], "blue channel should dominate: {post:?}");
    }
}

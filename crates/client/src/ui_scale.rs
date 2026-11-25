//! GUI-scale resolution and coordinate mapping.
//!
//! OpenBlock builds every UI frame in *logical* UI pixels and lets the GPU map
//! that frame onto the physical target. The scale transform is therefore
//! applied exactly once: [`ui_screen`] divides the physical window/screenshot
//! size by the scale factor, every screen is laid out and hit-tested in those
//! logical pixels, and [`ob_render::UiFrame`] is created with the logical size
//! so its normalised-device-coordinate conversion also scales every vertex,
//! glyph and panel border by the same factor.
//!
//! Pointer positions arrive in physical pixels. They must be divided by the
//! same factor ([`physical_to_ui_point`]) before any screen receives them.
//! Keeping these helpers pure makes the mapping and the 75/100/125/150%
//! screenshot matrix testable without a window or GPU.

use ob_core::config::{UI_SCALE_AUTO, UI_SCALE_CHOICES, normalise_ui_scale_percent};

/// Percentage used when `Auto` has no better information (720p and below).
pub const AUTO_UI_SCALE_BASE_PERCENT: u16 = 100;
/// Physical height at which `Auto` selects 125%.
pub const AUTO_UI_SCALE_125_MIN_HEIGHT: f32 = 1200.0;
/// Physical height at which `Auto` selects 150%.
pub const AUTO_UI_SCALE_150_MIN_HEIGHT: f32 = 1800.0;

/// Resolves the effective GUI-scale percentage for a physical target.
///
/// `Auto` keeps the 1280x720 and 960x540 release layouts at their 100%
/// baseline, selects 125% on 1440p-class displays and 150% on 4K-class
/// displays. Manual choices are normalised to the supported discrete set.
pub fn resolved_ui_scale_percent(setting: u16, physical_height: f32) -> u16 {
    if setting != UI_SCALE_AUTO {
        return normalise_ui_scale_percent(setting);
    }
    let height = if physical_height.is_finite() {
        physical_height
    } else {
        720.0
    };
    if height >= AUTO_UI_SCALE_150_MIN_HEIGHT {
        150
    } else if height >= AUTO_UI_SCALE_125_MIN_HEIGHT {
        125
    } else {
        AUTO_UI_SCALE_BASE_PERCENT
    }
}

/// Scale factor (`physical / logical`) for a setting and physical height.
pub fn ui_scale_factor(setting: u16, physical_height: f32) -> f32 {
    f32::from(resolved_ui_scale_percent(setting, physical_height)) / 100.0
}

/// Logical UI size for a physical target and GUI-scale setting.
///
/// The result is never smaller than 1x1; non-finite or zero inputs are
/// coerced to 1 so a bad window size cannot poison layout arithmetic.
pub fn ui_screen(physical: [f32; 2], setting: u16) -> [f32; 2] {
    let width = if physical[0].is_finite() {
        physical[0].max(1.0)
    } else {
        1.0
    };
    let height = if physical[1].is_finite() {
        physical[1].max(1.0)
    } else {
        1.0
    };
    let factor = ui_scale_factor(setting, height);
    [(width / factor).max(1.0), (height / factor).max(1.0)]
}

/// Converts a physical-pixel point to logical UI coordinates.
pub fn physical_to_ui_point(point: (f32, f32), factor: f32) -> (f32, f32) {
    if !factor.is_finite() || factor <= 0.0 {
        return point;
    }
    (point.0 / factor, point.1 / factor)
}

/// Converts a logical UI point back to physical pixels (tests and probes).
pub fn ui_to_physical_point(point: (f32, f32), factor: f32) -> (f32, f32) {
    if !factor.is_finite() || factor <= 0.0 {
        return point;
    }
    (point.0 * factor, point.1 * factor)
}

/// Repositions a logical cursor when the effective scale changes.
///
/// `cursor` is expressed in the old logical pixels. Multiplying by
/// `old_percent / new_percent` keeps the pointer over the same physical spot,
/// so changing GUI scale from the settings screen does not make the arrow jump
/// somewhere else under the mouse.
pub fn rescale_cursor(cursor: (f32, f32), old_percent: u16, new_percent: u16) -> (f32, f32) {
    let old = f32::from(old_percent.max(1)) / 100.0;
    let new = f32::from(new_percent.max(1)) / 100.0;
    if !old.is_finite() || !new.is_finite() || new <= 0.0 {
        return cursor;
    }
    (cursor.0 * old / new, cursor.1 * old / new)
}

/// F3/debug summary of the effective GUI scale for a physical target.
pub fn debug_line(setting: u16, physical: [f32; 2]) -> String {
    let percent = resolved_ui_scale_percent(setting, physical[1]);
    let logical = ui_screen(physical, setting);
    format!(
        "UI {percent}% ({}x{} logical)",
        logical[0].round() as u32,
        logical[1].round() as u32
    )
}

/// Human-readable value for the settings row, e.g. `Auto (100%)` or `125%`.
pub fn ui_scale_label(setting: u16, resolved_percent: u16) -> String {
    if setting == UI_SCALE_AUTO {
        format!("Auto ({resolved_percent}%)")
    } else {
        format!("{}%", normalise_ui_scale_percent(setting))
    }
}

/// Cycles the GUI-scale setting with Left/Right while keeping `Auto` useful.
///
/// `Auto` is treated as the nearest concrete choice for the current display,
/// so pressing Right on `Auto (100%)` selects 125% rather than wrapping to a
/// smaller explicit value.
pub fn cycle_ui_scale(current: u16, delta: i32, resolved_percent: u16) -> u16 {
    if delta == 0 {
        return current;
    }
    let base = if current == UI_SCALE_AUTO {
        normalise_ui_scale_percent(resolved_percent)
    } else {
        normalise_ui_scale_percent(current)
    };
    let index = UI_SCALE_CHOICES.iter().position(|choice| *choice == base).unwrap_or(1);
    let next = (index as i32 + delta.signum()).rem_euclid(UI_SCALE_CHOICES.len() as i32) as usize;
    UI_SCALE_CHOICES[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scale_keeps_release_resolutions_at_100_percent() {
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, 540.0), 100);
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, 720.0), 100);
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, 1080.0), 100);
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, 1440.0), 125);
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, 2160.0), 150);
        assert_eq!(resolved_ui_scale_percent(UI_SCALE_AUTO, f32::NAN), 100);
    }

    #[test]
    fn manual_scale_is_normalised_and_mapped_to_a_factor() {
        assert_eq!(resolved_ui_scale_percent(125, 720.0), 125);
        assert_eq!(resolved_ui_scale_percent(110, 720.0), 100);
        assert_eq!(resolved_ui_scale_percent(999, 720.0), 150);
        assert_eq!(ui_scale_factor(75, 720.0), 0.75);
        assert_eq!(ui_scale_factor(100, 720.0), 1.0);
        assert_eq!(ui_scale_factor(125, 720.0), 1.25);
        assert_eq!(ui_scale_factor(150, 720.0), 1.5);
    }

    #[test]
    fn logical_screens_preserve_the_release_baselines() {
        assert_eq!(ui_screen([1280.0, 720.0], 100), [1280.0, 720.0]);
        assert_eq!(ui_screen([960.0, 540.0], 100), [960.0, 540.0]);
        assert_eq!(ui_screen([960.0, 540.0], 125), [768.0, 432.0]);
        let logical = ui_screen([1280.0, 720.0], 150);
        assert!((logical[0] - 853.3333).abs() < 0.01);
        assert_eq!(logical[1], 480.0);
        // 75% buys more logical room for the same window.
        assert_eq!(ui_screen([960.0, 540.0], 75), [1280.0, 720.0]);
        // Degenerate input never yields zero-sized layouts.
        assert_eq!(ui_screen([0.0, f32::NAN], 150), [1.0, 1.0]);
    }

    #[test]
    fn pointer_mapping_round_trips_at_every_scale() {
        for percent in [75_u16, 100, 125, 150] {
            let factor = ui_scale_factor(percent, 720.0);
            let physical = (641.0, 359.0);
            let logical = physical_to_ui_point(physical, factor);
            let back = ui_to_physical_point(logical, factor);
            assert!((back.0 - physical.0).abs() < 0.001, "{percent}%");
            assert!((back.1 - physical.1).abs() < 0.001, "{percent}%");
            // The logical point is what the layout/hit-test sees.
            assert!((logical.0 * factor - physical.0).abs() < 0.001);
        }
    }

    #[test]
    fn a_click_at_a_logical_target_hits_after_physical_conversion() {
        let physical_target = [960.0, 540.0];
        for percent in [75_u16, 100, 125, 150] {
            let logical = ui_screen(physical_target, percent);
            let factor = ui_scale_factor(percent, physical_target[1]);
            // A button at the logical centre must map back to the physical
            // centre before the screen hit-tests it.
            let physical = ui_to_physical_point((logical[0] / 2.0, logical[1] / 2.0), factor);
            let cursor = physical_to_ui_point(physical, factor);
            assert!((cursor.0 - logical[0] / 2.0).abs() < 0.001, "{percent}%");
            assert!((cursor.1 - logical[1] / 2.0).abs() < 0.001, "{percent}%");
        }
    }

    #[test]
    fn cursor_rescaling_preserves_the_physical_pointer() {
        let cursor = (640.0, 360.0);
        let rescaled = rescale_cursor(cursor, 100, 125);
        assert_eq!(rescaled, (512.0, 288.0));
        let back = rescale_cursor(rescaled, 125, 100);
        assert_eq!(back, cursor);
    }

    #[test]
    fn auto_cycles_to_concrete_neighbours_in_the_right_direction() {
        assert_eq!(cycle_ui_scale(UI_SCALE_AUTO, 1, 100), 125);
        assert_eq!(cycle_ui_scale(UI_SCALE_AUTO, -1, 100), 75);
        assert_eq!(cycle_ui_scale(100, 1, 100), 125);
        assert_eq!(cycle_ui_scale(100, -1, 100), 75);
        assert_eq!(cycle_ui_scale(150, 1, 150), 75);
        assert_eq!(cycle_ui_scale(75, -1, 75), 150);
        assert_eq!(cycle_ui_scale(100, 0, 100), 100);
    }

    #[test]
    fn debug_line_names_the_effective_scale_and_logical_size() {
        assert_eq!(debug_line(UI_SCALE_AUTO, [1280.0, 720.0]), "UI 100% (1280x720 logical)");
        assert_eq!(debug_line(150, [1280.0, 720.0]), "UI 150% (853x480 logical)");
        assert_eq!(debug_line(125, [960.0, 540.0]), "UI 125% (768x432 logical)");
    }

    #[test]
    fn labels_show_the_auto_resolution_and_explicit_percent() {
        assert_eq!(ui_scale_label(UI_SCALE_AUTO, 100), "Auto (100%)");
        assert_eq!(ui_scale_label(UI_SCALE_AUTO, 150), "Auto (150%)");
        assert_eq!(ui_scale_label(125, 125), "125%");
        assert_eq!(ui_scale_label(999, 720), "150%");
    }
}

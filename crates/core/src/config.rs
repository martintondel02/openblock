//! User settings: loading, validation and atomic persistence.
//!
//! Settings live in `<data-dir>/config.json`. Unknown fields are preserved on
//! read (ignored) so newer builds can add options without breaking older ones,
//! and every value is clamped to a safe range on load before use.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name of the user settings document.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Smallest supported render distance in chunks.
pub const MIN_RENDER_DISTANCE: i32 = 2;
/// Largest render distance offered by the settings screen in v0.0.x.
pub const MAX_RENDER_DISTANCE: i32 = 24;
/// Smallest supported vertical field of view in degrees.
pub const MIN_FOV_DEGREES: f32 = 30.0;
/// Largest supported vertical field of view in degrees.
pub const MAX_FOV_DEGREES: f32 = 110.0;
/// Smallest supported mouse sensitivity multiplier.
pub const MIN_MOUSE_SENSITIVITY: f32 = 0.05;
/// Largest supported mouse sensitivity multiplier.
pub const MAX_MOUSE_SENSITIVITY: f32 = 3.0;

/// Persistent user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Streaming radius in chunks.
    pub render_distance_chunks: i32,
    /// Vertical field of view in degrees.
    pub fov_degrees: f32,
    /// Mouse look multiplier (1.0 = default).
    pub mouse_sensitivity: f32,
    /// Whether the window starts fullscreen.
    pub fullscreen: bool,
    /// Whether the F3 debug overlay is visible.
    pub show_debug_overlay: bool,
    /// Worker threads for chunk meshing; `None` selects the automatic default.
    pub mesh_workers: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_distance_chunks: 6,
            fov_degrees: 70.0,
            mouse_sensitivity: 1.0,
            fullscreen: false,
            show_debug_overlay: false,
            mesh_workers: None,
        }
    }
}

impl Settings {
    /// Clamps every value into its supported range, returning whether anything changed.
    pub fn clamp(&mut self) -> bool {
        let before = self.clone();
        self.render_distance_chunks = self
            .render_distance_chunks
            .clamp(MIN_RENDER_DISTANCE, MAX_RENDER_DISTANCE);
        if !self.fov_degrees.is_finite() {
            self.fov_degrees = Settings::default().fov_degrees;
        }
        self.fov_degrees = self.fov_degrees.clamp(MIN_FOV_DEGREES, MAX_FOV_DEGREES);
        if !self.mouse_sensitivity.is_finite() {
            self.mouse_sensitivity = Settings::default().mouse_sensitivity;
        }
        self.mouse_sensitivity = self
            .mouse_sensitivity
            .clamp(MIN_MOUSE_SENSITIVITY, MAX_MOUSE_SENSITIVITY);
        if let Some(workers) = self.mesh_workers {
            self.mesh_workers = Some(workers.clamp(1, 16));
        }
        before != *self
    }
}

/// Path of the settings document for a data directory.
pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE_NAME)
}

/// Loads settings from `<data_dir>/config.json`.
///
/// A missing file yields the defaults. A malformed file logs a warning and also
/// yields the defaults, because refusing to start over a corrupt settings file
/// would be hostile; the next save replaces it.
pub fn load_settings(data_dir: &Path) -> Settings {
    let path = settings_path(data_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Settings::default(),
        Err(err) => {
            log::warn!("could not read settings {}: {err}", path.display());
            return Settings::default();
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(mut settings) => {
            if settings.clamp() {
                log::warn!("settings in {} were out of range and have been clamped", path.display());
            }
            settings
        }
        Err(err) => {
            log::warn!("malformed settings {}: {err}; using defaults", path.display());
            Settings::default()
        }
    }
}

/// Saves settings atomically to `<data_dir>/config.json`.
pub fn save_settings(data_dir: &Path, settings: &Settings) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = settings_path(data_dir);
    let mut settings = sanitised(settings);
    settings.clamp();
    let text =
        serde_json::to_string_pretty(&settings).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text.as_bytes())?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

/// Copy of `settings` with non-finite floats replaced by defaults.
fn sanitised(settings: &Settings) -> Settings {
    let mut copy = settings.clone();
    if !copy.fov_degrees.is_finite() {
        copy.fov_degrees = Settings::default().fov_degrees;
    }
    if !copy.mouse_sensitivity.is_finite() {
        copy.mouse_sensitivity = Settings::default().mouse_sensitivity;
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openblock-core-config-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(load_settings(&dir), Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_round_trip() {
        let dir = temp_dir("roundtrip");
        let settings = Settings {
            render_distance_chunks: 12,
            fov_degrees: 90.0,
            mouse_sensitivity: 1.5,
            fullscreen: true,
            show_debug_overlay: true,
            mesh_workers: Some(6),
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir), settings);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_file_yields_defaults() {
        let dir = temp_dir("malformed");
        fs::write(settings_path(&dir), b"{ not json").expect("write");
        assert_eq!(load_settings(&dir), Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = temp_dir("clamp");
        fs::write(
            settings_path(&dir),
            br#"{"render_distance_chunks": 999, "fov_degrees": -10.0, "mouse_sensitivity": 50.0, "mesh_workers": 900}"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.render_distance_chunks, MAX_RENDER_DISTANCE);
        assert_eq!(settings.fov_degrees, MIN_FOV_DEGREES);
        assert_eq!(settings.mouse_sensitivity, MAX_MOUSE_SENSITIVITY);
        assert_eq!(settings.mesh_workers, Some(16));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let dir = temp_dir("unknown");
        fs::write(
            settings_path(&dir),
            br#"{"render_distance_chunks": 8, "future_option": {"nested": [1, 2, 3]}}"#,
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.render_distance_chunks, 8);
        assert_eq!(settings.fov_degrees, Settings::default().fov_degrees);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_sanitises_non_finite_values() {
        let dir = temp_dir("nan");
        let settings = Settings {
            fov_degrees: f32::NAN,
            mouse_sensitivity: f32::INFINITY,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.fov_degrees, Settings::default().fov_degrees);
        assert_eq!(loaded.mouse_sensitivity, Settings::default().mouse_sensitivity);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_path_is_inside_the_data_dir() {
        let dir = Path::new("/tmp/example");
        assert_eq!(settings_path(dir), PathBuf::from("/tmp/example/config.json"));
    }
}

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

/// Selectable procedural texture-pack style.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextureStyle {
    /// Clean Minecraft-inspired pixel art (default).
    #[default]
    Classic,
    /// Soft hand-painted Hytale-inspired look.
    Painterly,
    /// Detailed, saturated natural textures.
    Lush,
    /// Modern flat, minimal palette.
    Minimal,
}

impl TextureStyle {
    /// All selectable styles, in menu order.
    pub const ALL: [TextureStyle; 4] = [
        TextureStyle::Classic,
        TextureStyle::Painterly,
        TextureStyle::Lush,
        TextureStyle::Minimal,
    ];

    /// Stable settings/CLI name.
    pub fn name(self) -> &'static str {
        match self {
            TextureStyle::Classic => "classic",
            TextureStyle::Painterly => "painterly",
            TextureStyle::Lush => "lush",
            TextureStyle::Minimal => "minimal",
        }
    }

    /// Parses a settings/CLI name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|style| style.name() == name)
    }

    /// Next style in the cycle (menu buttons).
    pub fn next(self) -> Self {
        let index = Self::ALL.into_iter().position(|style| style == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous style in the cycle.
    pub fn previous(self) -> Self {
        let index = Self::ALL.into_iter().position(|style| style == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

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
/// Longest accepted persisted resource-pack path, in bytes.
pub const MAX_TEXTURE_PACK_PATH_BYTES: usize = 4096;

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
    /// Procedural texture-pack style, used when no external resource pack is
    /// selected and as the fallback when one fails to load.
    pub texture_style: TextureStyle,
    /// Optional path to a Minecraft-style resource pack, persisted after a
    /// successful `--texture-pack` launch.
    pub texture_pack: Option<String>,
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
            texture_style: TextureStyle::Classic,
            texture_pack: None,
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

    /// Why the persisted resource-pack path cannot be used, if it cannot.
    fn texture_pack_rejection_reason(&self) -> Option<&'static str> {
        let path = self.texture_pack.as_deref()?;
        if path.is_empty() {
            Some("path is empty")
        } else if path.len() > MAX_TEXTURE_PACK_PATH_BYTES {
            Some("path is longer than 4096 bytes")
        } else if path.chars().any(char::is_control) {
            Some("path contains control characters")
        } else {
            None
        }
    }

    /// Clears an unusable resource-pack path, returning the reason it was rejected.
    fn validate_texture_pack(&mut self) -> Option<&'static str> {
        let reason = self.texture_pack_rejection_reason();
        if reason.is_some() {
            self.texture_pack = None;
        }
        reason
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
            if let Some(reason) = settings.validate_texture_pack() {
                log::warn!(
                    "ignoring texture pack path in {}: {reason}; using the procedural texture style",
                    path.display()
                );
            }
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
    settings.validate_texture_pack();
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
            texture_pack: Some("packs/rotrBLOCKS V88".to_string()),
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir), settings);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn texture_pack_is_optional_and_round_trips() {
        let dir = temp_dir("pack-roundtrip");
        let settings = Settings {
            texture_pack: Some("/tmp/packs/example".to_string()),
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let text = fs::read_to_string(settings_path(&dir)).expect("read");
        assert!(text.contains("\"texture_pack\": \"/tmp/packs/example\""));
        assert_eq!(load_settings(&dir).texture_pack.as_deref(), Some("/tmp/packs/example"));

        let settings = Settings {
            texture_pack: None,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        assert_eq!(load_settings(&dir).texture_pack, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_persisted_texture_pack_is_read_as_none() {
        let dir = temp_dir("pack-empty");
        fs::write(settings_path(&dir), br#"{"texture_pack": ""}"#).expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.texture_pack, None);
        assert_eq!(
            settings.render_distance_chunks,
            Settings::default().render_distance_chunks
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_persisted_texture_pack_is_rejected() {
        let dir = temp_dir("pack-invalid");
        for (label, value) in [
            ("nul", r#""bad\u0000path""#.to_string()),
            ("newline", r#""bad\npath""#.to_string()),
            ("long", format!("\"{}\"", "a".repeat(MAX_TEXTURE_PACK_PATH_BYTES + 1))),
        ] {
            fs::write(
                settings_path(&dir),
                format!(r#"{{"render_distance_chunks": 8, "texture_pack": {value}}}"#),
            )
            .expect("write");
            let settings = load_settings(&dir);
            assert_eq!(settings.texture_pack, None, "{label} path must be rejected");
            // The rest of the settings document still loads.
            assert_eq!(settings.render_distance_chunks, 8, "{label}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn longest_accepted_texture_pack_path_is_kept() {
        let dir = temp_dir("pack-longest");
        let path = "b".repeat(MAX_TEXTURE_PACK_PATH_BYTES);
        fs::write(
            settings_path(&dir),
            serde_json::json!({ "texture_pack": path }).to_string(),
        )
        .expect("write");
        let settings = load_settings(&dir);
        assert_eq!(settings.texture_pack.as_deref(), Some(path.as_str()));
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
    fn texture_style_round_trips_and_cycles() {
        for style in TextureStyle::ALL {
            assert_eq!(TextureStyle::from_name(style.name()), Some(style));
            assert_eq!(style.next().previous(), style);
        }
        assert_eq!(TextureStyle::Classic.next(), TextureStyle::Painterly);
        assert_eq!(TextureStyle::Minimal.next(), TextureStyle::Classic);
        assert_eq!(TextureStyle::from_name("nope"), None);
        let dir = temp_dir("style");
        let settings = Settings {
            texture_style: TextureStyle::Lush,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("save");
        let loaded = load_settings(&dir);
        assert_eq!(loaded.texture_style, TextureStyle::Lush);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_path_is_inside_the_data_dir() {
        let dir = Path::new("/tmp/example");
        assert_eq!(settings_path(dir), PathBuf::from("/tmp/example/config.json"));
    }
}

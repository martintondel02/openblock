//! UI-font discovery, loading and live switching.
//!
//! The renderer bundles Monocraft (SIL OFL 1.1) as the default Minecraft-style
//! UI font and keeps the old 8×8 bitmap font for titles. This module decides
//! whether to replace the embedded UI font at startup:
//!
//! 1. explicit `--ui-font PATH` (persisted for future runs),
//! 2. a readable `settings.ui_font_path`,
//! 3. a font auto-discovered in `<data-dir>/fonts/` (`.ttf`/`.otf`,
//!    preferring names containing `minecraftia`, then `minecraft`),
//! 4. a system font whose file name contains `minecraftia`,
//! 5. otherwise the embedded Monocraft.
//!
//! Unreadable or invalid files never abort startup. Discovery logs the chosen
//! source and any fallback, and the settings screen can reset to Monocraft or
//! reload fonts without restarting.
//!
//! Users can drop a personal `Minecraftia.ttf` into `<data-dir>/fonts/` to get
//! the exact Minecraft look (see `docs/client-ui-font.md`). Minecraftia is not
//! bundled because its licence is personal-use only; Monocraft is bundled under
//! SIL OFL 1.1.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ob_core::config::Settings;
use ob_render::{OffscreenRenderer, Renderer, UiFontError};

use crate::ClientOptions;

/// Maximum UI-font file size read from disk, in bytes.
///
/// A sane TrueType/OpenType font is far smaller; the cap keeps an accidental
/// path to a huge file from allocating unbounded memory before the rasteriser
/// can reject it.
pub const MAX_UI_FONT_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum directory depth explored while looking for a system font.
const MAX_SYSTEM_FONT_DEPTH: usize = 4;

/// Maximum directory entries inspected per directory during system discovery.
const MAX_SYSTEM_FONT_ENTRIES: usize = 4096;

/// Directory whose fonts are considered before system fonts.
pub fn data_fonts_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("fonts")
}

/// Where a resolved UI font came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiFontSource {
    /// The embedded Monocraft font.
    BuiltIn,
    /// Explicit `--ui-font PATH` (normally also persisted in the settings).
    Cli(PathBuf),
    /// The persisted `settings.ui_font_path`.
    Settings(PathBuf),
    /// A `.ttf`/`.otf` discovered in `<data-dir>/fonts/`.
    DataFonts(PathBuf),
    /// A `minecraftia` font found under the platform font directories.
    System(PathBuf),
}

impl UiFontSource {
    /// File path backing this source, or `None` for the embedded font.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::BuiltIn => None,
            Self::Cli(path) | Self::Settings(path) | Self::DataFonts(path) | Self::System(path) => Some(path),
        }
    }

    /// Short human-readable source label used by the settings row.
    pub fn display_label(&self) -> String {
        match self.path() {
            None => "built-in".to_string(),
            Some(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
        }
    }

    /// Longer source description for logs.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Cli(_) => "command line",
            Self::Settings(_) => "saved setting",
            Self::DataFonts(_) => "data folder",
            Self::System(_) => "system fonts",
        }
    }

    /// Whether this source is the embedded font.
    pub fn is_builtin(&self) -> bool {
        matches!(self, Self::BuiltIn)
    }
}

/// The font bytes chosen for this run and the source that supplied them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUiFont {
    /// Where the bytes came from.
    pub source: UiFontSource,
    /// Font file contents, or `None` for the embedded Monocraft atlas.
    pub bytes: Option<Vec<u8>>,
}

impl ResolvedUiFont {
    /// Selects the embedded font.
    pub fn builtin() -> Self {
        Self {
            source: UiFontSource::BuiltIn,
            bytes: None,
        }
    }

    /// The source path, if this is not the embedded font.
    pub fn path(&self) -> Option<&Path> {
        self.source.path()
    }

    /// Short active-name/source label such as `Monocraft (built-in)`.
    pub fn display_label(&self, active_name: &str) -> String {
        format!("{active_name} ({})", self.source.display_label())
    }
}

/// The subset of renderer behaviour this module needs.
///
/// Implemented for both [`Renderer`] and [`OffscreenRenderer`] so windowed,
/// network and screenshot startup all use the same loading and fallback logic.
pub trait UiFontTarget {
    /// Replaces the runtime UI font; `None` restores the embedded Monocraft atlas.
    fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError>;
    /// Display name of the active UI font.
    fn ui_font_name(&self) -> &str;
}

impl UiFontTarget for Renderer {
    fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError> {
        Renderer::set_ui_font(self, font)
    }

    fn ui_font_name(&self) -> &str {
        Renderer::ui_font_name(self)
    }
}

impl UiFontTarget for OffscreenRenderer {
    fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError> {
        OffscreenRenderer::set_ui_font(self, font)
    }

    fn ui_font_name(&self) -> &str {
        OffscreenRenderer::ui_font_name(self)
    }
}

/// Installs `resolved` into `target`, falling back to the embedded Monocraft
/// atlas when the bytes are present but invalid.
///
/// Logs one `ui font: <name> (<path>|built-in)` line. The returned source is
/// the one actually active, so callers can show it in settings/F3.
pub fn install_ui_font<T: UiFontTarget>(target: &mut T, resolved: &ResolvedUiFont) -> UiFontSource {
    if let Some(bytes) = &resolved.bytes {
        match target.set_ui_font(Some(bytes)) {
            Ok(()) => {
                log_ui_font(target, &resolved.source);
                return resolved.source.clone();
            }
            Err(error) => {
                let path = resolved
                    .path()
                    .map_or_else(|| "<unknown>".to_string(), |path| path.display().to_string());
                log::warn!(
                    "could not load UI font {path}: {error}; keeping the built-in {}",
                    ob_render::default_ui_font_name()
                );
            }
        }
    }
    // A broken custom selection falls back to the embedded font rather than
    // retrying the same invalid bytes.
    if let Err(error) = target.set_ui_font(None) {
        log::warn!(
            "could not restore the built-in {} UI font: {error}",
            ob_render::default_ui_font_name()
        );
    }
    log_ui_font(target, &UiFontSource::BuiltIn);
    UiFontSource::BuiltIn
}

fn log_ui_font<T: UiFontTarget>(target: &T, source: &UiFontSource) {
    match source.path() {
        Some(path) => log::info!("ui font: {} ({})", target.ui_font_name(), path.display()),
        None => log::info!("ui font: {} (built-in)", target.ui_font_name()),
    }
}

/// Resolves the UI font for startup using the platform font directories.
///
/// Precedence: `--ui-font`, `--default-ui-font`, the persisted settings path,
/// `<data-dir>/fonts/` discovery, then a system `minecraftia` font. This never
/// fails: unreadable files log a warning and fall back to the embedded font.
pub fn resolve_ui_font(options: &ClientOptions, settings: &Settings, data_dir: &Path) -> ResolvedUiFont {
    let system_dirs = system_font_dirs();
    resolve_ui_font_with_system_dirs(options, settings, data_dir, &system_dirs)
}

/// Resolves the UI font from persisted settings and discovery only.
///
/// Used by the settings screen's Reload action, where CLI-only overrides have
/// already been persisted at startup and should not mask a newly installed
/// `Minecraftia.ttf`.
pub fn resolve_ui_font_for_settings(settings: &Settings, data_dir: &Path) -> ResolvedUiFont {
    let system_dirs = system_font_dirs();
    resolve_discovered(settings, data_dir, &system_dirs)
}

/// Testable implementation with injectable system font directories.
pub(crate) fn resolve_ui_font_with_system_dirs(
    options: &ClientOptions,
    settings: &Settings,
    data_dir: &Path,
    system_dirs: &[PathBuf],
) -> ResolvedUiFont {
    if options.default_ui_font {
        // The flag explicitly clears any persisted/discovered override.
        return ResolvedUiFont::builtin();
    }
    if let Some(path) = &options.ui_font {
        return match read_font_file(path) {
            Ok(bytes) => ResolvedUiFont {
                source: UiFontSource::Cli(path.clone()),
                bytes: Some(bytes),
            },
            Err(error) => {
                log::warn!(
                    "could not read --ui-font {}: {error}; using the built-in {}",
                    path.display(),
                    ob_render::default_ui_font_name()
                );
                ResolvedUiFont::builtin()
            }
        };
    }
    resolve_discovered(settings, data_dir, system_dirs)
}

fn resolve_discovered(settings: &Settings, data_dir: &Path, system_dirs: &[PathBuf]) -> ResolvedUiFont {
    if let Some(path) = settings.ui_font_path.as_deref().filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        match read_font_file(&path) {
            Ok(bytes) => {
                return ResolvedUiFont {
                    source: UiFontSource::Settings(path),
                    bytes: Some(bytes),
                };
            }
            Err(error) => log::warn!(
                "saved UI font {} is unavailable: {error}; searching the data and system font folders",
                path.display()
            ),
        }
    }

    let fonts_dir = data_fonts_dir(data_dir);
    if let Some(path) = discover_data_font(&fonts_dir) {
        match read_font_file(&path) {
            Ok(bytes) => {
                return ResolvedUiFont {
                    source: UiFontSource::DataFonts(path),
                    bytes: Some(bytes),
                };
            }
            Err(error) => log::warn!("could not read discovered UI font {}: {error}", path.display()),
        }
    }

    if let Some(path) = find_system_minecraftia(system_dirs) {
        match read_font_file(&path) {
            Ok(bytes) => {
                return ResolvedUiFont {
                    source: UiFontSource::System(path),
                    bytes: Some(bytes),
                };
            }
            Err(error) => log::warn!("could not read system UI font {}: {error}", path.display()),
        }
    }

    ResolvedUiFont::builtin()
}

/// Reads a font file with a size guard.
fn read_font_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    if metadata.len() > MAX_UI_FONT_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "font file is larger than {} MiB",
                MAX_UI_FONT_FILE_BYTES / (1024 * 1024)
            ),
        ));
    }
    fs::read(path)
}

/// Whether a path has a `.ttf` or `.otf` extension, case-insensitively.
pub fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ttf") || extension.eq_ignore_ascii_case("otf"))
}

/// Picks the preferred font in `<data-dir>/fonts/`.
///
/// Preference: `minecraftia` in the file name, then `minecraft`, then the
/// lexicographically first `.ttf`/`.otf` file. Names are compared
/// case-insensitively so `Minecraftia.TTF` wins.
pub fn discover_data_font(fonts_dir: &Path) -> Option<PathBuf> {
    let mut fonts: Vec<PathBuf> = fs::read_dir(fonts_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_font_file(path))
        .take(MAX_SYSTEM_FONT_ENTRIES)
        .collect();
    fonts.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    let name_matches = |path: &PathBuf, needle: &str| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase().contains(needle))
            .unwrap_or(false)
    };
    fonts
        .iter()
        .find(|path| name_matches(path, "minecraftia"))
        .or_else(|| fonts.iter().find(|path| name_matches(path, "minecraft")))
        .or_else(|| fonts.first())
        .cloned()
}

/// Platform font directories searched for a `minecraftia` file.
pub fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            dirs.push(PathBuf::from(xdg).join("fonts"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            let home = PathBuf::from(home);
            dirs.push(home.join(".local/share/fonts"));
            dirs.push(home.join(".fonts"));
        }
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
        dirs.push(PathBuf::from("/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts"));
    }
    #[cfg(windows)]
    {
        if let Some(windir) = std::env::var_os("WINDIR").filter(|value| !value.is_empty()) {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        }
    }
    let mut seen = Vec::new();
    dirs.retain(|dir| {
        let key = dir.to_string_lossy().to_ascii_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    dirs
}

/// Finds the first `.ttf`/`.otf` whose file name contains `minecraftia`.
///
/// Directories are searched in order and only a few levels deep, so a normal
/// `/usr/share/fonts/truetype/minecraftia/Minecraftia.ttf` is found without
/// walking the entire filesystem.
pub fn find_system_minecraftia(dirs: &[PathBuf]) -> Option<PathBuf> {
    for root in dirs {
        if let Some(found) = find_minecraftia_in(root, 0) {
            return Some(found);
        }
    }
    None
}

fn find_minecraftia_in(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > MAX_SYSTEM_FONT_DEPTH {
        return None;
    }
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .take(MAX_SYSTEM_FONT_ENTRIES)
    {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.is_file() && is_font_file(&path) {
            files.push(path);
        }
    }
    files.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    subdirs.sort_by_key(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    if let Some(found) = files.into_iter().find(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase().contains("minecraftia"))
            .unwrap_or(false)
    }) {
        return Some(found);
    }
    for subdir in subdirs {
        if let Some(found) = find_minecraftia_in(&subdir, depth + 1) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "openblock-ui-font-{}-{label}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir");
        }
        fs::write(path, bytes).expect("font file");
    }

    fn options_with_cli(path: &Path) -> ClientOptions {
        ClientOptions {
            ui_font: Some(path.to_path_buf()),
            ..ClientOptions::default()
        }
    }

    #[test]
    fn cli_font_wins_over_settings_data_and_system_fonts() {
        let dir = temp_dir("cli-precedence");
        let cli = dir.join("cli.ttf");
        let settings_path = dir.join("saved.ttf");
        let data = dir.join("data/fonts/Minecraftia.ttf");
        let system_dir = dir.join("system");
        let system = system_dir.join("Minecraftia.ttf");
        for (path, bytes) in [
            (&cli, b"cli".as_slice()),
            (&settings_path, b"saved".as_slice()),
            (&data, b"data".as_slice()),
            (&system, b"system".as_slice()),
        ] {
            write(path, bytes);
        }
        let settings = Settings {
            ui_font_path: Some(settings_path.to_string_lossy().into_owned()),
            ..Settings::default()
        };
        let resolved =
            resolve_ui_font_with_system_dirs(&options_with_cli(&cli), &settings, &dir.join("data"), &[system_dir]);
        assert_eq!(resolved.source, UiFontSource::Cli(cli.clone()));
        assert_eq!(resolved.bytes.as_deref(), Some(b"cli".as_slice()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saved_setting_wins_over_data_and_system_fonts() {
        let dir = temp_dir("settings-precedence");
        let settings_path = dir.join("saved.otf");
        let data = dir.join("data/fonts/Minecraftia.ttf");
        let system_dir = dir.join("system");
        write(&settings_path, b"saved");
        write(&data, b"data");
        write(&system_dir.join("Minecraftia.ttf"), b"system");
        let settings = Settings {
            ui_font_path: Some(settings_path.to_string_lossy().into_owned()),
            ..Settings::default()
        };
        let resolved =
            resolve_ui_font_with_system_dirs(&ClientOptions::default(), &settings, &dir.join("data"), &[system_dir]);
        assert_eq!(resolved.source, UiFontSource::Settings(settings_path));
        assert_eq!(resolved.bytes.as_deref(), Some(b"saved".as_slice()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn data_fonts_prefer_minecraftia_then_minecraft_then_sorted() {
        let dir = temp_dir("data-preference");
        let fonts = dir.join("data/fonts");
        write(&fonts.join("zzz.ttf"), b"z");
        write(&fonts.join("my-minecraft-copy.TTF"), b"m");
        write(&fonts.join("Minecraftia.OTF"), b"m");
        write(&fonts.join("notes.txt"), b"not a font");
        assert_eq!(discover_data_font(&fonts), Some(fonts.join("Minecraftia.OTF")));

        fs::remove_file(fonts.join("Minecraftia.OTF")).expect("remove");
        assert_eq!(discover_data_font(&fonts), Some(fonts.join("my-minecraft-copy.TTF")));

        fs::remove_file(fonts.join("my-minecraft-copy.TTF")).expect("remove");
        assert_eq!(discover_data_font(&fonts), Some(fonts.join("zzz.ttf")));
        assert_eq!(discover_data_font(&dir.join("missing")), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn system_search_recurses_shallowly_for_minecraftia() {
        let dir = temp_dir("system-search");
        let root = dir.join("fonts");
        write(&root.join("Other.ttf"), b"other");
        let nested = root.join("truetype/minecraftia");
        write(&nested.join("Minecraftia.ttf"), b"minecraftia");
        assert_eq!(
            find_system_minecraftia(&[dir.join("none"), root.clone()]),
            Some(nested.join("Minecraftia.ttf"))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn system_fonts_are_used_after_data_fonts() {
        let dir = temp_dir("system-fallback");
        let data = dir.join("data");
        let system_dir = dir.join("system");
        write(&system_dir.join("Minecraftia.ttf"), b"system");
        let resolved =
            resolve_ui_font_with_system_dirs(&ClientOptions::default(), &Settings::default(), &data, &[system_dir]);
        assert!(matches!(resolved.source, UiFontSource::System(_)));
        assert_eq!(resolved.bytes.as_deref(), Some(b"system".as_slice()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_paths_and_unreadable_files_fall_back_without_failing() {
        let dir = temp_dir("font-fallback");
        let missing = dir.join("missing.ttf");
        let data = dir.join("data");
        write(&data.join("fonts/fallback.ttf"), b"fallback");
        // A stale saved path falls through to data-font discovery.
        let settings = Settings {
            ui_font_path: Some(missing.to_string_lossy().into_owned()),
            ..Settings::default()
        };
        let resolved = resolve_ui_font_with_system_dirs(&ClientOptions::default(), &settings, &data, &[]);
        assert!(matches!(resolved.source, UiFontSource::DataFonts(_)));
        assert_eq!(resolved.bytes.as_deref(), Some(b"fallback".as_slice()));

        // An unreadable explicit CLI path falls straight back to the built-in
        // font; it never silently reuses a stale saved setting.
        let resolved = resolve_ui_font_with_system_dirs(&options_with_cli(&missing), &settings, &data, &[]);
        assert_eq!(resolved.source, UiFontSource::BuiltIn);
        assert!(resolved.bytes.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_ui_font_flag_clears_every_override() {
        let dir = temp_dir("default-flag");
        let path = dir.join("Minecraftia.ttf");
        write(&path, b"font");
        let settings = Settings {
            ui_font_path: Some(path.to_string_lossy().into_owned()),
            ..Settings::default()
        };
        let options = ClientOptions {
            default_ui_font: true,
            ..ClientOptions::default()
        };
        let resolved = resolve_ui_font_with_system_dirs(&options, &settings, &dir, std::slice::from_ref(&dir));
        assert_eq!(resolved.source, UiFontSource::BuiltIn);
        assert!(resolved.bytes.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn font_file_extension_check_is_case_insensitive() {
        assert!(is_font_file(Path::new("Minecraftia.ttf")));
        assert!(is_font_file(Path::new("Minecraftia.TTF")));
        assert!(is_font_file(Path::new("font.otf")));
        assert!(is_font_file(Path::new("font.OTF")));
        assert!(!is_font_file(Path::new("font.ttc")));
        assert!(!is_font_file(Path::new("font")));
    }

    struct FakeTarget {
        bytes: Option<Vec<u8>>,
    }

    impl UiFontTarget for FakeTarget {
        fn set_ui_font(&mut self, font: Option<&[u8]>) -> Result<(), UiFontError> {
            match font {
                Some(bytes) if bytes == b"bad" => Err(UiFontError::InvalidFont("bad test font".to_string())),
                Some(bytes) => {
                    self.bytes = Some(bytes.to_vec());
                    Ok(())
                }
                None => {
                    self.bytes = None;
                    Ok(())
                }
            }
        }

        fn ui_font_name(&self) -> &str {
            if self.bytes.is_some() { "custom" } else { "Monocraft" }
        }
    }

    #[test]
    fn install_reports_the_active_source_and_falls_back_on_invalid_fonts() {
        let mut target = FakeTarget { bytes: None };
        let resolved = ResolvedUiFont {
            source: UiFontSource::Cli(PathBuf::from("/tmp/MyFont.ttf")),
            bytes: Some(b"good".to_vec()),
        };
        let source = install_ui_font(&mut target, &resolved);
        assert_eq!(source, UiFontSource::Cli(PathBuf::from("/tmp/MyFont.ttf")));
        assert_eq!(target.bytes.as_deref(), Some(b"good".as_slice()));
        assert_eq!(source.display_label(), "MyFont.ttf");
        assert_eq!(resolved.display_label(target.ui_font_name()), "custom (MyFont.ttf)");

        let invalid = ResolvedUiFont {
            source: UiFontSource::Settings(PathBuf::from("/tmp/Broken.ttf")),
            bytes: Some(b"bad".to_vec()),
        };
        let source = install_ui_font(&mut target, &invalid);
        assert_eq!(source, UiFontSource::BuiltIn);
        assert!(target.bytes.is_none());
        assert_eq!(target.ui_font_name(), "Monocraft");
    }
}
